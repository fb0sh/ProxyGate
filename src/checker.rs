//! 健康检查器。
//!
//! 对每个代理，检查器问三个问题：能否通过它发出请求、
//! 请求是否成功、以及请求耗时多久。
//!
//! 并发由 [`Semaphore`] 限制，并且请求在途期间绝不持有代理池锁：
//! 检查器基于快照工作，最后在一个很短的临界区里把结果写回。
//!
//! 同一模块还负责 [`ProxyClients`]：一份预配置上游 HTTP 客户端的小缓存，
//! 由检查器和网关的普通 HTTP 路径共享。
//! 构建客户端并不廉价，因此每个 `(代理, 用途)` 组合保留一个。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reqwest::Client;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{HealthConfig, HealthRequirement};
use crate::error::{Error, Result};
use crate::model::{ProbeOutcome, Proxy, ProxyId};
use crate::pool::{HealthPolicy, HealthUpdate, PoolStats, ProxyPool};
use crate::progress::{CheckEvent, Progress};

/// 上游客户端用于什么用途；两种用途需要不同的超时与重定向行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientMode {
    /// 生命周期很短的探测请求。
    Check,
    /// 由网关转发的客户端请求。
    Forward,
}

/// 预先配置好上游的 reqwest 客户端缓存。
#[derive(Debug)]
pub struct ProxyClients {
    /// 以 `(用途, 代理 ID)` 为键的客户端缓存。
    clients: Mutex<HashMap<(ClientMode, ProxyId), Client>>,
    /// 探测请求的总超时时间。
    check_timeout: Duration,
    /// 建立连接的超时时间。
    connect_timeout: Duration,
    /// 缓存容量上限，达到上限时整体清空而不是逐个淘汰。
    capacity: usize,
}

impl ProxyClients {
    /// 用给定的探测超时与连接超时创建缓存，初始容量为 512。
    pub fn new(check_timeout: Duration, connect_timeout: Duration) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            check_timeout,
            connect_timeout,
            capacity: 512,
        }
    }

    /// 设置缓存容量上限，至少为 1。
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// 当前缓存的客户端数量。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 清空缓存。
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// 返回一个会把所有请求都经由 `proxy` 路由的客户端。
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

    /// 为一个代理按指定用途构建上游客户端。
    fn build(&self, proxy: &Proxy, mode: ClientMode) -> Result<Client> {
        let builder = Client::builder()
            // This client must use exactly this upstream: never fall back to
            // environment or system proxies.
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy.url.clone())?)
            .connect_timeout(self.connect_timeout)
            // 探测也装成普通桌面浏览器：`proxygate/x.y.z` 这种自报名号会被一部分
            // 站点（和一部分代理）直接拒掉，而那会把一条其实能用的代理判死。
            .user_agent(crate::useragent::random());

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

    /// 获取缓存锁，并忽略锁中毒。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(ClientMode, ProxyId), Client>> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 通过单个代理探测单个目标的结果。
#[derive(Debug, Clone)]
pub struct TargetResult {
    /// 探测目标的完整 URL。
    pub target: Arc<str>,
    /// 该目标是否成功应答。
    pub ok: bool,
    /// 该目标的响应耗时；失败时为 `None`。
    pub latency: Option<Duration>,
    /// 失败原因；成功时为 `None`。
    pub error: Option<String>,
}

impl TargetResult {
    /// 把探测目标缩短为便于阅读的形式，
    /// 例如 `https://www.google.com/generate_204` -> `www.google.com/generate_204`。
    pub fn label(&self) -> &str {
        short_target(&self.target)
    }

    /// 以毫秒表示的响应耗时。
    pub fn latency_ms(&self) -> Option<u64> {
        self.latency.map(|latency| latency.as_millis() as u64)
    }
}

/// 单个代理针对所有目标做一次检查的结果。
#[derive(Debug, Clone)]
pub struct HealthResult {
    /// 被检查代理的 ID。
    pub id: ProxyId,
    /// 本次检查中，该代理是否满足 `health.require`（`all` 或 `any`）。
    pub alive: bool,
    /// 已应答目标中最慢的耗时。
    pub latency: Option<Duration>,
    /// 各目标的探测结果，顺序与配置一致。
    pub targets: Vec<TargetResult>,
    /// 失败摘要；代理存活时为 `None`。
    pub error: Option<String>,
}

/// 一次健康检查整体的汇总。
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    /// 本次检查的代理数量。
    pub checked: usize,
    /// 判定为存活的代理数量。
    pub alive: usize,
    /// 判定为失效的代理数量。
    pub dead: usize,
    /// 本次检查的总耗时。
    pub duration: Duration,
    /// 每个代理的检查结果。
    pub results: Vec<HealthResult>,
}

