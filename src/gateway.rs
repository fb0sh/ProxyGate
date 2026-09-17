//! The HTTP proxy gateway.
//!
//! Clients talk plain HTTP proxy to ProxyGate and never learn which upstream —
//! or which upstream credentials — served the request:
//!
//! ```text
//! Client -> ProxyGate -> Selector -> http://user:pass@upstream:3128 -> Target
//! ```
//!
//! * `CONNECT` requests become a byte tunnel picked once for the whole tunnel
//!   (the upstream is chosen, dialled and handshaked before the 200 is sent, so
//!   a broken upstream can be retried transparently).
//! * Plain HTTP requests are forwarded with `reqwest`, which already knows how
//!   to speak HTTP-proxy and SOCKS5 (including credentials) upstream.
//!
//! Client authentication (`--auth user:pass`) is completely independent from
//! upstream authentication.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::body::Body;
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, BodyStream};
use hyper::body::Incoming;
use hyper::header::{HeaderName, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_socks::tcp::Socks5Stream;

use crate::checker::{ClientMode, ProxyClients};
use crate::error::{Error, Result};
use crate::model::{self, Proxy, ProxyScheme};
use crate::pool::{ProxyPool, Selection};
use crate::selector::Strategy;

/// Supertrait alias for a tunnel endpoint.
///
/// A trait object cannot combine `AsyncRead` and `AsyncWrite` directly, so the
/// two are bundled here and the tunnel works with `Box<dyn ProxyStream>`.
pub trait ProxyStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> ProxyStream for T {}

/// A stream whose concrete type depends on the upstream scheme.
pub type BoxedStream = Pin<Box<dyn ProxyStream>>;

/// What the proxy service returns to hyper.
pub type ProxyResponse = Response<Body>;
type ProxyResult = std::result::Result<ProxyResponse, Infallible>;

/// Everything the gateway needs that is not the pool.
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub strategy: Strategy,
    pub reuse_after: Duration,
    /// Extra attempts after the first upstream fails.
    pub retries: u32,
    pub connect_timeout: Duration,
    /// Consecutive failures after which an upstream is considered dead.
    pub max_failures: u32,
    /// Optional `(user, password)` required from clients.
    pub credentials: Option<(String, String)>,
}

impl Default for GatewayOptions {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            reuse_after: Duration::from_secs(1800),
            retries: 2,
            connect_timeout: Duration::from_secs(10),
            max_failures: 3,
            credentials: None,
        }
    }
}

/// The HTTP proxy gateway.
#[derive(Debug, Clone)]
pub struct Gateway {
    pool: Arc<ProxyPool>,
    clients: Arc<ProxyClients>,
    options: GatewayOptions,
}

impl Gateway {
    pub fn new(pool: Arc<ProxyPool>, clients: Arc<ProxyClients>, options: GatewayOptions) -> Self {
        Self {
            pool,
            clients,
            options,
        }
    }

