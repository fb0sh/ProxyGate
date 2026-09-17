//! Helpers shared by the integration tests.
//!
//! Everything here is a hand rolled TCP server: the tests must not depend on
//! external programs or on the network, and a deliberately dumb upstream proxy
//! is the easiest way to observe exactly what ProxyGate sends.

#![allow(dead_code)]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Spawns a TCP server, returning the address it listens on.
pub async fn spawn_server<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("local address");
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            tokio::spawn(async move { handler(stream).await });
        }
    });
    address
}

/// An address nothing listens on (used to simulate a dead upstream).
pub async fn dead_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    drop(listener);
    address
}

/// Reads until the blank line that ends an HTTP head.
pub async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buffer).into_owned());
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
}

/// Reads until the accumulated text contains `needle`, or the deadline passes.
pub async fn read_until_contains(
    stream: &mut TcpStream,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                return String::from_utf8_lossy(&buffer).into_owned();
            }
            Ok(Ok(read)) => {
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer);
                if text.contains(needle) {
                    return text.into_owned();
                }
            }
        }
    }
}

/// What a [`FakeProxy`] does with the requests it receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyBehaviour {
    /// Answer CONNECT with 200 and echo tunnel bytes, answer plain requests.
    Serve,
    /// Refuse CONNECT with 407.
    Refuse,
}

/// A hand rolled HTTP proxy used as an upstream.
pub struct FakeProxy {
    pub address: SocketAddr,
    /// Every request head the fake proxy saw, in order.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl FakeProxy {
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }

    pub fn first_request(&self) -> String {
        self.requests
            .lock()
            .expect("requests lock")
            .first()
            .cloned()
            .unwrap_or_default()
    }
}

/// Starts a fake HTTP proxy that answers plain requests with a canned body and
/// echoes everything sent through a CONNECT tunnel.
pub async fn fake_http_proxy(behaviour: ProxyBehaviour) -> FakeProxy {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();

    let address = spawn_server(move |mut stream| {
        let seen = seen.clone();
        async move {
            let Some(head) = read_head(&mut stream).await else {
                return;
            };
            let first_line = head.lines().next().unwrap_or_default().to_string();
            seen.lock().expect("requests lock").push(head.clone());

            if first_line.starts_with("CONNECT ") {
                match behaviour {
                    ProxyBehaviour::Refuse => {
                        let _ = stream
                            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                            .await;
                    }
                    ProxyBehaviour::Serve => {
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                            .await;
                        echo_loop(&mut stream).await;
                    }
                }
                return;
            }

            let body = "hello from the upstream proxy\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    })
    .await;

    FakeProxy { address, requests }
}

/// Prefixes everything it receives with `echo:` and sends it back.
async fn echo_loop(stream: &mut TcpStream) {
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let mut out = b"echo:".to_vec();
                out.extend_from_slice(&chunk[..read]);
                if stream.write_all(&out).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// A minimal SOCKS5 proxy: optional username/password auth, CONNECT only,
/// echoes the tunnel.
pub struct FakeSocks5 {
    pub address: SocketAddr,
    /// `(address type, target)` pairs requested by clients, in order.
    pub targets: Arc<Mutex<Vec<(u8, String)>>>,
}

impl FakeSocks5 {
    pub fn targets(&self) -> Vec<(u8, String)> {
        self.targets.lock().expect("targets lock").clone()
    }
}

/// Starts a fake SOCKS5 proxy.
///
/// `credentials` enables the username/password method (RFC 1929).
pub async fn fake_socks5_proxy(credentials: Option<(String, String)>) -> FakeSocks5 {
    let targets = Arc::new(Mutex::new(Vec::new()));
    let seen = targets.clone();

    let address = spawn_server(move |mut stream| {
        let seen = seen.clone();
        let credentials = credentials.clone();
        async move {
            // Greeting: VER NMETHODS METHODS...
            let mut header = [0u8; 2];
            if stream.read_exact(&mut header).await.is_err() {
                return;
            }
            let mut methods = vec![0u8; header[1] as usize];
            if stream.read_exact(&mut methods).await.is_err() {
                return;
            }

            match &credentials {
                Some(_) => {
                    if !methods.contains(&0x02) {
                        let _ = stream.write_all(&[0x05, 0xff]).await;
                        return;
                    }
                    let _ = stream.write_all(&[0x05, 0x02]).await;
                    if !authenticate(&mut stream, credentials.as_ref().expect("credentials")).await
                    {
                        return;
                    }
                }
                None => {
                    let _ = stream.write_all(&[0x05, 0x00]).await;
                }
            }

            // Request: VER CMD RSV ATYP DST.ADDR DST.PORT
            let mut request = [0u8; 4];
            if stream.read_exact(&mut request).await.is_err() {
                return;
            }
            if request[1] != 0x01 {
                // Only CONNECT is supported by the fake proxy.
                let _ = stream
                    .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                return;
            }

            let target = match request[3] {
                0x01 => {
                    let mut raw = [0u8; 4];
                    if stream.read_exact(&mut raw).await.is_err() {
                        return;
                    }
                    std::net::Ipv4Addr::from(raw).to_string()
                }
                0x04 => {
                    let mut raw = [0u8; 16];
                    if stream.read_exact(&mut raw).await.is_err() {
                        return;
                    }
                    std::net::Ipv6Addr::from(raw).to_string()
                }
                0x03 => {
                    let mut length = [0u8; 1];
                    if stream.read_exact(&mut length).await.is_err() {
                        return;
                    }
                    let mut raw = vec![0u8; length[0] as usize];
                    if stream.read_exact(&mut raw).await.is_err() {
                        return;
                    }
                    String::from_utf8_lossy(&raw).into_owned()
                }
                _ => return,
            };
            let mut port = [0u8; 2];
            if stream.read_exact(&mut port).await.is_err() {
                return;
            }
            let port = u16::from_be_bytes(port);
            seen.lock()
                .expect("targets lock")
                .push((request[3], format!("{target}:{port}")));

            // Success, bound address 0.0.0.0:0.
            let _ = stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            echo_loop(&mut stream).await;
        }
    })
    .await;

    FakeSocks5 { address, targets }
}

async fn authenticate(stream: &mut TcpStream, credentials: &(String, String)) -> bool {
    let mut version = [0u8; 1];
    if stream.read_exact(&mut version).await.is_err() {
        return false;
    }
    let mut user_length = [0u8; 1];
    if stream.read_exact(&mut user_length).await.is_err() {
        return false;
    }
    let mut user = vec![0u8; user_length[0] as usize];
    if stream.read_exact(&mut user).await.is_err() {
        return false;
    }
    let mut password_length = [0u8; 1];
    if stream.read_exact(&mut password_length).await.is_err() {
        return false;
    }
    let mut password = vec![0u8; password_length[0] as usize];
    if stream.read_exact(&mut password).await.is_err() {
        return false;
    }

    let ok = user == credentials.0.as_bytes() && password == credentials.1.as_bytes();
    let _ = stream
        .write_all(if ok { &[0x01, 0x00] } else { &[0x01, 0x01] })
        .await;
    ok
}

/// A one-request HTTP server that replies with `body`.
pub async fn fake_http_server(body: &'static str) -> SocketAddr {
    spawn_server(move |mut stream| async move {
        if read_head(&mut stream).await.is_none() {
            return;
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    })
    .await
}