impl CheckReport {
    /// 用于日志的紧凑单行摘要。
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

/// 用每一个已配置的目标探测每一个代理。
///
/// 同一个代理的多个目标是*并发*探测的——
/// 一个代理要么在一个超时窗口内到达全部目标，要么一个也到达不了，
/// 因此增加目标不会让一次健康检查的墙钟时间成倍增长。
///
/// `health.require` 取 `all` 或 `any`（默认 `any`），
/// 它决定多少个目标通过才算存活。
/// 至于失败容错，`max_failures` 只宽容那些曾经工作过、
/// 偶尔探测失败的代理；从未成功过的代理会在第一次失败时立即失效。
#[derive(Debug, Clone)]
pub struct HealthChecker {
    /// 已配置的探测目标。
    targets: Vec<Arc<str>>,
    /// 判定存活所需满足的条件（`all` 或 `any`）。
    require: HealthRequirement,
    /// 同时在途的检查数量上限。
    concurrency: usize,
    /// 连续失败多少次后判定为失效。
    /// 判死与退避规则，由健康检查统一使用（池子按它安排下一次探测）。
    policy: HealthPolicy,
    /// 与网关共享的上游客户端缓存。
    clients: Arc<ProxyClients>,
}

/// 探测过程中两次进度报告之间的最小间隔。
///
/// 一次全池探测在大池子上要几分钟，5 秒一行既不会刷屏又能看出在动。
const CHECK_REPORT_INTERVAL: Duration = Duration::from_secs(5);

impl HealthChecker {
    /// 根据健康检查配置与客户端缓存创建检查器。
    pub fn new(config: &HealthConfig, clients: Arc<ProxyClients>) -> Self {
        Self {
            targets: config
                .targets()
                .iter()
                .map(|target| Arc::from(target.as_str()))
                .collect(),
            require: config.require,
            concurrency: config.concurrency.max(1),
            policy: config.policy(),
            clients,
        }
    }

    /// 已配置的探测目标。
    pub fn targets(&self) -> Vec<&str> {
        self.targets.iter().map(|target| target.as_ref()).collect()
    }

    /// 便于人读的目标列表，例如 `google.com/generate_204 + cn.bing.com/`。
    pub fn targets_label(&self) -> String {
        self.targets
            .iter()
            .map(|target| short_target(target))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// 判定存活所需满足的条件。
    pub fn require(&self) -> HealthRequirement {
        self.require
    }

    /// 同时在途的检查数量上限。
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// 连续失败多少次后判定为失效。
    pub fn max_failures(&self) -> u32 {
        self.policy.max_failures
    }

    /// 检查所有代理，同时在途的请求不超过 `concurrency` 个。
    pub async fn check_all(&self, proxies: &[Proxy]) -> CheckReport {
        self.check_all_reporting(proxies, &()).await
    }

    /// 与 [`HealthChecker::check_all`] 相同，但把「探测了多少、活了多少」
    /// 按固定间隔发给 `progress`。
    ///
    /// 一次全池探测在大池子上要几分钟，中间不报进度就只能干等。
    pub async fn check_all_reporting(
        &self,
        proxies: &[Proxy],
        progress: &dyn Progress,
    ) -> CheckReport {
        let started = Instant::now();
        let total = proxies.len();
        let mut last_report = Instant::now();
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

            if last_report.elapsed() >= CHECK_REPORT_INTERVAL {
                progress.check(CheckEvent {
                    done: results.len(),
                    total,
                    alive: results.iter().filter(|result| result.alive).count(),
                    elapsed: started.elapsed(),
                });
                last_report = Instant::now();
            }
        }

        while let Some(joined) = tasks.join_next().await {
            if let Ok(result) = joined {
                results.push(result);
            }
            if last_report.elapsed() >= CHECK_REPORT_INTERVAL {
                progress.check(CheckEvent {
                    done: results.len(),
                    total,
                    alive: results.iter().filter(|result| result.alive).count(),
                    elapsed: started.elapsed(),
                });
                last_report = Instant::now();
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

    /// 检查所有代理，并把结果写回代理池。
    ///
    /// 写回时沿用代理池的失败规则：从未成功过的代理立即失效，
    /// `max_failures` 只宽容曾经可用、偶发失败的代理。
    pub async fn check_and_apply(&self, pool: &ProxyPool, proxies: &[Proxy]) -> CheckReport {
        self.check_and_apply_reporting(pool, proxies, &()).await
    }

    /// 与 [`HealthChecker::check_and_apply`] 相同，但把进度发给 `progress`。
    pub async fn check_and_apply_reporting(
        &self,
        pool: &ProxyPool,
        proxies: &[Proxy],
        progress: &dyn Progress,
    ) -> CheckReport {
        let report = self.check_all_reporting(proxies, progress).await;
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
        let _stats: PoolStats = pool.apply_health_pass(&updates, &self.policy);
        report
    }

    /// 并发地对所有目标检查单个代理。
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

/// 通过一个已经配置好的客户端探测单个目标。
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
            error: Some(crate::error::describe_reqwest_error(&error)),
        },
    }
}

/// 汇总失败的目标，例如
/// `google.com/generate_204: timeout; cn.bing.com/: ok`，失败的目标排在前面。
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

/// 去掉 scheme，把 `https://cn.bing.com/` 变成 `cn.bing.com/`。
fn short_target(target: &str) -> &str {
    target
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(target)
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
        pool.record_success(
            &id,
            Some(Duration::from_millis(5)),
            SystemTime::now(),
            &HealthPolicy::default(),
        );

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