    /// Accepts connections until `shutdown` resolves.
    pub async fn serve<S>(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: S,
    ) -> std::io::Result<()>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let local = listener.local_addr().ok();
        tracing::info!(address = ?local, "HTTP proxy gateway listening");
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("HTTP proxy gateway shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            // Per-connection accept errors (EMFILE, ECONNABORTED)
                            // must not kill the listener.
                            tracing::warn!(error = %error, "accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let gateway = self.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request| {
                            let gateway = gateway.clone();
                            async move { gateway.handle(request, peer).await }
                        });
                        let connection = hyper::server::conn::http1::Builder::new()
                            .keep_alive(true)
                            .serve_connection(TokioIo::new(stream), service)
                            .with_upgrades();
                        if let Err(error) = connection.await {
                            tracing::debug!(client = %peer, error = %error, "client connection closed");
                        }
                    });
                }
            }
        }
    }

    /// Entry point for one client request.
    async fn handle(&self, request: Request<Incoming>, peer: std::net::SocketAddr) -> ProxyResult {
        if let Some(rejection) = self.rejection(&request) {
            tracing::debug!(client = %peer, "rejected unauthenticated client");
            return Ok(rejection);
        }

        if request.method() == Method::CONNECT {
            Ok(self.handle_connect(request).await)
        } else {
            Ok(self.handle_http(request, peer).await)
        }
    }

    /// Enforces gateway credentials, if configured.
    ///
    /// Returns the `407` to send back when the client is not allowed in, or
    /// `None` when the request may proceed.
    fn rejection(&self, request: &Request<Incoming>) -> Option<ProxyResponse> {
        let Some((user, password)) = &self.options.credentials else {
            return None;
        };

        let provided = request
            .headers()
            .get(PROXY_AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(decode_basic_credentials);

        match provided {
            Some((got_user, got_password))
                if constant_time_eq(got_user.as_bytes(), user.as_bytes())
                    && constant_time_eq(got_password.as_bytes(), password.as_bytes()) =>
            {
                None
            }
            _ => Some(
                Response::builder()
                    .status(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
                    .header(PROXY_AUTHENTICATE, "Basic realm=\"proxygate\"")
                    .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(Body::from("proxygate: proxy authentication required\n"))
                    .unwrap_or_else(|_| Response::new(Body::empty())),
            ),
        }
    }

    /// Picks an upstream using the configured strategy.
    fn select(&self) -> Option<Selection> {
        let selection = self.pool.select(
            self.options.strategy,
            self.options.reuse_after,
            SystemTime::now(),
        )?;
        tracing::debug!(
            upstream = %selection.proxy.to_masked_string(),
            round = selection.round,
            reset_round = selection.reset_round,
            candidates = selection.candidates,
            "selected upstream"
        );
        Some(selection)
    }

    async fn handle_connect(&self, mut request: Request<Incoming>) -> ProxyResponse {
        let target = connect_target(&request);
        let Some((host, port)) = target else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "CONNECT requires an authority-form target such as example.com:443",
            );
        };

        // The client's connection is upgraded only after we answer 200, so the
        // upgrade future has to be captured before the request is consumed.
        let on_upgrade = hyper::upgrade::on(&mut request);

        let attempts = self.options.retries.saturating_add(1);
        let mut last_error = String::from("no healthy upstream proxy available");

        for attempt in 0..attempts {
            let Some(selection) = self.select() else {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no healthy upstream proxy available",
                );
            };
            let upstream = selection.proxy.clone();

            match self.open_tunnel(&upstream, &host, port).await {
                Ok((stream, leftover)) => {
                    let pool = self.pool.clone();
                    let id = upstream.id.clone();
                    let latency = upstream.latency;
                    let peer_target = format!("{host}:{port}");
                    tokio::spawn(async move {
                        match on_upgrade.await {
                            Ok(upgraded) => {
                                let mut client: BoxedStream = Box::pin(TokioIo::new(upgraded));
                                match relay(&mut client, stream, &leftover).await {
                                    Ok((from_client, from_upstream)) => {
                                        tracing::debug!(target = %peer_target, bytes_up = from_client, bytes_down = from_upstream, "tunnel closed");
                                        pool.record_success(&id, latency, SystemTime::now());
                                    }
                                    Err(error) => {
                                        tracing::debug!(target = %peer_target, error = %error, "tunnel ended with an error");
                                        pool.record_success(&id, latency, SystemTime::now());
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::debug!(target = %peer_target, error = %error, "client upgrade failed");
                            }
                        }
                    });

                    tracing::debug!(
                        upstream = %upstream.to_masked_string(),
                        target = %format!("{host}:{port}"),
                        attempt = attempt + 1,
                        "tunnel established"
                    );
                    return Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::empty())
                        .unwrap_or_else(|_| Response::new(Body::empty()));
                }
                Err(error) => {
                    last_error = error.to_string();
                    let alive = self
                        .pool
                        .record_failure(&upstream.id, self.options.max_failures);
                    tracing::warn!(
                        upstream = %upstream.to_masked_string(),
                        target = %format!("{host}:{port}"),
                        attempt = attempt + 1,
                        alive = ?alive,
                        error = %error,
                        "upstream tunnel failed"
                    );
                }
            }
        }

        error_response(
            StatusCode::BAD_GATEWAY,
            &format!("every upstream failed: {last_error}"),
        )
    }

    async fn handle_http(
        &self,
        request: Request<Incoming>,
        peer: std::net::SocketAddr,
    ) -> ProxyResponse {
        let method = request.method().clone();
        let uri = request.uri().clone();

        if uri.scheme().is_none() || uri.authority().is_none() {
            return error_response(
                StatusCode::BAD_REQUEST,
                "expected an absolute request URI: configure this endpoint as an HTTP proxy",
            );
        }

        // Retrying means sending the body twice, so it is only safe for GET and
        // HEAD — and only after buffering it.
        let retryable = matches!(method, Method::GET | Method::HEAD);
        let attempts = if retryable {
            self.options.retries.saturating_add(1)
        } else {
            1
        };

        let (parts, incoming) = request.into_parts();
        let (buffered, mut incoming) = if retryable {
            match incoming.collect().await {
                Ok(collected) => (Some(collected.to_bytes()), None),
                Err(error) => {
                    tracing::debug!(client = %peer, error = %error, "cannot read request body");
                    return error_response(StatusCode::BAD_REQUEST, "cannot read the request body");
                }
            }
        } else {
            (None, Some(incoming))
        };

        let mut last_error = String::from("no healthy upstream proxy available");
        for attempt in 0..attempts {
            let Some(selection) = self.select() else {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no healthy upstream proxy available",
                );
            };
            let upstream = selection.proxy.clone();

            let client = match self.clients.get(&upstream, ClientMode::Forward) {
                Ok(client) => client,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };

            let body = match &buffered {
                Some(bytes) => reqwest::Body::from(bytes.clone()),
                None => {
                    let incoming = incoming.take().expect("streamed bodies are never retried");
                    reqwest::Body::wrap_stream(
                        BodyStream::new(incoming)
                            .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) }),
                    )
                }
            };

            let mut outgoing = client.request(parts.method.clone(), uri.to_string());
            for (name, value) in parts.headers.iter() {
                // Never leak client-side proxy credentials upstream, and drop
                // hop-by-hop headers that only apply to this connection.
                if is_hop_by_hop(name) {
                    continue;
                }
                outgoing = outgoing.header(name.clone(), value.clone());
            }

            let started = Instant::now();
            match outgoing.body(body).send().await {
                Ok(response) => {
                    let latency = started.elapsed();
                    self.pool
                        .record_success(&upstream.id, Some(latency), SystemTime::now());
                    tracing::debug!(
                        upstream = %upstream.to_masked_string(),
                        method = %method,
                        url = %uri,
                        status = %response.status(),
                        elapsed_ms = latency.as_millis() as u64,
                        attempt = attempt + 1,
                        "forwarded"
                    );
                    return build_response(response);
                }
                Err(error) => {
                    last_error = crate::checker::describe_error(&error);
                    let retry = retryable
                        && (error.is_connect() || error.is_timeout())
                        && attempt + 1 < attempts;
                    if retry {
                        tracing::warn!(
                            upstream = %upstream.to_masked_string(),
                            url = %uri,
                            attempt = attempt + 1,
                            error = %last_error,
                            "retrying on another upstream"
                        );
                    } else {
                        let alive = self
                            .pool
                            .record_failure(&upstream.id, self.options.max_failures);
                        tracing::warn!(
                            upstream = %upstream.to_masked_string(),
                            url = %uri,
                            alive = ?alive,
                            error = %last_error,
                            "forwarding failed"
                        );
                        break;
                    }
                }
            }
        }

        error_response(
            StatusCode::BAD_GATEWAY,
            &format!("every upstream failed: {last_error}"),
        )
    }

    /// Dials the upstream and, for HTTP proxies, performs the CONNECT handshake.
    async fn open_tunnel(
        &self,
        upstream: &Proxy,
        host: &str,
        port: u16,
    ) -> Result<(BoxedStream, Vec<u8>)> {
        let handshake = self.open_tunnel_inner(upstream, host, port);
        match tokio::time::timeout(self.options.connect_timeout, handshake).await {
            Ok(result) => result,
            Err(_) => Err(Error::other(format!(
                "connecting through {} timed out after {:?}",
                upstream.to_masked_string(),
                self.options.connect_timeout
            ))),
        }
    }

    async fn open_tunnel_inner(
        &self,
        upstream: &Proxy,
        host: &str,
        port: u16,
    ) -> Result<(BoxedStream, Vec<u8>)> {
        match upstream.scheme() {
            ProxyScheme::Http => {
                let mut stream = TcpStream::connect(upstream.authority()).await?;

                let mut request =
                    format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
                if let Some(user) = upstream.username() {
                    let password = upstream.password().unwrap_or_default();
                    let token = model::base64_encode(format!("{user}:{password}").as_bytes());
                    request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
                }
                request.push_str("Proxy-Connection: keep-alive\r\n\r\n");
                stream.write_all(request.as_bytes()).await?;
                stream.flush().await?;

                let (head, leftover) = read_head(&mut stream, 16 * 1024).await?;
                let status = parse_status_code(&head)?;
                if !(200..300).contains(&status) {
                    return Err(Error::other(format!(
                        "upstream answered {status} to CONNECT"
                    )));
                }
                // The upstream side is a plain tokio stream; `TokioIo` is only
                // needed for the client side of the tunnel.
                Ok((Box::pin(stream) as BoxedStream, leftover))
            }
            ProxyScheme::Socks5 | ProxyScheme::Socks5h => {
                let proxy_address = (upstream.host(), upstream.port());
                let user = upstream.username();
                let password = upstream.password().unwrap_or_default();

                let stream = if upstream.scheme() == ProxyScheme::Socks5h {
                    // Remote DNS: hand the host name to the proxy.
                    socks5_connect(proxy_address, (host, port), user.as_deref(), &password).await?
                } else {
                    // Local DNS: resolve here, then hand over the address.
                    let mut resolved = tokio::net::lookup_host((host, port)).await?;
                    let address = resolved
                        .next()
                        .ok_or_else(|| Error::other(format!("cannot resolve {host}")))?;
                    socks5_connect(proxy_address, address, user.as_deref(), &password).await?
                };

                Ok((Box::pin(stream) as BoxedStream, Vec::new()))
            }
        }
    }
}

