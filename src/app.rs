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
    /// "池子里的探测计划变了，重新算一下该睡多久"。
    ///
    /// 健康循环睡多久是**算出来的**（最近一个到期探测还差多久）。如果它先睡
    /// 下、池子随后才被填上（初始化或刷新），那一觉就睡错了——所以任何改变
    /// 计划的地方都会敲一下这个通知。
    health_reschedule: Notify,
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
            pool.restore_health(&entries, &config.health.policy());
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
            health_reschedule: Notify::new(),
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
        // 慢的来源要几十秒，那期间已经拿到的代理不该只存在内存里。
        let mut fetched = 0;
        let mut rejected = 0;
        let mut truncated = 0;
        let mut added = 0;
        let mut existing = 0;
        let mut fresh_ids: HashSet<ProxyId> = HashSet::new();

        let outcomes = self
            .subscribers
            .fetch_all_streaming(self.progress.as_ref(), |outcome| {
                crate::metrics::record_subscriber(outcome.ok(), outcome.count());
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
    ///
    /// "全量"只用在两处：首次初始化，以及 `POST /api/v1/check` 这样的显式请求。
    /// 后台循环走 [`App::check_due`]，只探到点的那些。
    pub async fn check(&self, alive_only: bool) -> Result<CheckReport> {
        if alive_only {
            self.check_where(|proxy| proxy.alive).await
        } else {
            self.check_where(|_| true).await
        }
    }

    /// 只探测到点的代理（`health.backoff_*` 决定谁到点了）。
    ///
    /// 这是后台循环用的版本：免费代理池里绝大多数条目是死的，按成功代理的
    /// 节奏重探它们只是浪费带宽和 fd。失败退避、成功按 `health.interval`。
    pub async fn check_due(&self) -> Result<CheckReport> {
        let now = SystemTime::now();
        let due = self.pool.due(now);
        if due.is_empty() {
            return Ok(CheckReport::default());
        }
        self.check_proxies(due).await
    }

    /// 距离下一个到期探测还有多久；池子为空时返回 `None`。
    ///
    /// 健康循环用它决定睡多久，而不是固定按 `health.interval` 醒来。
    pub fn next_check_in(&self) -> Option<Duration> {
        self.pool.next_check_in(SystemTime::now())
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
        let mut proxies = self.pool.snapshot();
        proxies.retain(|proxy| keep(proxy));
        self.check_proxies(proxies).await
    }

    /// 探测给定的一批代理，并把结果写回代理池。
    async fn check_proxies(&self, proxies: Vec<Proxy>) -> Result<CheckReport> {
        let _guard = self.busy.lock().await;
        if proxies.is_empty() {
            debug!("nothing to check");
            return Ok(CheckReport::default());
        }

        let report = self
            .checker
            .check_and_apply_reporting(&self.pool, &proxies, self.progress.as_ref())
            .await;
        // 探测的成功/失败比是判断"这个池子值不值得留着"的第一手数据。
        crate::metrics::record_check(report.alive, report.dead);
        self.checked_at.store(
            state::unix_secs(SystemTime::now()).max(0) as u64,
            Ordering::SeqCst,
        );
        self.save_cache();
        // 这一轮改写了每个代理的下次探测时间，唤醒循环重算。
        self.reschedule_health();
        Ok(report)
    }

    /// 挑选一个代理，并记录本次使用。
    pub fn select(&self, strategy: Strategy) -> Option<Selection> {
        let mut options = self.config.selection_options();
        // `App::select` 的调用方（API、发放验证的候选轮换）只关心策略本身。
        options.strategy = strategy;
        let selection = self.pool.select(options, SystemTime::now())?;

        tracing::debug!(
            proxy = %selection.proxy.to_masked_string(),
            round = selection.round,
            reset_round = selection.reset_round,
            candidates = selection.candidates,
            "selected proxy"
        );

        self.persist_after_rotation();
        Some(selection)
    }

    /// 让健康循环重新计算下一次唤醒时间。
    ///
    /// 由任何改动"谁该在什么时候被探"的地方调用：初始化、刷新、以及每完成
    /// 一轮探测。
    fn reschedule_health(&self) {
        self.health_reschedule.notify_one();
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
        // `/api/v1/get` 是最高频的端点，所以它的耗时（含现探）单独记一条。
        let started = Instant::now();
        let result = self.select_for_handout_inner(strategy).await;
        crate::metrics::record_get(strategy.as_str(), result.is_some(), started.elapsed());
        result
    }

    /// [`App::select_for_handout`] 的主体，指标在调用方记，失败路径也一样。
    async fn select_for_handout_inner(&self, strategy: Strategy) -> Option<Selection> {
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
                crate::metrics::record_verify("fresh");
                return Some(selection);
            }

            let started = Instant::now();
            let result = self.verify_checker.check_one(&selection.proxy).await;
            let now = SystemTime::now();

            if result.alive {
                crate::metrics::record_verify("ok");
                self.pool.record_success(
                    &selection.proxy.id,
                    result.latency,
                    now,
                    &self.config.health.policy(),
                );
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
            crate::metrics::record_verify("fail");
            self.pool.record_failure(
                &selection.proxy.id,
                &self.config.health.policy(),
                SystemTime::now(),
            );
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

    /// `/get` 与网关在轮换之后调用：把轮换状态写盘，但**不在请求线程上写**。
    ///
    /// 节流判断留在原地（原子、纳秒级），真正的文件 I/O 交给阻塞线程池——
    /// `std::fs::write` 是同步的，池子大时一次几毫秒到几十毫秒，堵在异步任务
    /// 里就等于堵住 reactor。没有 tokio 运行时（库的使用者直接调 `select`）
    /// 时退回同步写，行为与以前一致。
    fn persist_after_rotation(&self) {
        let now = SystemTime::now();
        if !self.store.claim_write(now, PERSIST_INTERVAL) {
            return;
        }

        let usage = self.pool.usage();
        let generation = self.pool.generation();
        let store = self.store.clone();

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || match store.persist_usage(usage, generation, now) {
                    Ok(_) => crate::metrics::record_state_save(true),
                    Err(error) => {
                        crate::metrics::record_state_save(false);
                        warn!(error = %error, "cannot persist rotation state");
                    }
                });
            }
            Err(_) => match store.persist_usage(usage, generation, now) {
                Ok(_) => crate::metrics::record_state_save(true),
                Err(error) => {
                    crate::metrics::record_state_save(false);
                    warn!(error = %error, "cannot persist rotation state");
                }
            },
        }
    }

    /// 写入轮换状态，可选择绕过节流。
    ///
    /// 同步版本：关机和缓存写入用它，那里需要一个确定的结果。
    pub fn persist(&self, force: bool) -> Result<bool> {
        let now = SystemTime::now();
        let result = if force {
            self.store.persist(&self.pool, now).map(|_| true)
        } else {
            self.store
                .persist_throttled(&self.pool, now, PERSIST_INTERVAL)
        };
        match result {
            Ok(wrote) => {
                // `false` 表示被节流跳过：那不是一次写盘，不计数。
                if wrote {
                    crate::metrics::record_state_save(true);
                }
                Ok(wrote)
            }
            Err(error) => {
                crate::metrics::record_state_save(false);
                Err(error)
            }
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
/// 库只发进度事件，这里把它们写成 `info` 级日志，所以服务端日志能看到每个
/// 来源跑了多久、拿到多少，以及健康探测与发放验证的进行情况。
#[derive(Debug, Default, Clone, Copy)]
pub struct LogProgress;

impl Progress for LogProgress {
    fn fetch(&self, event: FetchEvent<'_>) {
        match event {
            FetchEvent::Started { name } => {
                info!(subscriber = %name, "running subscriber script");
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
                proxy = %event.proxy,
                attempt = event.attempt,
                attempts = event.attempts,
                elapsed_ms = event.latency.map(|l| l.as_millis() as u64).unwrap_or(0),
                "hand-out verification passed"
            );
        } else {
            info!(
                proxy = %event.proxy,
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

/// 睡多久就得醒一次的上限。
///
/// 池子里最近的一个到期探测可能还在 30 分钟后；睡那么久会让"空转醒来看看"
/// 变得不灵敏（`POST /api/v1/check` 走的是另一条通知路径，不受影响）。30 秒
/// 醒来扫一遍几千个条目是微秒级的开销。
const HEALTH_MAX_SLEEP: Duration = Duration::from_secs(30);

/// 两次唤醒之间的最短间隔，防止退避到 0 时把 CPU 打满。
const HEALTH_MIN_SLEEP: Duration = Duration::from_millis(50);

/// 按每个代理自己的退避计划重新探测健康状态。
///
/// 旧版按 `health.interval` 固定唤醒并**全量重探**：1,019 条池子里只有 9 条
/// 可能成功，剩下 99.1% 的探测是白花的（实测数据见 `BENCHMARKS.md`）。现在
/// 每次醒来只探"到点"的那些——成功的按 `health.interval` 保鲜，失败的按
/// `health.backoff_*` 指数退避，稳定之后每小时探测量降一个数量级。
pub(crate) async fn health_loop(app: Arc<App>, shutdown: watch::Receiver<bool>) {
    loop {
        let wait = app
            .next_check_in()
            .unwrap_or(HEALTH_MAX_SLEEP)
            .clamp(HEALTH_MIN_SLEEP, HEALTH_MAX_SLEEP);

        let forced = tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = tokio::time::sleep(wait) => false,
            // `POST /api/v1/check`：客户端要求现在就来一轮（全量，不看退避）。
            _ = app.check_now.notified() => true,
            // 计划变了（初始化完成、刷新进了新代理、上一轮刚写完下次探测时间）：
            // 别继续睡那个已经过时的时长，回去重算。
            _ = app.health_reschedule.notified() => continue,
        };

        let result = if forced {
            info!("health check requested through the API");
            app.check(false).await
        } else {
            app.check_due().await
        };

        match result {
            // 全量轮次值得一行 info；到点的那种每几秒就有一次，记 debug。
            Ok(report) if forced => {
                info!(report = %report.summary(), "health check complete")
            }
            Ok(report) if report.checked > 0 => {
                debug!(report = %report.summary(), "due health checks complete")
            }
            Ok(_) => {}
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

    /// 一个最短的订阅源脚本：直接返回 `host:port` 形式的代理。
    fn lua_subscriber(name: &str, host: &str, port: u16) -> SubscriberConfig {
        SubscriberConfig {
            name: name.to_string(),
            lua_code: Some(format!(
                "return {{ {{ type = 'http', ip = '{host}', port = {port} }} }}"
            )),
            lua_file: None,
            timeout: None,
            limit: None,
            enabled: true,
            params: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn health_is_not_probed_again_once_there_are_results() {
        let (proxy, hits) = counting_proxy().await;

        let (host, port) = (proxy.ip().to_string(), proxy.port());

        // 每次"命令行调用"都是一个新进程：这里就是新的 App，共用同一个缓存
        // 目录，所以下一次能从 `cache.json` 里拿到上一次的判定结果。
        // 目录只清一次——`scratch_config` 会删目录，不能每次调用都建。
        let mut base = scratch_config("missing");
        base.subscribers = vec![lua_subscriber("local", &host, port)];
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

        let mut config = scratch_config("incremental");
        // 慢源靠一次慢 `fetch` 拖住；Lua 里没有 sleep，所以用真实的网络等待。
        let slow = SubscriberConfig {
            name: "slow".to_string(),
            lua_code: Some(
                "local _ = fetch(slow_url)\n\
                 return { { type = 'http', ip = '9.9.9.9', port = 9999 } }"
                    .to_string(),
            ),
            lua_file: None,
            timeout: Some(Duration::from_secs(10)),
            limit: None,
            enabled: true,
            params: BTreeMap::from([(
                "slow_url".to_string(),
                serde_yaml::Value::from(format!("http://{address}/list.txt")),
            )]),
        };
        config.subscribers = vec![lua_subscriber("fast", "1.1.1.1", 1111), slow];
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

#[cfg(test)]
mod backoff_tests {
    use super::*;
    use crate::config::SubscriberConfig;

    /// 一个订阅源脚本，返回一条指向本地死端口的代理。
    fn dead_subscriber() -> SubscriberConfig {
        SubscriberConfig {
            name: "one".into(),
            lua_code: Some("return { { type = 'http', ip = '127.0.0.1', port = 1 } }".into()),
            lua_file: None,
            timeout: None,
            limit: None,
            enabled: true,
            params: Default::default(),
        }
    }

    fn scratch(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("proxygate-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = Config::default();
        config.state.dir = Some(dir);
        config.health.targets = Some(vec!["http://127.0.0.1:1/".into()]);
        config.health.timeout = Duration::from_millis(200);
        config.subscribers = vec![dead_subscriber()];
        config
    }

    #[tokio::test]
    async fn a_failing_proxy_is_only_probed_when_it_is_due() {
        let mut config = scratch("due");
        config.health.interval = Duration::from_secs(60);
        config.health.backoff_base = Duration::from_millis(200);
        config.health.backoff_max = Duration::from_millis(800);

        let app = Arc::new(App::new(config, None).expect("app"));
        app.refresh().await.expect("refresh");
        let first = app.check(false).await.expect("first pass");
        assert_eq!(first.checked, 1, "the first pass probes everything");

        // 刚探完：立刻再来一次"到点探测"应该是空的。
        assert!(app.pool.due(SystemTime::now()).is_empty());
        assert_eq!(app.check_due().await.expect("due").checked, 0);
        let wait = app.next_check_in().expect("scheduled");
        // 第一次退避 = base × 2^1 = 400ms（毫秒精度，留点余量）。
        assert!(wait <= Duration::from_millis(450), "{wait:?}");

        // 等过第一次退避。
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            app.check_due().await.expect("due").checked,
            1,
            "the failing proxy must be retried once its backoff elapses"
        );

        // 第二次退避更长（800ms 上限）：300ms 后还不该到点。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(app.check_due().await.expect("due").checked, 0);
    }

    #[tokio::test]
    async fn a_healthy_proxy_waits_for_the_plain_interval() {
        let mut config = scratch("healthy");
        config.health.interval = Duration::from_secs(600);
        config.health.backoff_base = Duration::from_millis(100);
        config.subscribers = vec![SubscriberConfig {
            name: "ok".into(),
            lua_code: Some("return {}".into()),
            lua_file: None,
            timeout: None,
            limit: None,
            enabled: true,
            params: Default::default(),
        }];
        let app = Arc::new(App::new(config, None).expect("app"));
        app.refresh().await.expect("refresh");
        app.pool
            .insert(crate::model::normalize("http://127.0.0.1:1").expect("proxy url"));

        let stats = app.pool.apply_health_pass(
            &[(
                app.pool.snapshot()[0].id.clone(),
                crate::pool::HealthUpdate {
                    alive: true,
                    latency: Some(Duration::from_millis(5)),
                    checked_at: SystemTime::now(),
                    probes: Vec::new(),
                },
            )],
            &app.config.health.policy(),
        );
        assert_eq!(stats.alive, 1);

        // 健康的代理按 `health.interval` 排期，而不是退避的几百毫秒。
        let wait = app.next_check_in().expect("scheduled");
        assert!(wait > Duration::from_secs(500), "{wait:?}");
        assert!(app.pool.due(SystemTime::now()).is_empty());
    }
}
