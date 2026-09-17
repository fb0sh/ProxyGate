//! Health checker.
//!
//! For every proxy the checker asks three questions: can a request be made
//! through it, did the request succeed, and how long did it take.
//!
//! Concurrency is bounded by a [`Semaphore`], and the pool lock is never held
//! while a request is in flight: the checker works on a snapshot and writes the
//! results back in one short critical section.
//!
//! The same module owns [`ProxyClients`], the small cache of pre-configured
//! upstream HTTP clients shared by the checker and by the gateway's plain HTTP
//! path. Building a client is not free, so one is kept per (proxy, purpose).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reqwest::Client;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{HealthConfig, HealthRequirement};
use crate::error::{Error, Result};
use crate::model::{ProbeOutcome, Proxy, ProxyId};
use crate::pool::{HealthUpdate, PoolStats, ProxyPool};

/// What an upstream client is used for; the two flavours need different
/// timeouts and redirect behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientMode {
    /// Short-lived health probes.
    Check,
    /// Client requests forwarded by the gateway.
    Forward,
}

/// Cache of upstream-configured reqwest clients.
#[derive(Debug)]
pub struct ProxyClients {
    clients: Mutex<HashMap<(ClientMode, ProxyId), Client>>,
    check_timeout: Duration,
    connect_timeout: Duration,
    capacity: usize,
}

impl ProxyClients {
    pub fn new(check_timeout: Duration, connect_timeout: Duration) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            check_timeout,
            connect_timeout,
            capacity: 512,
        }
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Returns a client that routes every request through `proxy`.
    pub fn get(&self, proxy: &Proxy, mode: ClientMode) -> Result<Client> {
        let key = (mode, proxy.id.clone());
        if let Some(client) = self.lock().get(&key) {
            return Ok(client.clone());
        }
        let client = self.build(proxy, mode)?;
        let mut guard = self.lock();
        // Simple bound: drop everything rather than evicting one by one.
        if guard.len() >= self.capacity {
            guard.clear();
        }
        guard.insert(key, client.clone());
        Ok(client)
    }

    fn build(&self, proxy: &Proxy, mode: ClientMode) -> Result<Client> {
        let builder = Client::builder()
            // This client must use exactly this upstream: never fall back to
            // environment or system proxies.
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy.url.clone())?)
            .connect_timeout(self.connect_timeout)
            .user_agent(concat!("proxygate/", env!("CARGO_PKG_VERSION")));

        let builder = match mode {
            ClientMode::Check => builder
                .timeout(self.check_timeout)
                .http1_only()
                .redirect(reqwest::redirect::Policy::limited(5)),
            // Forwarding must be transparent: the client decides what to do with
            // 3xx responses, and a large download may legitimately take minutes.
            ClientMode::Forward => builder.redirect(reqwest::redirect::Policy::none()),
        };

        builder.build().map_err(Error::Http)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(ClientMode, ProxyId), Client>> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Result of probing one target through one proxy.
#[derive(Debug, Clone)]
pub struct TargetResult {
    pub target: Arc<str>,
    pub ok: bool,
    pub latency: Option<Duration>,
    pub error: Option<String>,
}

impl TargetResult {
    /// `https://www.google.com/generate_204` -> `www.google.com/generate_204`.
    pub fn label(&self) -> &str {
        short_target(&self.target)
    }

    pub fn latency_ms(&self) -> Option<u64> {
        self.latency.map(|latency| latency.as_millis() as u64)
    }
}

/// Result of checking a single proxy against every target.
#[derive(Debug, Clone)]
pub struct HealthResult {
    pub id: ProxyId,
    /// Whether the proxy satisfied `health.require` across all targets.
    pub alive: bool,
    /// Slowest answered target.
    pub latency: Option<Duration>,
    /// Per-target outcome, in configuration order.
    pub targets: Vec<TargetResult>,
    /// Failure summary, `None` when the proxy is alive.
    pub error: Option<String>,
}

/// Aggregate of one health pass.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub checked: usize,
    pub alive: usize,
    pub dead: usize,
    pub duration: Duration,
    pub results: Vec<HealthResult>,
}

impl CheckReport {
    /// Compact one-line summary for logs.
    pub fn summary(&self) -> String {
        format!(
            "{} checked, {} alive, {} dead in {:.1}s",
            self.checked,
            self.alive,
            self.dead,
            self.duration.as_secs_f64()
        )
    }
}