/// Small helper so the two SOCKS5 call shapes stay in one place.
async fn socks5_connect<'t, T>(
    proxy_address: (&str, u16),
    target: T,
    user: Option<&str>,
    password: &str,
) -> Result<Socks5Stream<TcpStream>>
where
    T: tokio_socks::IntoTargetAddr<'t>,
{
    let stream = match user {
        Some(user) => {
            Socks5Stream::connect_with_password(proxy_address, target, user, password).await
        }
        None => Socks5Stream::connect(proxy_address, target).await,
    };
    stream.map_err(|error| Error::other(format!("socks5 handshake failed: {error}")))
}

/// Bidirectional copy, flushing any bytes that were already read from the
/// upstream while parsing its CONNECT response.
async fn relay(
    client: &mut BoxedStream,
    mut upstream: BoxedStream,
    leftover: &[u8],
) -> std::io::Result<(u64, u64)> {
    if !leftover.is_empty() {
        client.write_all(leftover).await?;
        client.flush().await?;
    }
    tokio::io::copy_bidirectional(client, &mut upstream).await
}

/// Reads up to the end of an HTTP head, returning it with any extra bytes.
async fn read_head<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = find_head_end(&buffer) {
            let leftover = buffer.split_off(end);
            return Ok((buffer, leftover));
        }
        if buffer.len() >= limit {
            return Err(Error::other(format!(
                "upstream response head exceeded {limit} bytes"
            )));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(Error::other(
                "upstream closed the connection before answering CONNECT",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Position just past the blank line that ends a head.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

/// Status code of `HTTP/1.1 200 Connection established`.
fn parse_status_code(head: &[u8]) -> Result<u16> {
    let text = String::from_utf8_lossy(head);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::other("upstream sent a malformed status line"))
}

/// `host:port` for a CONNECT request (authority-form, port defaults to 443).
fn connect_target(request: &Request<Incoming>) -> Option<(String, u16)> {
    let authority = request
        .uri()
        .authority()
        .map(|authority| authority.to_string())?;
    split_host_port(&authority, 443)
}

/// Splits `host:port`, tolerating bracketed IPv6 and a missing port.
pub fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port = port.parse::<u16>().ok()?;
            Some((host.to_string(), port))
        }
        Some(_) => None,
        None if !authority.is_empty() => Some((authority.to_string(), default_port)),
        None => None,
    }
}

