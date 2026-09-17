//! 应用运行时：配置与缓存的冷启动、刷新、健康检查与选择。
//!
//! 命令之间共享的一切都放在 [`App`] 里。它持有代理池、磁盘状态、订阅源集合
//! 与健康检查器，并且刻意做到可以被任意 Rust 程序直接使用——二进制只不过是
//! 对本模块的一层薄封装。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::checker::{CheckReport, HealthChecker, ProxyClients};
use crate::config::Config;
use crate::error::Result;
use crate::model::{self, ProbeOutcome, Proxy, ProxyId};
use crate::pool::{HealthRestore, ProxyPool, Selection};
use crate::selector::Strategy;
use crate::state::{self, CacheFile, HealthFile, StateStore, TargetHealthFile};
use crate::subscriber::SubscriberSet;

/// 一次选择最多间隔多久重写一次 `state.json`。
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Application runtime
// ---------------------------------------------------------------------------

/// 冷启动期间应当如何使用缓存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// 缓存比配置的时间间隔更新时才使用它。
    IfStale,
    /// 忽略缓存，直接做实际工作。
    Force,
    /// 无论缓存多旧都直接使用。
    Never,
}

/// 冷启动时对刷新与健康检查各自采用的缓存策略。
#[derive(Debug, Clone, Copy)]
pub struct Bootstrap {
    /// 订阅源刷新策略。
    pub refresh: Freshness,
    /// 健康检查策略。
    pub check: Freshness,
}

/// 一次订阅源刷新的统计结果。
#[derive(Debug, Clone)]
pub struct RefreshSummary {
    /// 参与本次刷新的订阅源数量。
    pub subscribers: usize,
    /// 抓取失败的订阅源数量。
    pub failed: usize,
    /// 成功取回的代理条目总数（含重复项）。
    pub fetched: usize,
    /// 归一化后新加入代理池的代理数量。
    pub added: usize,
    /// 代理池中已存在、本次未新增的代理数量。
    pub existing: usize,
    /// 因订阅源不再提供而被移除的代理数量。
    pub removed: usize,
    /// 因格式非法而被拒绝的条目数量。
    pub rejected: usize,
    /// 因订阅源的 `limit` 被丢弃的可用代理数量。
    pub truncated: usize,
    /// 本次刷新耗时。
    pub duration: Duration,
}

/// 所有命令共享的运行时上下文。
pub struct App {
    /// 已合并命令行覆盖项的运行时配置。
    pub config: Config,
    /// 实际加载的配置文件路径（使用内置默认值时为空）。
    pub config_path: Option<PathBuf>,
    /// 进程内唯一的代理池。
    pub pool: Arc<ProxyPool>,
    /// 负责读写 `state.json` 与缓存文件的磁盘状态存储。
    pub store: Arc<StateStore>,
    /// 网关与健康检查共用的 HTTP 客户端集合。
    pub clients: Arc<ProxyClients>,
    /// 健康检查器。
    pub checker: HealthChecker,
    /// 已构造好的订阅源集合。
    pub subscribers: SubscriberSet,
    /// 最近一次成功抓取订阅源的 Unix 秒数（0 表示从未抓取）。
    pub fetched_at: AtomicU64,
    /// 最近一次完成健康检查的 Unix 秒数（0 表示从未检查）。
    pub checked_at: AtomicU64,
    /// 串行化刷新与检查，避免 `serve` 同时跑两轮。
    pub busy: tokio::sync::Mutex<()>,
}