/// Probes every proxy against every configured target.
///
/// The targets of one proxy are probed *concurrently* — a proxy either reaches
/// all of them within one timeout window or it does not, so adding a target does
/// not multiply the wall clock time of a health pass.
#[derive(Debug, Clone)]
pub struct HealthChecker {
    targets: Vec<Arc<str>>,
    require: HealthRequirement,
    concurrency: usize,
    max_failures: u32,
    clients: Arc<ProxyClients>,
}

impl HealthChecker {
    pub fn new(config: &HealthConfig, clients: Arc<ProxyClients>) -> Self {
        Self {
            targets: config
                .targets()
                .iter()
                .map(|target| Arc::from(target.as_str()))
                .collect(),
            require: config.require,
            concurrency: config.concurrency.max(1),
            max_failures: config.max_failures,
            clients,
        }
    }

    /// The configured probe targets.
    pub fn targets(&self) -> Vec<&str> {
        self.targets.iter().map(|target| target.as_ref()).collect()
    }

    /// Human readable target list, e.g. `google.com/generate_204 + cn.bing.com/`.
    pub fn targets_label(&self) -> String {
        self.targets
            .iter()
            .map(|target| short_target(target))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    pub fn require(&self) -> HealthRequirement {
        self.require
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn max_failures(&self) -> u32 {
        self.max_failures
    }

    /// Checks every proxy with at most `concurrency` requests in flight.
    pub async fn check_all(&self, proxies: &[Proxy]) -> CheckReport {
        let started = Instant::now();
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut tasks: JoinSet<HealthResult> = JoinSet::new();
        let mut results = Vec::with_capacity(proxies.len());

        for proxy in proxies {
            // Acquiring before spawning keeps the number of live tasks bounded.
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("health semaphore is never closed");
            let checker = self.clone();
            let proxy = proxy.clone();
            tasks.spawn(async move {
                let result = checker.check_one(&proxy).await;
                drop(permit);
                result
            });

            // Harvest finished tasks so a huge pool does not accumulate results.
            while let Some(joined) = tasks.try_join_next() {
                if let Ok(result) = joined {
                    results.push(result);
                }
            }
        }

        while let Some(joined) = tasks.join_next().await {
            if let Ok(result) = joined {
                results.push(result);
            }
        }

        let alive = results.iter().filter(|result| result.alive).count();
        CheckReport {
            checked: results.len(),
            alive,
            dead: results.len() - alive,
            duration: started.elapsed(),
            results,
        }
    }

    /// Checks all proxies and writes the outcome into the pool.
    pub async fn check_and_apply(&self, pool: &ProxyPool, proxies: &[Proxy]) -> CheckReport {
        let report = self.check_all(proxies).await;
        let now = SystemTime::now();
        let updates: Vec<(ProxyId, HealthUpdate)> = report
            .results
            .iter()
            .map(|result| {
                (
                    result.id.clone(),
                    HealthUpdate {
                        alive: result.alive,
                        latency: result.latency,
                        checked_at: now,
                        probes: result
                            .targets
                            .iter()
                            .map(|target| ProbeOutcome {
                                target: target.target.clone(),
                                ok: target.ok,
                                latency: target.latency,
                            })
                            .collect(),
                    },
                )
            })
            .collect();
        let _stats: PoolStats = pool.apply_health_pass(&updates, self.max_failures);
        report
    }

    /// Checks one proxy against every target, concurrently.
    pub async fn check_one(&self, proxy: &Proxy) -> HealthResult {
        let client = match self.clients.get(proxy, ClientMode::Check) {
            Ok(client) => client,
            Err(error) => {
                let message = error.to_string();
                return HealthResult {
                    id: proxy.id.clone(),
                    alive: false,
                    latency: None,
                    targets: self
                        .targets
                        .iter()
                        .map(|target| TargetResult {
                            target: target.clone(),
                            ok: false,
                            latency: None,
                            error: Some(message.clone()),
                        })
                        .collect(),
                    error: Some(message),
                };
            }
        };

        // One client, all targets at once. The client's connection pool keeps
        // the probes independent of each other.
        let results: Vec<TargetResult> =
            futures_util::future::join_all(self.targets.iter().map(|target| {
                let client = client.clone();
                async move { probe(&client, target.clone()).await }
            }))
            .await;

        let passed = results.iter().filter(|result| result.ok).count();
        let alive = self.require.satisfied_by(passed, results.len());

        // The slowest answered target is the honest latency of the tunnel.
        let latency = results
            .iter()
            .filter(|result| result.ok)
            .filter_map(|result| result.latency)
            .max();

        let error = if alive {
            None
        } else {
            Some(failure_summary(&results))
        };

        HealthResult {
            id: proxy.id.clone(),
            alive,
            latency,
            targets: results,
            error,
        }
    }
}

/// Probes a single target through an already-configured client.
async fn probe(client: &Client, target: Arc<str>) -> TargetResult {
    let started = Instant::now();
    match client.get(target.as_ref()).send().await {
        Ok(response) => {
            let status = response.status();
            let latency = started.elapsed();
            // Drain the body so the probe is complete and the socket is
            // released before we report.
            let _ = response.bytes().await;
            TargetResult {
                target,
                ok: status.is_success(),
                latency: Some(latency),
                error: (!status.is_success()).then(|| format!("unexpected status {status}")),
            }
        }
        Err(error) => TargetResult {
            target,
            ok: false,
            latency: None,
            error: Some(describe_error(&error)),
        },
    }
}

/// `google.com/generate_204: timeout; cn.bing.com/: ok`, failing targets first.
fn failure_summary(results: &[TargetResult]) -> String {
    let mut parts: Vec<String> = results
        .iter()
        .filter(|result| !result.ok)
        .map(|result| {
            format!(
                "{}: {}",
                short_target(&result.target),
                result.error.as_deref().unwrap_or("failed")
            )
        })
        .collect();
    if parts.is_empty() {
        parts.push("no target answered".to_string());
    }
    parts.join("; ")
}

/// `https://cn.bing.com/` -> `cn.bing.com/`.
fn short_target(target: &str) -> &str {
    target
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(target)
}

/// Compact error description: classification plus the root cause.
pub fn describe_error(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else {
        "request"
    };

    let mut root: &(dyn std::error::Error + 'static) = error;
    while let Some(source) = std::error::Error::source(root) {
        root = source;
    }

    if root.to_string() == error.to_string() {
        format!("{kind}: {}", error)
    } else {
        format!("{kind}: {} ({})", error, root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::normalize;
    use crate::pool::ProxyPool;

    fn proxy(port: u16) -> Proxy {
        Proxy::new(normalize(&format!("127.0.0.1:{port}")).unwrap())
    }

    /// A fake upstream HTTP proxy that answers `200` for one target and `502`
    /// for everything else — the cheapest way to make two targets disagree.
    async fn selective_proxy() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 4096];
                    loop {
                        let read = stream.read(&mut buffer).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        let head = String::from_utf8_lossy(&buffer[..read]);
                        let response = if head.contains("good.test") {
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                        } else {
                            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n"
                        };
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        address
    }

    async fn two_targets(
        require: crate::config::HealthRequirement,
    ) -> (HealthChecker, std::net::SocketAddr) {
        let upstream = selective_proxy().await;
        let config = HealthConfig {
            targets: Some(vec![
                "http://good.test/ok".to_string(),
                "http://bad.test/".to_string(),
            ]),
            timeout: Duration::from_secs(2),
            concurrency: 2,
            require,
            ..HealthConfig::default()
        };
        let clients = Arc::new(ProxyClients::new(config.timeout, config.timeout));
        (HealthChecker::new(&config, clients), upstream)
    }

    fn proxy_at(address: std::net::SocketAddr) -> Proxy {
        Proxy::new(normalize(&address.to_string()).unwrap())
    }

    #[tokio::test]
    async fn require_all_needs_every_target() {
        let (checker, upstream) = two_targets(crate::config::HealthRequirement::All).await;

        let result = &checker.check_all(&[proxy_at(upstream)]).await.results[0];
        assert_eq!(result.targets.len(), 2, "one probe per configured target");
        assert!(
            result.targets[0].ok,
            "good.test answers 200 through the proxy"
        );
        assert!(
            !result.targets[1].ok,
            "bad.test answers 502 through the proxy"
        );
        assert!(
            !result.alive,
            "with `require: all`, a half-reachable proxy is dead"
        );
        assert_eq!(
            result.latency, result.targets[0].latency,
            "latency comes from the targets that answered"
        );
        let error = result.error.clone().expect("a failure summary");
        assert!(
            error.contains("bad.test"),
            "the failing target is named: {error}"
        );
    }

    #[tokio::test]
    async fn require_any_accepts_a_partially_reachable_proxy() {
        let (checker, upstream) = two_targets(crate::config::HealthRequirement::Any).await;

        let result = &checker.check_all(&[proxy_at(upstream)]).await.results[0];
        assert!(result.alive, "one answered target satisfies `any`");
        assert_eq!(result.targets.iter().filter(|target| target.ok).count(), 1);
        assert!(result.error.is_none(), "no failure summary when alive");
    }

    #[tokio::test]
    async fn stores_per_target_results_in_the_pool() {
        let (checker, upstream) = two_targets(crate::config::HealthRequirement::Any).await;

        let pool = ProxyPool::new();
        let (id, _) = pool.insert(normalize(&upstream.to_string()).unwrap());
        checker.check_and_apply(&pool, &pool.snapshot()).await;

        let proxy = pool.get(&id).unwrap();
        assert!(proxy.alive);
        assert_eq!(proxy.probes.len(), 2);
        assert_eq!(proxy.targets_summary(), "1/2");
        assert_eq!(proxy.targets_passed(), 1);
        assert_eq!(proxy.failed_targets(), vec!["http://bad.test/"]);
    }

    #[tokio::test]
    async fn reports_dead_proxies_without_hanging() {
        // Port 1 is almost always closed; the connect fails immediately.
        let config = HealthConfig {
            targets: Some(vec!["http://127.0.0.1:1/generate_204".into()]),
            timeout: Duration::from_secs(2),
            concurrency: 4,
            ..HealthConfig::default()
        };
        let clients = Arc::new(ProxyClients::new(config.timeout, config.timeout));
        let checker = HealthChecker::new(&config, clients);

        let report = checker.check_all(&[proxy(1)]).await;
        assert_eq!(report.checked, 1);
        assert_eq!(report.alive, 0);
        assert!(report.results[0].error.is_some());
    }

    #[tokio::test]
    async fn applies_threshold_through_the_pool() {
        let config = HealthConfig {
            targets: Some(vec!["http://127.0.0.1:1/generate_204".into()]),
            timeout: Duration::from_secs(2),
            concurrency: 2,
            max_failures: 2,
            ..HealthConfig::default()
        };
        let clients = Arc::new(ProxyClients::new(config.timeout, config.timeout));
        let checker = HealthChecker::new(&config, clients);

        let pool = ProxyPool::new();
        let (id, _) = pool.insert(normalize("127.0.0.1:1").unwrap());
        // Pretend the proxy was alive before the first pass.
        pool.record_success(&id, Some(Duration::from_millis(5)), SystemTime::now());

        let proxies = pool.snapshot();
        checker.check_and_apply(&pool, &proxies).await;
        assert!(
            pool.get(&id).unwrap().alive,
            "one failure is below the threshold"
        );

        checker.check_and_apply(&pool, &proxies).await;
        let proxy = pool.get(&id).unwrap();
        assert!(
            !proxy.alive,
            "second consecutive failure marks the proxy dead"
        );
        assert_eq!(proxy.failures, 2);
    }

    #[test]
    fn caches_clients_per_proxy_and_mode() {
        let clients = ProxyClients::new(Duration::from_secs(1), Duration::from_secs(1));
        let upstream = proxy(8080);

        clients.get(&upstream, ClientMode::Check).unwrap();
        clients.get(&upstream, ClientMode::Check).unwrap();
        assert_eq!(
            clients.len(),
            1,
            "the same proxy and mode reuses one client"
        );

        clients.get(&upstream, ClientMode::Forward).unwrap();
        assert_eq!(clients.len(), 2, "forwarding needs different settings");

        clients.get(&proxy(8081), ClientMode::Check).unwrap();
        assert_eq!(clients.len(), 3);

        clients.clear();
        assert!(clients.is_empty());
    }

    #[test]
    fn capacity_bound_clears_the_cache() {
        let clients =
            ProxyClients::new(Duration::from_secs(1), Duration::from_secs(1)).with_capacity(2);
        for port in 1..=5 {
            clients.get(&proxy(port), ClientMode::Check).unwrap();
        }
        assert!(clients.len() <= 2);
    }
}