/// Headers that apply to one hop only and must not be forwarded.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn build_response(response: reqwest::Response) -> ProxyResponse {
    let status = response.status();
    let headers = response.headers().clone();
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    match builder.body(Body::from_stream(response.bytes_stream())) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "cannot rebuild the upstream response");
            error_response(StatusCode::BAD_GATEWAY, "malformed upstream response")
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> ProxyResponse {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("proxygate: {message}\n")))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Extracts `user:password` from a `Basic` proxy authorization header.
pub fn decode_basic_credentials(value: &str) -> Option<(String, String)> {
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = model::base64_decode(encoded.trim())?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// Comparison that does not short-circuit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("example.com:443", 443),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            split_host_port("example.com", 443),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:8443", 443),
            Some(("2001:db8::1".into(), 8443))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]", 443),
            Some(("2001:db8::1".into(), 443))
        );
        assert_eq!(split_host_port("example.com:notaport", 443), None);
        assert_eq!(split_host_port("", 443), None);

        // Hostile authorities must be rejected, never panic.
        for authority in [
            "",
            ":",
            "::",
            ":8080",
            "host:",
            "[]",
            "[::1]",
            "a:b:c",
            "\u{1f600}:1",
        ] {
            let _ = split_host_port(authority, 443);
        }
        assert_eq!(split_host_port("[::1]:", 8443), Some(("::1".into(), 8443)));
        assert_eq!(split_host_port("::", 443), None);
    }

    #[test]
    fn decodes_proxy_authorization() {
        let header = format!("Basic {}", model::base64_encode(b"admin:secret"));
        assert_eq!(
            decode_basic_credentials(&header),
            Some(("admin".to_string(), "secret".to_string()))
        );
        assert_eq!(decode_basic_credentials("Bearer token"), None);
        assert_eq!(decode_basic_credentials("Basic !!!"), None);
    }

    #[test]
    fn detects_head_boundaries() {
        assert_eq!(find_head_end(b"HTTP/1.1 200 OK\r\n\r\n"), Some(19));
        assert_eq!(find_head_end(b"HTTP/1.1 200 OK\r\n"), None);
        let parsed =
            parse_status_code(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").unwrap();
        assert_eq!(parsed, 407);
    }

    #[test]
    fn constant_time_comparison_is_length_aware() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