impl App {
    /// 加载配置状态与缓存，并执行请求的工作。
    ///
    /// 冷启动依次恢复三样东西：缓存里的代理、`state.json` 里的轮换事实、
    /// 上一次的健康检查结果；随后按 `options` 决定是否刷新订阅源与复查
    /// 健康状态。
    pub async fn bootstrap(
        config: Config,
        config_path: Option<PathBuf>,
        options: Bootstrap,
    ) -> Result<Self> {
        let store = Arc::new(StateStore::new(config.cache_dir()));
        store.ensure_dir()?;

        let clients = Arc::new(ProxyClients::new(
            config.health.timeout,
            config.gateway.connect_timeout,
        ));
        let checker = HealthChecker::new(&config.health, clients.clone());
        let subscribers = SubscriberSet::new(&config)?;
        let pool = Arc::new(ProxyPool::new());

        // 1. Proxies from the cache.
        let cache = store.load_cache();
        let now = SystemTime::now();
        let mut cached_urls = Vec::with_capacity(cache.proxies.len());
        for raw in &cache.proxies {
            match model::normalize(raw) {
                Ok(url) => cached_urls.push(url),
                Err(error) => debug!(entry = %raw, error = %error, "ignoring invalid cached proxy"),
            }
        }
        if !cached_urls.is_empty() {
            let merged = pool.merge(cached_urls);
            debug!(
                total = pool.len(),
                added = merged.added,
                "restored proxies from cache"
            );
        }

        // 2. Usage facts: generation and per-proxy last use.
        let restored = store.restore(&pool);
        if restored.restored > 0 || restored.missing > 0 {
            debug!(
                restored = restored.restored,
                missing = restored.missing,
                generation = restored.generation,
                "restored rotation state"
            );
        }

        // 3. Cached health, so an unchanged proxy keeps its last known status.
        let checked_at = cache.checked_at.as_deref().and_then(state::parse_rfc3339);
        if !cache.health.is_empty() {
            let entries: Vec<(ProxyId, HealthRestore)> = cache
                .health
                .iter()
                .map(|(id, entry)| {
                    (
                        id.clone(),
                        HealthRestore {
                            alive: entry.alive,
                            latency: entry.latency_ms.map(Duration::from_millis),
                            failures: entry.failures,
                            checked_at,
                            probes: entry
                                .targets
                                .iter()
                                .map(|probe| ProbeOutcome {
                                    target: Arc::from(probe.target.as_str()),
                                    ok: probe.ok,
                                    latency: probe.latency_ms.map(Duration::from_millis),
                                })
                                .collect(),
                        },
                    )
                })
                .collect();
            pool.restore_health(&entries);
        }

        let app = Self {
            fetched_at: AtomicU64::new(
                cache
                    .fetched_at
                    .as_deref()
                    .and_then(state::parse_rfc3339)
                    .map(|time| state::unix_secs(time).max(0) as u64)
                    .unwrap_or(0),
            ),
            checked_at: AtomicU64::new(
                checked_at
                    .map(|time| state::unix_secs(time).max(0) as u64)
                    .unwrap_or(0),
            ),
            config,
            config_path,
            pool,
            store,
            clients,
            checker,
            subscribers,
            busy: tokio::sync::Mutex::new(()),
        };

        if options.refresh == Freshness::Force
            || (options.refresh == Freshness::IfStale
                && !cache.proxies_fresh(app.config.refresh.interval, now))
        {
            let summary = app.refresh().await?;
            info!(
                fetched = summary.fetched,
                added = summary.added,
                failed_subscribers = summary.failed,
                "subscriber refresh complete"
            );
        }

        if options.check == Freshness::Force
            || (options.check == Freshness::IfStale
                && !cache.health_fresh(app.config.health.interval, now))
        {
            let report = app.check(false).await?;
            if report.checked > 0 {
                info!(report = %report.summary(), "initial health check complete");
            }
        }

        Ok(app)
    }

    /// 抓取全部订阅源，并把结果合并进代理池。
    ///
    /// 只有整轮抓取全部成功时才允许淘汰代理：否则一个失败的订阅源可能顺手
    /// 删掉它提供的全部代理。
    pub async fn refresh(&self) -> Result<RefreshSummary> {
        let _guard = self.busy.lock().await;
        let started = Instant::now();

        if self.subscribers.configured() == 0 {
            debug!("no subscribers configured; nothing to refresh");
        }

        let outcomes = self.subscribers.fetch_all().await;
        let all_ok = !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.ok());

