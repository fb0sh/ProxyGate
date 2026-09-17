//! End to end tests for the HTTP proxy gateway.
//!
//! Each test starts a fake upstream (HTTP or SOCKS5, written by hand), puts it
//! in a pool, runs a real gateway and talks to it with a raw TCP client — so the
//! assertions are about bytes on the wire, not about internal state.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use proxygate::checker::ProxyClients;
use proxygate::gateway::{Gateway, GatewayOptions};
use proxygate::model::{self, normalize};
use proxygate::pool::{HealthUpdate, ProxyPool};
use proxygate::selector::Strategy;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use common::{
    ProxyBehaviour, dead_address, fake_http_proxy, fake_socks5_proxy, read_until_contains,
};

const WAIT: Duration = Duration::from_secs(5);

/// Builds a pool whose proxies are already marked alive.
fn pool_from(upstreams: &[(String, Duration)]) -> Arc<ProxyPool> {
    let pool = ProxyPool::new();
    let mut updates = Vec::new();
    for (raw, latency) in upstreams {
        let (id, _) = pool.insert(normalize(raw).expect("valid upstream url"));
        updates.push((
            id,
            HealthUpdate {
                alive: true,
                latency: Some(*latency),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        ));
    }
    pool.apply_health_pass(&updates, 3);
    Arc::new(pool)
}

fn http_proxy(upstreams: &[SocketAddr]) -> Vec<(String, Duration)> {
    upstreams
        .iter()
        .enumerate()
        .map(|(index, address)| {
            (
                format!("http://{address}"),
                Duration::from_millis(10 * (index as u64 + 1)),
            )
        })
        .collect()
}

/// Starts a gateway on an ephemeral port and returns its address.
async fn start_gateway(pool: Arc<ProxyPool>, options: GatewayOptions) -> SocketAddr {
    let clients = Arc::new(ProxyClients::new(
        Duration::from_secs(5),
        Duration::from_secs(5),
    ));
    let gateway = Arc::new(Gateway::new(pool, clients, options));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    tokio::spawn(async move {
        let _ = gateway.serve(listener, std::future::pending::<()>()).await;
    });
    address
}

#[tokio::test]
async fn connect_tunnels_through_an_http_upstream() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "expected a 200, got: {head}"
    );

    // The tunnel must be byte transparent in both directions.
    client
        .write_all(b"ping\n")
        .await
        .expect("write into tunnel");
    let echoed = read_until_contains(&mut client, "echo:ping", WAIT).await;
    assert!(
        echoed.contains("echo:ping"),
        "tunnel did not echo: {echoed}"
    );

    let request = upstream.first_request();
    assert!(
        request.starts_with("CONNECT example.test:443 HTTP/1.1"),
        "unexpected upstream request: {request}"
    );
}

#[tokio::test]
async fn forwards_plain_http_in_absolute_form() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(
            b"GET http://example.test/hello HTTP/1.1\r\nHost: example.test\r\nProxy-Connection: keep-alive\r\n\r\n",
        )
        .await
        .expect("send GET");

    let response = read_until_contains(&mut client, "hello from the upstream proxy", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );

    let request = upstream.first_request();
    assert!(
        request.starts_with("GET http://example.test/hello HTTP/1.1"),
        "the absolute request target must be preserved: {request}"
    );
}

#[tokio::test]
async fn rejects_a_request_without_an_absolute_uri() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"GET /just-a-path HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .await
        .expect("send GET");

    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "unexpected response: {response}"
    );
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn requires_client_credentials_and_never_leaks_them_upstream() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 0,
            credentials: Some(("admin".to_string(), "secret".to_string())),
            ..GatewayOptions::default()
        },
    )
    .await;

    // No credentials: 407 with a challenge, and nothing reaches the upstream.
    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .await
        .expect("send GET");
    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 407"),
        "unexpected response: {response}"
    );
    assert!(
        response
            .to_lowercase()
            .contains("proxy-authenticate: basic"),
        "missing challenge: {response}"
    );
    assert_eq!(
        upstream.request_count(),
        0,
        "an unauthenticated request must not reach the upstream"
    );

    // Wrong password: still 407.
    let token = model::base64_encode(b"admin:wrong");
    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(
            format!(
                "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\nProxy-Authorization: Basic {token}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send GET");
    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 407"),
        "unexpected response: {response}"
    );

    // Correct credentials: forwarded, and the header is stripped on the way out.
    let token = model::base64_encode(b"admin:secret");
    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(
            format!(
                "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\nProxy-Authorization: Basic {token}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send GET");
    let response = read_until_contains(&mut client, "hello from the upstream proxy", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );

    let request = upstream.first_request();
    assert!(
        !request.to_lowercase().contains("proxy-authorization"),
        "client credentials leaked to the upstream: {request}"
    );
}

