//! 应用运行时：配置与缓存的冷启动、刷新、健康检查与选择。
//!
//! 命令之间共享的一切都放在 [`App`] 里。它持有代理池、磁盘状态、订阅源集合
//! 与健康检查器，并且刻意做到可以被任意 Rust 程序直接使用——二进制只不过是
//! 对本模块的一层薄封装。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use crate::checker::{CheckReport, HealthChecker, ProxyClients};
use crate::config::Config;
use crate::error::Result;
use crate::model::{self, ProbeOutcome, Proxy, ProxyId};
use crate::pool::{HealthRestore, ProxyPool, Selection};
use crate::progress::{CheckEvent, FetchEvent, Progress, VerifyEvent};
use crate::selector::Strategy;
use crate::state::{self, CacheFile, HealthFile, StateStore, TargetHealthFile};
use crate::subscriber::SubscriberSet;

/// 一次选择最多间隔多久重写一次 `state.json`。
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

/// 冷启动失败后，两次尝试之间至少间隔多久。
///
/// 客户端在初始化完成前会拿到 `503`，并且每次都会请求一次重试；没有这个
/// 下限，一个紧循环的客户端就能让刚失败的抓取不停重跑。
pub const INITIALIZE_RETRY_INTERVAL: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Application runtime
// ---------------------------------------------------------------------------

/// 冷启动的就绪状态，CLI、网关与 REST API 共享。
///
/// 它回答两个问题：`serve` 是否可以开始发放代理（[`Readiness::is_ready`]），
/// 以及现在是不是正在做第一次抓取与探测（[`Readiness::is_initializing`]）。
/// API 在未就绪时返回 `503`，并调用 [`Readiness::request_init`] 请求后台立刻
/// 再试一次。
#[derive(Debug, Default)]
pub struct Readiness {
    /// 冷启动是否已经成功完成过一次。
    ready: AtomicBool,
    /// 是否正有一个任务在初始化。
    running: AtomicBool,
    /// 最近一次初始化的失败原因；成功后清空。
    error: Mutex<Option<String>>,
    /// 初始化尝试次数，用于 `/api/v1/health` 与日志。
    attempts: AtomicU64,
    /// 最近一次尝试开始的 Unix 秒数。
    last_attempt: AtomicU64,
    /// 客户端请求重试时用来唤醒 [`App::initialize_throttled`] 的等待。
    wakeup: Notify,
}

impl Readiness {
    /// 冷启动是否已经成功完成过至少一次。
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// 是否正有任务在初始化。
    pub fn is_initializing(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// 最近一次初始化的失败原因（成功后为 `None`）。
    pub fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 已经开始的初始化尝试次数。
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::SeqCst)
    }

    /// 请求立刻重试一次初始化。
    ///
    /// 由 REST API 在返回 `503` 时调用：等待中的刷新循环会因此提前醒来，
    /// 而不是干等到下一个刷新周期。
    pub fn request_init(&self) {
        self.wakeup.notify_one();
    }

    /// 等待一次重试请求，最多等 `timeout`。
    pub(crate) async fn wait_for_request(&self, timeout: Duration) {
        tokio::select! {
            _ = self.wakeup.notified() => {}
            _ = tokio::time::sleep(timeout) => {}
        }
    }

    /// 标记为已就绪，并清掉上一次的失败原因。
    pub fn mark_ready(&self) {
        *self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.ready.store(true, Ordering::SeqCst);
    }

    /// 记录一次初始化失败；在成功之前会一直保留最后这个原因。
    pub fn mark_failed(&self, message: String) {
        *self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    }

    /// 占住“正在初始化”的位置。
    ///
    /// 已经就绪、已有任务在跑，或距离上次尝试不足 `min_interval` 时返回
    /// `None`。返回的 [`InitGuard`] 在 `Drop` 时释放占用，所以即使任务
    /// panic，“正在初始化”也不会永久卡住。
    fn claim(self: &Arc<Self>, min_interval: Duration) -> Option<InitGuard> {
        if self.is_ready() {
            return None;
        }
        let now = state::unix_secs(SystemTime::now()).max(0) as u64;
        if !min_interval.is_zero() {
            let last = self.last_attempt.load(Ordering::SeqCst);
            if last > 0 && now.saturating_sub(last) < min_interval.as_secs() {
                return None;
            }
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.last_attempt.store(now, Ordering::SeqCst);
        Some(InitGuard {
            readiness: self.clone(),
        })
    }
}