        let mut urls = Vec::new();
        let mut fetched = 0;
        let mut failed = 0;
        let mut rejected = 0;
        let mut truncated = 0;
        for outcome in &outcomes {
            if outcome.ok() {
                fetched += outcome.count();
                rejected += outcome.rejected.len();
                truncated += outcome.truncated;
                debug!(
                    subscriber = %outcome.name,
                    found = outcome.count(),
                    rejected = outcome.rejected.len(),
                    skipped = outcome.skipped,
                    truncated = outcome.truncated,
                    elapsed_ms = outcome.duration.as_millis() as u64,
                    "subscriber fetched"
                );
            } else {
                failed += 1;
                warn!(
                    subscriber = %outcome.name,
                    error = outcome.error.as_deref().unwrap_or("unknown error"),
                    "subscriber failed"
                );
            }
            urls.extend(outcome.proxies.iter().cloned());
        }

        let fresh_ids: HashSet<ProxyId> = urls.iter().map(Proxy::id_of).collect();
        let merged = self.pool.merge(urls);

        // Only a completely successful refresh may evict proxies: a subscriber
        // that failed could otherwise delete all of its proxies.
        let mut removed = 0;
        if all_ok {
            removed = self.pool.retain(&fresh_ids);
            if removed > 0 {
                info!(removed, "dropped proxies that are no longer offered");
            }
        }

        self.fetched_at.store(
            state::unix_secs(SystemTime::now()).max(0) as u64,
            Ordering::SeqCst,
        );
        self.save_cache();