#[tokio::test]
async fn authenticates_clients_on_connect_too() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 0,
            credentials: Some(("admin".to_string(), "secret".to_string())),
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");
    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 407"),
        "unexpected response: {response}"
    );

    let token = model::base64_encode(b"admin:secret");
    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(
            format!(
                "CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\nProxy-Authorization: Basic {token}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send CONNECT");
    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );
}

#[tokio::test]
async fn sends_upstream_credentials_to_an_http_proxy() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let url = format!("http://provider-user:provider-pass@{}", upstream.address);
    let gateway = start_gateway(
        pool_from(&[(url, Duration::from_millis(1))]),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");
    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "unexpected response: {head}"
    );

    let request = upstream.first_request();
    let expected = model::base64_encode(b"provider-user:provider-pass");
    assert!(
        request.contains(&format!("Proxy-Authorization: Basic {expected}")),
        "upstream credentials missing: {request}"
    );
}

#[tokio::test]
async fn retries_another_upstream_when_the_first_is_dead() {
    let upstream = fake_http_proxy(ProxyBehaviour::Serve).await;
    let dead = dead_address().await;

    // Latency ordering forces the dead upstream to be picked first, so the
    // retry path is always exercised.
    let pool = pool_from(&[
        (format!("http://{dead}"), Duration::from_millis(1)),
        (
            format!("http://{}", upstream.address),
            Duration::from_millis(500),
        ),
    ]);

    let gateway = start_gateway(
        pool,
        GatewayOptions {
            strategy: Strategy::Latency,
            retries: 1,
            connect_timeout: Duration::from_secs(2),
            max_failures: 1,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the second upstream should have served the tunnel: {head}"
    );
    assert_eq!(upstream.request_count(), 1);
}

#[tokio::test]
async fn reports_bad_gateway_when_every_upstream_fails() {
    let upstream = fake_http_proxy(ProxyBehaviour::Refuse).await;
    let gateway = start_gateway(
        pool_from(&http_proxy(&[upstream.address])),
        GatewayOptions {
            retries: 1,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 502"),
        "unexpected response: {response}"
    );
}

#[tokio::test]
async fn answers_503_when_the_pool_is_empty() {
    let gateway = start_gateway(Arc::new(ProxyPool::new()), GatewayOptions::default()).await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "unexpected response: {response}"
    );
}

#[tokio::test]
async fn connect_tunnels_through_socks5() {
    let socks = fake_socks5_proxy(None).await;
    let gateway = start_gateway(
        pool_from(&[(
            format!("socks5://{}", socks.address),
            Duration::from_millis(1),
        )]),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "unexpected response: {head}"
    );

    client
        .write_all(b"ping\n")
        .await
        .expect("write into tunnel");
    let echoed = read_until_contains(&mut client, "echo:ping", WAIT).await;
    assert!(
        echoed.contains("echo:ping"),
        "tunnel did not echo: {echoed}"
    );

    // `socks5://` resolves locally, so the proxy receives an IPv4 target.
    assert_eq!(socks.targets(), vec![(0x01u8, "127.0.0.1:443".to_string())]);
}

#[tokio::test]
async fn socks5h_lets_the_proxy_resolve_the_hostname() {
    let socks = fake_socks5_proxy(None).await;
    let gateway = start_gateway(
        pool_from(&[(
            format!("socks5h://{}", socks.address),
            Duration::from_millis(1),
        )]),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "unexpected response: {head}"
    );

    // Address type 3 is a domain name: remote DNS.
    assert_eq!(
        socks.targets(),
        vec![(0x03u8, "example.test:443".to_string())]
    );
}

#[tokio::test]
async fn socks5_authenticates_with_credentials() {
    let socks = fake_socks5_proxy(Some(("user".to_string(), "pass".to_string()))).await;
    let gateway = start_gateway(
        pool_from(&[(
            format!("socks5://user:pass@{}", socks.address),
            Duration::from_millis(1),
        )]),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let head = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "unexpected response: {head}"
    );
    assert_eq!(
        socks.targets().len(),
        1,
        "the authenticated handshake must succeed"
    );
}

#[tokio::test]
async fn socks5_without_credentials_cannot_use_a_guarded_proxy() {
    let socks = fake_socks5_proxy(Some(("user".to_string(), "pass".to_string()))).await;
    let gateway = start_gateway(
        pool_from(&[(
            format!("socks5://{}", socks.address),
            Duration::from_millis(1),
        )]),
        GatewayOptions {
            retries: 0,
            ..GatewayOptions::default()
        },
    )
    .await;

    let mut client = TcpStream::connect(gateway)
        .await
        .expect("connect to gateway");
    client
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n")
        .await
        .expect("send CONNECT");

    let response = read_until_contains(&mut client, "\r\n\r\n", WAIT).await;
    assert!(
        response.starts_with("HTTP/1.1 502"),
        "unexpected response: {response}"
    );
}