/// [`Readiness`] 内部用来占住“正在初始化”的守卫。
///
/// 由 `Readiness::claim` 返回，`Drop` 时释放占用。
#[derive(Debug)]
pub struct InitGuard {
    /// 占用时对应的就绪状态。
    readiness: Arc<Readiness>,
}

impl InitGuard {
    /// 本次初始化成功完成。
    pub fn succeeded(&self) {
        self.readiness.mark_ready();
    }

    /// 本次初始化失败，并记录原因。
    pub fn failed(&self, message: String) {
        self.readiness.mark_failed(message);
    }
}

impl Drop for InitGuard {
    fn drop(&mut self) {
        self.readiness.running.store(false, Ordering::SeqCst);
    }
}

/// 冷启动期间应当如何使用缓存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// 缓存比配置的时间间隔更新时才使用它。
    IfStale,
    /// 忽略缓存，直接做实际工作。
    Force,
    /// 无论缓存多旧都直接使用。
    Never,
    /// 只有完全没有结果时才做实际工作。
    ///
    /// 健康检查用它：`get` / `list` 这类"顺手问一句"的命令不该因为上次探测
    /// 过了 30 秒就对整个代理池重探一遍——几千个代理要几分钟，那就不叫
    /// `get` 了。探测归 `refresh`、`check` 和 `serve` 的后台循环。
    IfMissing,
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
    /// 冷启动的就绪状态：`serve` 未就绪时对外返回 `503`。
    pub readiness: Arc<Readiness>,
    /// 抓取与探测的进度接收器；命令行渲染成进度，`serve` 转成日志。
    pub progress: Arc<dyn Progress>,
    /// 发放前验证用的检查器：超时取 `selection.verify_timeout`，比全池
    /// 探测用的 `health.timeout` 短。
    verify_checker: HealthChecker,
    /// `POST /api/v1/refresh` 用它把刷新循环提前叫醒。
    refresh_now: Notify,
    /// `POST /api/v1/check` 用它把探测循环提前叫醒。
    check_now: Notify,
}

impl std::fmt::Debug for App {
    /// 手写 `Debug`：`App` 里装的是池子、客户端与任务状态，没有适合一行打印
    /// 的东西，列几个有意义的字段就够（`ApiState` 需要 `Debug`）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("config_path", &self.config_path)
            .field("proxies", &self.pool.len())
            .field("ready", &self.readiness.is_ready())
            .finish_non_exhaustive()
    }
}