        Ok(RefreshSummary {
            subscribers: outcomes.len(),
            failed,
            fetched,
            added: merged.added,
            existing: merged.existing,
            removed,
            rejected,
            truncated,
            duration: started.elapsed(),
        })
    }

    /// 探测全部（或仅存活的）代理，并把结果写回代理池。
    pub async fn check(&self, alive_only: bool) -> Result<CheckReport> {
        let _guard = self.busy.lock().await;

        let mut proxies = self.pool.snapshot();
        if alive_only {
            proxies.retain(|proxy| proxy.alive);
        }
        if proxies.is_empty() {
            debug!("nothing to check");
            return Ok(CheckReport::default());
        }

        let report = self.checker.check_and_apply(&self.pool, &proxies).await;
        self.checked_at.store(
            state::unix_secs(SystemTime::now()).max(0) as u64,
            Ordering::SeqCst,
        );
        self.save_cache();
        Ok(report)
    }

    /// 挑选一个代理，并记录本次使用。
    pub fn select(&self, strategy: Strategy) -> Option<Selection> {
        let selection = self.pool.select(
            strategy,
            self.config.selection.reuse_after,
            SystemTime::now(),
        )?;

        tracing::debug!(
            proxy = %selection.proxy.to_masked_string(),
            round = selection.round,
            reset_round = selection.reset_round,
            candidates = selection.candidates,
            "selected proxy"
        );

        if let Err(error) = self.persist(false) {
            warn!(error = %error, "cannot persist rotation state");
        }
        Some(selection)
    }

    /// 写入轮换状态，可选择绕过节流。
    pub fn persist(&self, force: bool) -> Result<bool> {
        let now = SystemTime::now();
        if force {
            self.store.persist(&self.pool, now)?;
            Ok(true)
        } else {
            self.store
                .persist_throttled(&self.pool, now, PERSIST_INTERVAL)
        }
    }

    /// 写入订阅源与健康检查缓存。
    ///
    /// 落盘只是优化，不影响正确性：缓存目录只读时，缓存会退化成“永不新鲜”，
    /// 而不是把网关拖垮。
    fn save_cache(&self) {
        if let Err(error) = self.try_save_cache() {
            warn!(error = %error, "cannot write the cache; continuing without it");
        }
    }

    /// 真正执行缓存写入，把失败交给调用方处理。
    fn try_save_cache(&self) -> Result<()> {
        let proxies = self.pool.snapshot();
        let fetched = self.fetched_at.load(Ordering::SeqCst);
        let checked = self.checked_at.load(Ordering::SeqCst);

        let cache = CacheFile {
            fetched_at: (fetched > 0)
                .then(|| state::to_rfc3339(state::from_unix_secs(fetched as i64))),
            proxies: proxies.iter().map(Proxy::to_full_string).collect(),
            checked_at: (checked > 0)
                .then(|| state::to_rfc3339(state::from_unix_secs(checked as i64))),
            health: proxies
                .iter()
                .map(|proxy| {
                    (
                        proxy.id.clone(),
                        HealthFile {
                            alive: proxy.alive,
                            latency_ms: proxy.latency_ms(),
                            failures: proxy.failures,
                            targets: proxy
                                .probes
                                .iter()
                                .map(|probe| TargetHealthFile {
                                    target: probe.target.to_string(),
                                    ok: probe.ok,
                                    latency_ms: probe.latency.map(|d| d.as_millis() as u64),
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
        };
        self.store.save_cache(&cache)
    }

    /// 解释当前为什么没有任何代理可以被选中。
    pub fn empty_reason(&self) -> String {
        let stats = self.pool.stats();
        if stats.total == 0 {
            if self.subscribers.configured() == 0 {
                "the pool is empty and no subscribers are configured; add one to config.yaml and run `proxygate refresh`".to_string()
            } else {
                format!(
                    "the pool is empty: {} subscriber(s) produced no usable proxy (run `proxygate refresh -v` to see why)",
                    self.subscribers.configured()
                )
            }
        } else if stats.alive == 0 {
            format!(
                "{} proxy(ies) in the pool, none passed the health check against {} (require: {}; run `proxygate check` for the per-target reasons)",
                stats.total,
                self.config.health.targets().join(" + "),
                self.config.health.require
            )
        } else {
            format!(
                "no proxy could be selected ({} alive of {} total)",
                stats.alive, stats.total
            )
        }
    }
}

/// 关闭标志被置位（或发送端被丢弃）时完成。
pub(crate) async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    let _ = receiver.wait_for(|stop| *stop).await;
}

/// 按刷新间隔重新抓取订阅源，并复查健康状态。
///
/// 若刷新带来了新代理，会立刻补一次健康检查，让它们先拿到判定结果，再被
/// 分发出去。
pub(crate) async fn refresh_loop(app: Arc<App>, shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(app.config.refresh.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; bootstrap already did the work.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = ticker.tick() => {
                let added = match app.refresh().await {
                    Ok(summary) => {
                        info!(
                            fetched = summary.fetched,
                            added = summary.added,
                            removed = summary.removed,
                            failed = summary.failed,
                            "subscriber refresh complete"
                        );
                        summary.added
                    }
                    Err(error) => {
                        warn!(error = %error, "subscriber refresh failed");
                        0
                    }
                };

                // New proxies need a verdict before they can be handed out; if
                // nothing changed, the health loop owns the next pass.
                if added > 0 {
                    match app.check(false).await {
                        Ok(report) => info!(report = %report.summary(), "health check complete"),
                        Err(error) => warn!(error = %error, "health check failed"),
                    }
                }
            }
        }
    }
}

/// 按健康检查间隔重新探测健康状态。
pub(crate) async fn health_loop(app: Arc<App>, shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(app.config.health.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = ticker.tick() => {
                match app.check(false).await {
                    Ok(report) => info!(report = %report.summary(), "health check complete"),
                    Err(error) => warn!(error = %error, "health check failed"),
                }
            }
        }
    }
}