impl App {
    /// 读取本地状态，不做任何网络请求。
    ///
    /// 冷启动依次恢复三样东西：缓存里的代理、`state.json` 里的轮换事实、
    /// 上一次的健康检查结果。抓取订阅源与探测代理是 [`App::initialize`]
    /// 的事，因为 `serve` 需要先把端口挂上、再在后台慢慢做那件事。
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_progress(config, config_path, Arc::new(()))
    }

    /// 与 [`App::new`] 相同，但指定进度接收器。
    pub fn new_with_progress(
        config: Config,
        config_path: Option<PathBuf>,
        progress: Arc<dyn Progress>,
    ) -> Result<Self> {
        let store = Arc::new(StateStore::new(config.cache_dir()));
        store.ensure_dir()?;

        let clients = Arc::new(ProxyClients::new(
            config.health.timeout,
            config.gateway.connect_timeout,
        ));
        let checker = HealthChecker::new(&config.health, clients.clone());
        // 发放验证用的是同一套目标与判定规则，只是超时更短。
        let verify_clients = Arc::new(ProxyClients::new(
            config.selection.verify_timeout,
            config.gateway.connect_timeout,
        ));
        let verify_checker = HealthChecker::new(&config.health, verify_clients);
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
            readiness: Arc::new(Readiness::default()),
            progress,
            verify_checker,
            refresh_now: Notify::new(),
            check_now: Notify::new(),
        };

        // The cache is the only source of freshness left once `initialize`
        // runs on its own; remember what it said about the two timestamps.
        debug!(
            proxies_fresh = cache.proxies_fresh(app.config.refresh.interval, now),
            health_fresh = cache.health_fresh(app.config.health.interval, now),
            "local state restored"
        );

        Ok(app)
    }

    /// 冷启动：先读本地状态，再按 `options` 抓取订阅源与探测代理。
    ///
    /// 命令行走这条路（阻塞到做完为止）；`serve` 走 [`App::new`] 加后台的
    /// [`App::initialize_throttled`]，这样端口能立刻开始接受请求。
    pub async fn bootstrap(
        config: Config,
        config_path: Option<PathBuf>,
        options: Bootstrap,
    ) -> Result<Self> {
        Self::bootstrap_with_progress(config, config_path, options, Arc::new(())).await
    }

    /// 与 [`App::bootstrap`] 相同，但指定进度接收器。
    ///
    /// 命令行走这条路：冷启动可能要几分钟（抓取 + 首次探测），进度要能实时
    /// 显示，而不是等结束才知道发生了什么。
    pub async fn bootstrap_with_progress(
        config: Config,
        config_path: Option<PathBuf>,
        options: Bootstrap,
        progress: Arc<dyn Progress>,
    ) -> Result<Self> {
        let app = Self::new_with_progress(config, config_path, progress)?;
        app.initialize(options).await?;
        Ok(app)
    }

    /// 执行一次冷启动工作，成功后标记为就绪。
    ///
    /// 已经就绪、或已有任务在初始化时直接返回成功。
    pub async fn initialize(&self, options: Bootstrap) -> Result<()> {
        self.initialize_throttled(options, Duration::ZERO).await
    }

    /// 与 [`App::initialize`] 相同，但限制两次尝试的最小间隔。
    ///
    /// `min_interval` 用来挡掉“客户端不停拿到 503、于是不停请求重试”造成的
    /// 重复抓取。因为要先占住初始化位置再干活，重复调用不会并发跑两轮。
    pub async fn initialize_throttled(
        &self,
        options: Bootstrap,
        min_interval: Duration,
    ) -> Result<()> {
        let Some(guard) = self.readiness.claim(min_interval) else {
            debug!("initialization skipped: ready, already running, or too soon");
            return Ok(());
        };

        let result = self.initialize_inner(options).await;
        match &result {
            Ok(()) => guard.succeeded(),
            Err(error) => guard.failed(error.to_string()),
        }
        result
    }

    /// 真正执行冷启动工作：按新鲜度决定是否抓取订阅源、是否复查健康状态。
    async fn initialize_inner(&self, options: Bootstrap) -> Result<()> {
        let now = SystemTime::now();

        if options.refresh == Freshness::Force
            || (options.refresh == Freshness::IfStale
                && !self.fresh(
                    self.fetched_at.load(Ordering::SeqCst),
                    self.config.refresh.interval,
                    now,
                ))
        {
            let summary = self.refresh().await?;
            info!(
                fetched = summary.fetched,
                added = summary.added,
                failed_subscribers = summary.failed,
                "subscriber refresh complete"
            );
        }

        let checked_at = self.checked_at.load(Ordering::SeqCst);
        let should_check = match options.check {
            Freshness::Force => true,
            Freshness::Never => false,
            Freshness::IfStale => !self.fresh(checked_at, self.config.health.interval, now),
            // 一次都没探过才算"缺"。`check` 即使池子是空的也会写
            // `checked_at`，所以空池不会让这里反复探测。
            Freshness::IfMissing => checked_at == 0,
        };
        if should_check {
            let report = self.check(false).await?;
            if report.checked > 0 {
                info!(report = %report.summary(), "initial health check complete");
            }
        }

        Ok(())
    }

    /// 某个时间戳是否还在 `interval` 之内（0 表示从未发生过）。
    fn fresh(&self, stamp: u64, interval: Duration, now: SystemTime) -> bool {
        if stamp == 0 {
            return false;
        }
        now.duration_since(state::from_unix_secs(stamp as i64))
            .map(|age| age < interval)
            .unwrap_or(true)
    }

    /// 冷启动的就绪状态。
    pub fn readiness(&self) -> &Arc<Readiness> {
        &self.readiness
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

        // 每个订阅源一完成就合并进池子并落盘，而不是等最慢的那个：
        // `freeproxy-gh` 要四分钟，那期间已经拿到的代理不该只存在内存里。
        let mut fetched = 0;
        let mut rejected = 0;
        let mut truncated = 0;
        let mut added = 0;
        let mut existing = 0;
        let mut fresh_ids: HashSet<ProxyId> = HashSet::new();

        let outcomes = self
            .subscribers
            .fetch_all_streaming(self.progress.as_ref(), |outcome| {
                if !outcome.ok() {
                    // 面向用户的失败报告由进度接收器负责（命令行的
                    // `✗ 名字 失败：…`、serve 的 LogProgress 警告）。
                    debug!(
                        subscriber = %outcome.name,
                        error = outcome.error.as_deref().unwrap_or("unknown error"),
                        "subscriber failed"
                    );
                    return;
                }

                fetched += outcome.count();
                rejected += outcome.rejected.len();
                truncated += outcome.truncated;
                for proxy in &outcome.proxies {
                    fresh_ids.insert(Proxy::id_of(proxy));
                }

                // 这一批有"有用的东西"就立刻落盘。
                let merged = self.pool.merge(outcome.proxies.clone());
                added += merged.added;
                existing += merged.existing;
                if merged.added > 0 {
                    self.save_cache();
                }

                debug!(
                    subscriber = %outcome.name,
                    found = outcome.count(),
                    rejected = outcome.rejected.len(),
                    skipped = outcome.skipped,
                    added = merged.added,
                    elapsed_ms = outcome.duration.as_millis() as u64,
                    "subscriber fetched"
                );
            })
            .await;

        let all_ok = !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.ok());
        let failed = outcomes.iter().filter(|outcome| !outcome.ok()).count();

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
            added,
            existing,
            removed,
            rejected,
            truncated,
            duration: started.elapsed(),
        })
    }

    /// 探测全部（或仅存活的）代理，并把结果写回代理池。
    pub async fn check(&self, alive_only: bool) -> Result<CheckReport> {
        if alive_only {
            self.check_where(|proxy| proxy.alive).await
        } else {
            self.check_where(|_| true).await
        }
    }

    /// 只探测还没有任何判定结果的代理——`refresh` 新抓到的那批。
    ///
    /// 刷新后再把整个池子重探一遍是浪费：老代理的结果还在有效期内，只有新
    /// 来的没有判定，而没判定就不能被发放。
    pub async fn check_pending(&self) -> Result<CheckReport> {
        self.check_where(|proxy| proxy.last_checked_at.is_none())
            .await
    }

    /// 探测满足 `keep` 的代理，并把结果写回代理池。
    async fn check_where(&self, keep: impl Fn(&Proxy) -> bool) -> Result<CheckReport> {
        let _guard = self.busy.lock().await;

        let mut proxies = self.pool.snapshot();
        proxies.retain(|proxy| keep(proxy));
        if proxies.is_empty() {
            debug!("nothing to check");
            return Ok(CheckReport::default());
        }

        let report = self
            .checker
            .check_and_apply_reporting(&self.pool, &proxies, self.progress.as_ref())
            .await;
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

    /// 请求后台立刻抓取一轮订阅源。
    ///
    /// 由 `POST /api/v1/refresh` 调用：刷新循环会提前醒来，而不是等到
    /// 下一个 `refresh.interval`。真正干活的是那个循环，所以这里没有并发
    /// 问题，也不需要第二份抓取逻辑。
    pub fn request_refresh(&self) {
        self.refresh_now.notify_one();
    }

    /// 请求后台立刻探测一轮代理池（`POST /api/v1/check`）。
    pub fn request_check(&self) {
        self.check_now.notify_one();
    }

    /// 发放一个代理：需要时先现探一次，探通了才交出去。
    ///
    /// 与 [`App::select`] 的区别只有一点——**保证交出去的这个刚刚是可用的**：
    ///
    /// * 判定比 `selection.max_age` 新（默认 60 秒）→ 直接用，毫秒级返回；
    /// * 判定旧了（或者从来没探过）→ 探它一次，通过就发；失败就把它标死，
    ///   再按轮换挑下一个，最多 `selection.verify_attempts` 次。
    ///
    /// 顺带一个副作用：每次验证都会把结果写回池子，所以代理池是被"用"干净
    /// 的，而不是只靠周期性的全量探测。
    pub async fn select_for_handout(&self, strategy: Strategy) -> Option<Selection> {
        let attempts = if self.config.selection.verify {
            self.config.selection.verify_attempts.max(1)
        } else {
            1
        };

        for attempt in 1..=attempts {
            let selection = self.select(strategy)?;

            if !self.needs_verification(&selection.proxy) {
                debug!(
                    proxy = %selection.proxy.to_masked_string(),
                    "handing out a proxy with a fresh verdict"
                );
                return Some(selection);
            }

            let started = Instant::now();
            let result = self.verify_checker.check_one(&selection.proxy).await;
            let now = SystemTime::now();

            if result.alive {
                self.pool
                    .record_success(&selection.proxy.id, result.latency, now);
                self.progress.verify(VerifyEvent {
                    proxy: &selection.proxy.to_masked_string(),
                    ok: true,
                    latency: Some(started.elapsed()),
                    error: None,
                    attempt,
                    attempts,
                });
                return Some(selection);
            }

            let error = result.error.unwrap_or_else(|| "unknown error".to_string());
            self.pool
                .record_failure(&selection.proxy.id, self.config.health.max_failures);
            self.progress.verify(VerifyEvent {
                proxy: &selection.proxy.to_masked_string(),
                ok: false,
                latency: None,
                error: Some(&error),
                attempt,
                attempts,
            });
        }

        None
    }

    /// 该代理是否需要现探一次才能发放。
    fn needs_verification(&self, proxy: &Proxy) -> bool {
        if !self.config.selection.verify {
            return false;
        }
        let max_age = self.config.selection.max_age;
        match proxy.last_checked_at {
            // 时钟回拨或未来时间戳：当作新鲜，别在这里较劲。
            Some(checked_at) => SystemTime::now()
                .duration_since(checked_at)
                .map(|age| age > max_age)
                .unwrap_or(false),
            None => true,
        }
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
                "the pool is empty and no subscribers are configured; add one to config.yaml and POST /api/v1/refresh".to_string()
            } else {
                format!(
                    "the pool is empty: {} subscriber(s) produced no usable proxy (POST /api/v1/refresh and watch the logs to see why)",
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

/// 把进度事件转成日志的接收器：`serve` 用它。
///
/// 命令行有更好看的终端进度（`commands::ConsoleProgress`），但后台循环没有
/// 终端，所以这里按 `info` 写日志——`serve` 的日志里因此能看到每个来源拿到
/// 了多少、慢的还在下多少。
#[derive(Debug, Default, Clone, Copy)]
pub struct LogProgress;

impl Progress for LogProgress {
    fn fetch(&self, event: FetchEvent<'_>) {
        match event {
            FetchEvent::Started { name, kind, format } => {
                info!(
                    subscriber = name,
                    kind,
                    format = format.as_str(),
                    "fetching subscriber"
                );
            }
            FetchEvent::Download {
                name,
                bytes,
                elapsed,
            } => {
                info!(
                    subscriber = name,
                    kilobytes = bytes / 1024,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "still downloading"
                );
            }
            FetchEvent::Finished(outcome) => {
                if outcome.ok() {
                    info!(
                        subscriber = %outcome.name,
                        found = outcome.count(),
                        rejected = outcome.rejected.len(),
                        skipped = outcome.skipped,
                        truncated = outcome.truncated,
                        elapsed_ms = outcome.duration.as_millis() as u64,
                        "subscriber fetched"
                    );
                } else {
                    warn!(
                        subscriber = %outcome.name,
                        error = outcome.error.as_deref().unwrap_or("unknown error"),
                        "subscriber failed"
                    );
                }
            }
        }
    }

    fn check(&self, event: CheckEvent) {
        info!(
            done = event.done,
            total = event.total,
            alive = event.alive,
            elapsed_ms = event.elapsed.as_millis() as u64,
            "health check progress"
        );
    }

    fn verify(&self, event: VerifyEvent<'_>) {
        if event.ok {
            info!(
                proxy = event.proxy,
                attempt = event.attempt,
                attempts = event.attempts,
                elapsed_ms = event.latency.map(|l| l.as_millis() as u64).unwrap_or(0),
                "hand-out verification passed"
            );
        } else {
            info!(
                proxy = event.proxy,
                attempt = event.attempt,
                attempts = event.attempts,
                error = event.error.unwrap_or("unknown error"),
                "hand-out verification failed; trying the next candidate"
            );
        }
    }
}

/// 关闭标志被置位（或发送端被丢弃）时完成。
pub(crate) async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    let _ = receiver.wait_for(|stop| *stop).await;
}

/// 冷启动与定期刷新共用的后台循环。
///
/// 循环开始时若还没就绪，它先当"初始化器"：`serve` 已经把端口挂上了，第一次
/// 抓取与探测在这里做。失败不会让 `serve` 退出——它会每
/// [`INITIALIZE_RETRY_INTERVAL`] 重试一次，而客户端拿到 `503` 时的
/// [`Readiness::request_init`] 会让它立刻醒来。
///
/// 就绪之后它按刷新间隔重新抓取订阅源并复查健康状态；若刷新带来了新代理，
/// 会立刻补一次健康检查，让它们先拿到判定结果，再被分发出去。
pub(crate) async fn refresh_loop(
    app: Arc<App>,
    shutdown: watch::Receiver<bool>,
    bootstrap: Bootstrap,
) {
    let mut ticker = tokio::time::interval(app.config.refresh.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一个 tick 立即完成；初始化由下面的循环自己做，先把它消耗掉。
    ticker.tick().await;

    loop {
        if app.readiness().is_ready() {
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
        } else {
            match app
                .initialize_throttled(bootstrap, INITIALIZE_RETRY_INTERVAL)
                .await
            {
                Ok(()) if app.readiness().is_ready() => info!(
                    proxies = app.pool.len(),
                    alive = app.pool.stats().alive,
                    "proxygate is ready"
                ),
                // 已有任务在做，或距上次尝试还不到重试下限。
                Ok(()) => debug!("initialization attempt skipped"),
                Err(error) => warn!(
                    error = %error,
                    retry_in_seconds = INITIALIZE_RETRY_INTERVAL.as_secs(),
                    "initialization failed; the API keeps answering 503"
                ),
            }
        }

        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = ticker.tick() => {}
            // `POST /api/v1/refresh`：客户端要求现在就来一轮。
            _ = app.refresh_now.notified() => {
                info!("refresh requested through the API");
            }
            // 未就绪时，客户端的一次 503 就足以让我们提前醒来重试。
            _ = app.readiness().wait_for_request(INITIALIZE_RETRY_INTERVAL),
                if !app.readiness().is_ready() => {}
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
            _ = ticker.tick() => {}
            // `POST /api/v1/check`：客户端要求现在就来一轮。
            _ = app.check_now.notified() => {
                info!("health check requested through the API");
            }
        }

        match app.check(false).await {
            Ok(report) => info!(report = %report.summary(), "health check complete"),
            Err(error) => warn!(error = %error, "health check failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;

    use crate::config::SubscriberConfig;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 一个假的"上游代理"：只接受连接并立刻关掉，但会数一共接受了几次。
    ///
    /// 健康探测无论如何都要先连上游，所以探测次数就等于这里的连接数——比去
    /// 数探测目标可靠得多（探测目标在单元测试里根本连不到）。
    async fn counting_proxy() -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });

        (address, hits)
    }

    /// 一个慢速 HTTP 服务器：`delay` 之后才回响应体。
    async fn slow_http_server(body: &'static str, delay: Duration) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    let _ = stream.read(&mut request).await;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        address
    }

    /// 每个测试一个独立的、干净的缓存目录。
    ///
    /// 同一个测试名在多次运行之间会复用路径，所以要先删掉：残留的
    /// `cache.json` 会让"冷启动"不再冷。
    fn scratch_config(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("proxygate-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = Config::default();
        config.state.dir = Some(dir);
        config
    }

    fn file_subscriber(name: &str, path: std::path::PathBuf) -> SubscriberConfig {
        SubscriberConfig::File {
            name: name.to_string(),
            path,
            format: crate::config::Format::Plaintext,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn health_is_not_probed_again_once_there_are_results() {
        let (proxy, hits) = counting_proxy().await;

        let dir =
            std::env::temp_dir().join(format!("proxygate-app-pending-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let list = dir.join("list.txt");
        std::fs::write(&list, format!("{proxy}\n")).expect("write list");

        // 每次"命令行调用"都是一个新进程：这里就是新的 App，共用同一个缓存
        // 目录，所以下一次能从 `cache.json` 里拿到上一次的判定结果。
        // 目录只清一次——`scratch_config` 会删目录，不能每次调用都建。
        let mut base = scratch_config("missing");
        base.subscribers = vec![file_subscriber("local", list.clone())];
        base.health.target = Some("http://example.test/".to_string());
        base.health.targets = None;
        base.health.timeout = Duration::from_millis(500);
        base.health.concurrency = 1;
        let config = || base.clone();

        // 1. 冷启动：池子是空的、也没有任何判定结果，所以既抓又探。
        let first = App::new(config(), None).expect("app");
        first
            .initialize(Bootstrap {
                refresh: Freshness::Force,
                check: Freshness::IfMissing,
            })
            .await
            .expect("initialize");
        let after_cold = hits.load(Ordering::SeqCst);
        assert!(after_cold > 0, "冷启动应该探过一次");

        // 2. 第二次调用（新进程，缓存里有结果）：不该再探整个池子。
        //    这正是 `get` / `list` 之前的行为——过了 health.interval 就重探
        //    一遍全池，几千个代理要几分钟。
        let second = App::new(config(), None).expect("app");
        second
            .initialize(Bootstrap {
                refresh: Freshness::Never,
                check: Freshness::IfMissing,
            })
            .await
            .expect("initialize");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            after_cold,
            "有判定结果之后，`get` / `list` 不该重探"
        );

        // 3. 显式 `--check` / `check`：必须真的重探。
        let third = App::new(config(), None).expect("app");
        assert_eq!(
            third.pool.len(),
            1,
            "上一次的代理应该从 cache.json 恢复出来"
        );
        third
            .initialize(Bootstrap {
                refresh: Freshness::Never,
                check: Freshness::Force,
            })
            .await
            .expect("initialize");
        assert!(
            hits.load(Ordering::SeqCst) > after_cold,
            "--check 必须真的重探"
        );
    }

    #[tokio::test]
    async fn refresh_probes_only_the_proxies_it_has_no_verdict_for() {
        let (proxy, _hits) = counting_proxy().await;

        let mut config = scratch_config("pending");
        config.health.target = Some("http://example.test/".to_string());
        config.health.targets = None;
        config.health.timeout = Duration::from_millis(500);
        config.health.concurrency = 1;
        let app = App::new(config, None).expect("app");

        // 一个已经探过（有判定结果），一个还没有。
        let (known, _) = app
            .pool
            .insert(model::normalize(&format!("http://{proxy}")).expect("proxy url"));
        app.pool.update_health(&[(
            known.clone(),
            crate::pool::HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(10)),
                checked_at: SystemTime::now(),
                probes: vec![ProbeOutcome {
                    target: Arc::from("http://example.test/"),
                    ok: true,
                    latency: Some(Duration::from_millis(10)),
                }],
            },
        )]);
        let _ = app
            .pool
            .insert(model::normalize("http://127.0.0.1:9").expect("proxy url"));

        // 只探"没有判定"的那一个：`refresh` 之后补探的就是这些。
        let report = app.check_pending().await.expect("check_pending");
        assert_eq!(report.checked, 1, "{report:?}");
    }

    #[tokio::test]
    async fn refresh_persists_each_subscriber_as_it_arrives() {
        let address = slow_http_server("9.9.9.9:9999\n", Duration::from_millis(1200)).await;

        let dir =
            std::env::temp_dir().join(format!("proxygate-app-incremental-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let list = dir.join("fast.txt");
        std::fs::write(&list, "1.1.1.1:1111\n").expect("write list");

        let mut config = scratch_config("incremental");
        config.subscribers = vec![
            file_subscriber("fast", list),
            SubscriberConfig::Http {
                name: "slow".to_string(),
                url: format!("http://{address}/list.txt"),
                format: crate::config::Format::Plaintext,
                headers: BTreeMap::new(),
                timeout: Some(Duration::from_secs(10)),
                enabled: true,
            },
        ];
        let cache = config.cache_dir().join("cache.json");

        let app = Arc::new(App::new(config, None).expect("app"));
        let refreshing = tokio::spawn({
            let app = app.clone();
            async move { app.refresh().await }
        });

        // 快源已经落地了，慢源（1.2 秒）还挂着——这正是"有用的先写盘"。
        let deadline = Instant::now() + Duration::from_millis(1000);
        let mut found = false;
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&cache) {
                if text.contains("1.1.1.1:1111") {
                    found = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(found, "快源拿到的代理应该在慢源完成之前就已经落盘");
        assert!(
            !std::fs::read_to_string(&cache)
                .unwrap_or_default()
                .contains("9.9.9.9:9999"),
            "慢源此刻还没完成，不该已经写进去了"
        );

        let summary = refreshing.await.expect("refresh task").expect("refresh");
        assert_eq!(summary.added, 2);
        let text = std::fs::read_to_string(&cache).expect("cache");
        assert!(text.contains("9.9.9.9:9999"), "{text}");
    }
}
