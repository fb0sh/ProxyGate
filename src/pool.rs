//! 代理池。
//!
//! 读路径（`/api/v1/get`、网关）走**不可变快照**：池子持有一个
//! [`ArcSwap`] 指向 `Vec<Arc<Entry>>`，读到的是共享的 `Arc`，既不拿锁也不
//! 复制代理。写路径分两类：
//!
//! * **结构变化**（合并订阅源结果、按 id 集合裁剪）拿到索引的写锁，改完重建
//!   一次快照；
//! * **事实变化**（健康判定、失败计数、轮换标记）只改条目里的原子量，**不碰
//!   锁，也不重建快照**。
//!
//! 之所以这么分：健康探测每轮要写几千次（改事实），而 `/get` 是高频读。旧版
//! 把两者塞进同一个 `RwLock<HashMap>`，`/get` 还得为跑选择器把整个池子克隆一遍
//! ——5,000 条时每次请求约 1.7ms 的持锁时间把吞吐压到 ~600 QPS（实测数据在
//! `BENCHMARKS.md`）。现在 `/get` 的代价是遍历一遍共享的 `Arc` 列表。
//!
//! 轮换的"一轮内不重复分发"靠条目上的 CAS 认领保证：抢不到就试下一个候选，
//! 语义与旧的写锁版本一致（池子只有一个健康代理时，多出来的并发请求必然拿到
//! 同一个代理——旧版在推进轮次之后也是如此）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use url::Url;

use crate::model::{ProbeOutcome, Proxy, ProxyId};
use crate::selector::{self, Candidate, Plan, SelectionOptions};

/// 轮次编号从 1 开始，这样新插入的代理（`generation` 为 0）
/// 会被视为“尚未使用”。
pub const FIRST_GENERATION: u64 = 1;

/// 把一批 URL 合并进代理池的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// 新增的代理数量。
    pub added: usize,
    /// 已存在并被更新的代理数量。
    pub existing: usize,
}

impl MergeStats {
    /// 本次合并处理的 URL 总数。
    pub fn total(&self) -> usize {
        self.added + self.existing
    }
}

/// 一次健康检查后写回的健康事实。
#[derive(Debug, Clone)]
pub struct HealthUpdate {
    /// 本次检查是否成功。
    pub alive: bool,
    /// 本次检查测量到的延迟。
    pub latency: Option<Duration>,
    /// 本次检查的时间。
    pub checked_at: SystemTime,
    /// 产生本次更新的那一轮检查的逐目标结果。
    pub probes: Vec<ProbeOutcome>,
}

/// 从磁盘缓存读回的健康事实（绝对值，不做计数）。
#[derive(Debug, Clone)]
pub struct HealthRestore {
    /// 缓存中记录的存活状态。
    pub alive: bool,
    /// 缓存中记录的延迟。
    pub latency: Option<Duration>,
    /// 缓存中记录的连续失败次数。
    pub failures: u32,
    /// 缓存中记录的最后检查时间。
    pub checked_at: Option<SystemTime>,
    /// 缓存中记录的逐目标探测结果。
    pub probes: Vec<ProbeOutcome>,
}

/// [`ProxyPool::select`] 分发出去的代理。
#[derive(Debug, Clone)]
pub struct Selection {
    /// 被选中的代理。
    pub proxy: Proxy,
    /// 该代理被分配到的轮次。
    pub round: u64,
    /// 本次选择是否开启了新的轮次。
    pub reset_round: bool,
    /// 做出该选择时代理池中的健康代理数量。
    pub healthy: usize,
    /// 本次选择考虑过的候选数量。
    pub candidates: usize,
}

/// 代理池的规模统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolStats {
    /// 代理总数。
    pub total: usize,
    /// 健康代理数量。
    pub alive: usize,
    /// 死亡代理数量。
    pub dead: usize,
}

/// 池子里的一个代理：结构不可变，事实全在原子量里。
///
/// 「结构」指标识符和 URL——它们一旦进池子就不会变；「事实」指健康、延迟、
/// 失败次数、轮换标记，这些会被健康探测和分发频繁修改。放在原子量里，读路径
/// 才能在不拿锁的情况下看到最新值。
#[derive(Debug)]
pub struct Entry {
    /// 由规范化 URL 派生的稳定标识符。
    id: ProxyId,
    /// 归一化后的代理 URL，可能带凭据。
    url: Url,
    /// 最近一次探测/使用是否成功。
    alive: AtomicBool,
    /// 最近一次测量到的延迟（毫秒）；[`UNKNOWN_LATENCY_MS`] 表示未知。
    latency_ms: AtomicU64,
    /// 连续失败次数。
    failures: AtomicU32,
    /// 最近一次判定时间（Unix 毫秒，`0` 表示从未判定）。
    checked_at_ms: AtomicU64,
    /// 最近一次被分发到的轮次（`0` 表示从未分发）。
    round: AtomicU64,
    /// 最近一次被分发的时间（Unix 毫秒，`0` 表示从未分发）。
    used_at_ms: AtomicU64,
    /// 下一次该被探测的时间（Unix 毫秒，`0` 表示随时可探）。
    next_check_at_ms: AtomicU64,
    /// 最近一轮健康检查里每个目标的探测结果。写（健康检查）与读（API、持久化）
    /// 都很稀少，用一把小锁比继续塞原子量清楚。
    probes: Mutex<Vec<ProbeOutcome>>,
}

/// [`Entry::latency_ms`] 里表示"未知"的值。
const UNKNOWN_LATENCY_MS: u64 = u64::MAX;

/// 健康探测的节奏：什么时候重探一个代理，以及连续失败几次算死。
///
/// 由 `config.health` 翻译而来（[`crate::config::HealthConfig::policy`]），
/// 池子只负责照着它给每个条目算下一次探测时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    /// 原本可用的代理连续失败多少次后判死。
    pub max_failures: u32,
    /// 探测成功之后隔多久再探（就是 `health.interval`）。
    pub ok_delay: Duration,
    /// 失败后退避的起点，之后每次翻倍。
    pub backoff_base: Duration,
    /// 失败退避的上限。
    pub backoff_max: Duration,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            max_failures: 3,
            ok_delay: Duration::from_secs(300),
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(1800),
        }
    }
}

impl HealthPolicy {
    /// 一次探测之后，隔多久再探它。
    ///
    /// 成功就按 [`HealthPolicy::ok_delay`]（判定要保鲜）；失败按 2 的幂退避：
    /// 死代理第一次失败后 5s 再看一眼，然后 10s、20s……一直退到
    /// [`HealthPolicy::backoff_max`]。免费池子里 99% 的条目是死的，让它们
    /// 按成功代理的节奏重探纯属浪费（见 `BENCHMARKS.md`）。
    pub fn next_delay(&self, alive: bool, failures: u32) -> Duration {
        if alive {
            return self.ok_delay;
        }
        let shift = failures.min(16);
        let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.backoff_base
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

impl Entry {
    /// 从一个普通代理构造条目。
    fn from_proxy(proxy: Proxy) -> Self {
        Self {
            id: proxy.id,
            url: proxy.url,
            alive: AtomicBool::new(proxy.alive),
            latency_ms: AtomicU64::new(match proxy.latency {
                Some(latency) => latency.as_millis().min(u64::MAX as u128) as u64,
                None => UNKNOWN_LATENCY_MS,
            }),
            failures: AtomicU32::new(proxy.failures),
            checked_at_ms: AtomicU64::new(millis(proxy.last_checked_at)),
            round: AtomicU64::new(proxy.generation),
            used_at_ms: AtomicU64::new(millis(proxy.last_used_at)),
            // 从缓存/持久化恢复出来的代理按"立刻可探"处理；真正决定要不要探的是
            // `restore_health` 之后的退避计划。
            next_check_at_ms: AtomicU64::new(0),
            probes: Mutex::new(proxy.probes),
        }
    }

    /// 标识符。
    pub fn id(&self) -> &ProxyId {
        &self.id
    }

    /// 归一化后的代理 URL。
    pub fn url(&self) -> &Url {
        &self.url
    }

    fn is_alive(&self) -> bool {
        // `alive` 是唯一带正确性含义的标志（别把死代理发出去），所以
        // 用 Acquire/Release 配对；延迟、失败次数、轮次都是启发式数据，
        // Relaxed 足够，也省掉不必要的屏障。
        self.alive.load(Ordering::Acquire)
    }

    fn latency(&self) -> Option<Duration> {
        match self.latency_ms.load(Ordering::Relaxed) {
            UNKNOWN_LATENCY_MS => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    fn last_used_at(&self) -> Option<SystemTime> {
        from_millis(self.used_at_ms.load(Ordering::Relaxed))
    }

    fn checked_at(&self) -> Option<SystemTime> {
        from_millis(self.checked_at_ms.load(Ordering::Relaxed))
    }

    /// 重新探测的结果：`alive`、延迟、判定时间与下一次探测时间。
    fn apply_probe(
        &self,
        alive: bool,
        latency: Option<Duration>,
        checked_at: SystemTime,
        probes: Option<&Vec<ProbeOutcome>>,
        policy: &HealthPolicy,
    ) {
        self.checked_at_ms
            .store(millis(Some(checked_at)), Ordering::Relaxed);
        if let Some(probes) = probes {
            *self.probes.lock().unwrap_or_else(PoisonError::into_inner) = probes.clone();
        }

        let failures = if alive {
            self.alive.store(true, Ordering::Release);
            self.failures.store(0, Ordering::Relaxed);
            if let Some(latency) = latency {
                self.set_latency(latency);
            }
            0
        } else {
            let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
            // 从未成功过的代理一次失败即判死；原本可用的按 `max_failures` 容忍。
            let threshold = policy.max_failures.max(1);
            let was_alive = self.alive.load(Ordering::Acquire);
            self.alive
                .store(was_alive && failures < threshold, Ordering::Release);
            failures
        };

        self.schedule_next_check(checked_at, alive, failures, policy);
    }

    /// 记下"下一次该探它"的时刻。
    fn schedule_next_check(
        &self,
        from: SystemTime,
        alive: bool,
        failures: u32,
        policy: &HealthPolicy,
    ) {
        let delay = policy.next_delay(alive, failures);
        self.next_check_at_ms
            .store(millis(Some(from + delay)), Ordering::Relaxed);
    }

    /// 下一次该探测的时间；`None` 表示从没安排过（随时可探）。
    pub fn next_check_at(&self) -> Option<SystemTime> {
        from_millis(self.next_check_at_ms.load(Ordering::Relaxed))
    }

    /// 现在是否到了该探测它的时间。
    pub fn is_due(&self, now: SystemTime) -> bool {
        match self.next_check_at() {
            Some(at) => at <= now,
            None => true,
        }
    }

    fn set_latency(&self, latency: Duration) {
        self.latency_ms.store(
            latency.as_millis().min(u64::MAX as u128 - 1) as u64,
            Ordering::Relaxed,
        );
    }

    /// 记一次成功（健康检查之外，网关在请求成功时也会调）。
    fn record_success(
        &self,
        latency: Option<Duration>,
        checked_at: SystemTime,
        policy: &HealthPolicy,
    ) {
        self.alive.store(true, Ordering::Release);
        self.failures.store(0, Ordering::Relaxed);
        if let Some(latency) = latency {
            self.set_latency(latency);
        }
        self.checked_at_ms
            .store(millis(Some(checked_at)), Ordering::Relaxed);
        self.schedule_next_check(checked_at, true, 0, policy);
    }

    /// 记一次失败，返回更新后的存活标志。
    fn record_failure(&self, policy: &HealthPolicy, now: SystemTime) -> bool {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        let threshold = policy.max_failures.max(1);
        let was_alive = self.alive.load(Ordering::Acquire);
        let alive = was_alive && failures < threshold;
        self.alive.store(alive, Ordering::Release);
        self.schedule_next_check(now, alive, failures, policy);
        alive
    }

    /// 还原持久化/缓存里的健康事实（文件里的值就是权威，不做失败计数推断）。
    ///
    /// 同时按退避计划算出下一次探测时间，这样重启不会把所有死代理一次性重探。
    fn restore_health(&self, restore: &HealthRestore, policy: &HealthPolicy) {
        self.alive.store(restore.alive, Ordering::Release);
        match restore.latency {
            Some(latency) => self.set_latency(latency),
            None => self.latency_ms.store(UNKNOWN_LATENCY_MS, Ordering::Relaxed),
        }
        self.failures.store(restore.failures, Ordering::Relaxed);
        self.checked_at_ms
            .store(millis(restore.checked_at), Ordering::Relaxed);
        *self.probes.lock().unwrap_or_else(PoisonError::into_inner) = restore.probes.clone();
        if let Some(checked_at) = restore.checked_at {
            self.schedule_next_check(checked_at, restore.alive, restore.failures, policy);
        }
    }

    /// 还原轮换信息。
    fn restore_usage(&self, round: u64, used_at: Option<SystemTime>) {
        self.round.store(round, Ordering::Relaxed);
        self.used_at_ms.store(millis(used_at), Ordering::Relaxed);
    }

    /// 复制成普通代理（API、持久化、缓存用；不在热点路径上）。
    fn to_proxy(&self) -> Proxy {
        Proxy {
            id: self.id.clone(),
            url: self.url.clone(),
            alive: self.is_alive(),
            latency: self.latency(),
            failures: self.failures.load(Ordering::Relaxed),
            probes: self
                .probes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            generation: self.round.load(Ordering::Relaxed),
            last_used_at: self.last_used_at(),
            last_checked_at: self.checked_at(),
        }
    }
}

impl Candidate for Arc<Entry> {
    fn is_alive(&self) -> bool {
        Entry::is_alive(self)
    }

    fn round(&self) -> u64 {
        self.round.load(Ordering::Relaxed)
    }

    fn latency(&self) -> Option<Duration> {
        Entry::latency(self)
    }

    fn last_used_at(&self) -> Option<SystemTime> {
        Entry::last_used_at(self)
    }

    fn failures(&self) -> u32 {
        self.failures.load(Ordering::Relaxed)
    }
}

/// 线程安全的代理集合。
#[derive(Debug)]
pub struct ProxyPool {
    /// 按标识符索引的条目，只在结构变化与按 id 写入时拿锁。
    index: RwLock<HashMap<ProxyId, Arc<Entry>>>,
    /// 读路径用的不可变快照，结构变化后整体替换。
    snapshot: ArcSwap<Vec<Arc<Entry>>>,
    /// 当前轮次，用于代理轮换。
    generation: AtomicU64,
}

impl Default for ProxyPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyPool {
    /// 创建一个空代理池，轮次从 [`FIRST_GENERATION`] 开始。
    pub fn new() -> Self {
        Self::with_generation(FIRST_GENERATION)
    }

    /// 创建一个空代理池，并指定初始轮次。
    pub fn with_generation(generation: u64) -> Self {
        Self {
            index: RwLock::new(HashMap::new()),
            snapshot: ArcSwap::from_pointee(Vec::new()),
            generation: AtomicU64::new(generation.max(FIRST_GENERATION)),
        }
    }

    /// 用一组已有的代理构造代理池（测试和缓存加载器会用到）。
    pub fn from_proxies<I: IntoIterator<Item = Proxy>>(proxies: I) -> Self {
        let pool = Self::new();
        {
            let mut guard = pool.write();
            for proxy in proxies {
                let entry = Arc::new(Entry::from_proxy(proxy));
                guard.insert(entry.id.clone(), entry);
            }
        }
        pool.rebuild();
        pool
    }

    /// 插入一个代理 URL，返回其标识符以及是否为新增。
    pub fn insert(&self, url: Url) -> (ProxyId, bool) {
        let entry = Arc::new(Entry::from_proxy(Proxy::new(url)));
        let id = entry.id.clone();
        let is_new = {
            let mut guard = self.write();
            match guard.get(&id) {
                // 已存在：URL 不会变（id 由 URL 派生），保持原有事实。
                Some(_) => false,
                None => {
                    guard.insert(id.clone(), entry);
                    true
                }
            }
        };
        if is_new {
            self.rebuild();
        }
        (id, is_new)
    }

    /// 插入一个已构造好的代理；若已存在则保留其使用记录。
    pub fn insert_proxy(&self, proxy: Proxy) -> bool {
        let entry = Arc::new(Entry::from_proxy(proxy));
        let is_new = {
            let mut guard = self.write();
            match guard.get(&entry.id) {
                Some(_) => false,
                None => {
                    guard.insert(entry.id.clone(), entry);
                    true
                }
            }
        };
        if is_new {
            self.rebuild();
        }
        is_new
    }

    /// 一次合并大量 URL，是订阅源刷新之后的常规路径。
    pub fn merge<I: IntoIterator<Item = Url>>(&self, urls: I) -> MergeStats {
        let mut stats = MergeStats::default();
        {
            let mut guard = self.write();
            for url in urls {
                let entry = Arc::new(Entry::from_proxy(Proxy::new(url)));
                match guard.get(&entry.id) {
                    Some(_) => stats.existing += 1,
                    None => {
                        guard.insert(entry.id.clone(), entry);
                        stats.added += 1;
                    }
                }
            }
        }
        // 一次合并只需要重建一次快照：一轮刷新里每个订阅源调一次。
        if stats.added > 0 {
            self.rebuild();
        }
        stats
    }

    /// 删除所有标识符不在 `keep` 中的代理，返回删除数量。
    ///
    /// 仅在每个订阅源都成功的刷新之后调用，
    /// 因此部分失败不会误删仍存活的代理。
    pub fn retain(&self, keep: &HashSet<ProxyId>) -> usize {
        let removed = {
            let mut guard = self.write();
            let before = guard.len();
            guard.retain(|id, _| keep.contains(id));
            before - guard.len()
        };
        if removed > 0 {
            self.rebuild();
        }
        removed
    }

    /// 代理池中的代理总数。
    pub fn len(&self) -> usize {
        self.snapshot.load().len()
    }

    /// 代理池是否为空。
    pub fn is_empty(&self) -> bool {
        self.snapshot.load().is_empty()
    }

    /// 返回代理池的规模统计。
    pub fn stats(&self) -> PoolStats {
        let entries = self.snapshot.load();
        let alive = entries.iter().filter(|entry| entry.is_alive()).count();
        PoolStats {
            total: entries.len(),
            alive,
            dead: entries.len() - alive,
        }
    }

    /// 代理池是否包含指定标识符的代理。
    pub fn contains(&self, id: &ProxyId) -> bool {
        self.read().contains_key(id)
    }

    /// 按标识符获取代理的副本。
    pub fn get(&self, id: &ProxyId) -> Option<Proxy> {
        self.entry(id).map(|entry| entry.to_proxy())
    }

    /// 所有代理的确定性副本（按标识符排序）。
    pub fn snapshot(&self) -> Vec<Proxy> {
        self.snapshot
            .load()
            .iter()
            .map(|entry| entry.to_proxy())
            .collect()
    }

    /// 当前轮次。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 设置当前轮次，低于 [`FIRST_GENERATION`] 的值会被抬升。
    pub fn set_generation(&self, generation: u64) {
        self.generation
            .store(generation.max(FIRST_GENERATION), Ordering::SeqCst);
    }

    /// 在一个短暂的写锁窗口内应用健康检查结果。
    pub fn update_health(&self, updates: &[(ProxyId, HealthUpdate)]) -> usize {
        let mut applied = 0;
        for (id, update) in updates {
            let Some(entry) = self.entry(id) else {
                continue;
            };
            entry
                .checked_at_ms
                .store(millis(Some(update.checked_at)), Ordering::Relaxed);
            entry.alive.store(update.alive, Ordering::Release);
            match update.latency {
                Some(latency) => entry.set_latency(latency),
                None => entry
                    .latency_ms
                    .store(UNKNOWN_LATENCY_MS, Ordering::Relaxed),
            }
            if update.alive {
                entry.failures.store(0, Ordering::Relaxed);
            }
            applied += 1;
        }
        applied
    }

    /// 应用一整轮健康检查的结果。
    ///
    /// 成功会复活代理并清零失败计数；
    /// 失败规则见 [`ProxyPool::record_failure`]。
    pub fn apply_health_pass(
        &self,
        updates: &[(ProxyId, HealthUpdate)],
        policy: &HealthPolicy,
    ) -> PoolStats {
        for (id, update) in updates {
            let Some(entry) = self.entry(id) else {
                continue;
            };
            // 探测结果描述的正是刚跑的这一轮，无论死活都记下来：部分可达的代理
            // 恰恰是运维想看到的东西。
            entry.apply_probe(
                update.alive,
                update.latency,
                update.checked_at,
                Some(&update.probes),
                policy,
            );
        }
        self.stats()
    }

    /// 从缓存文件恢复健康事实，
    /// 不改变失败计数的语义（文件中的值就是权威）。
    pub fn restore_health(
        &self,
        entries: &[(ProxyId, HealthRestore)],
        policy: &HealthPolicy,
    ) -> usize {
        let mut applied = 0;
        for (id, restore) in entries {
            if let Some(entry) = self.entry(id) {
                entry.restore_health(restore, policy);
                applied += 1;
            }
        }
        applied
    }

    /// 记录一次在健康检查器之外的成功使用：
    /// 网关在客户端请求成功时会这样做。
    pub fn record_success(
        &self,
        id: &ProxyId,
        latency: Option<Duration>,
        checked_at: SystemTime,
        policy: &HealthPolicy,
    ) {
        if let Some(entry) = self.entry(id) {
            entry.record_success(latency, checked_at, policy);
        }
    }

    /// 记录一次失败，
    /// 并在连续失败达到 `max_failures` 次后把代理标记为死亡，
    /// 返回新的存活标志。
    ///
    /// 从未成功过的代理在一次失败后即判为死亡。
    pub fn record_failure(
        &self,
        id: &ProxyId,
        policy: &HealthPolicy,
        now: SystemTime,
    ) -> Option<bool> {
        self.entry(id)
            .map(|entry| entry.record_failure(policy, now))
    }

    /// 在当前轮次中把代理标记为已使用。
    pub fn mark_used(&self, id: &ProxyId, used_at: SystemTime) -> bool {
        let generation = self.generation();
        self.mark_used_in_round(id, generation, used_at)
    }

    /// 在指定轮次中把代理标记为已使用，返回代理是否存在。
    pub fn mark_used_in_round(&self, id: &ProxyId, generation: u64, used_at: SystemTime) -> bool {
        match self.entry(id) {
            Some(entry) => {
                entry.round.store(generation, Ordering::Relaxed);
                entry
                    .used_at_ms
                    .store(millis(Some(used_at)), Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// 在代理加载完成之后恢复持久化的使用信息（`state.json`）。
    pub fn restore_usage(
        &self,
        id: &ProxyId,
        generation: u64,
        last_used_at: Option<SystemTime>,
    ) -> bool {
        match self.entry(id) {
            Some(entry) => {
                entry.restore_usage(generation, last_used_at);
                true
            }
            None => false,
        }
    }

    /// 到点该重探的代理（`next_check_at` 已过或从未安排过）。
    ///
    /// 这是健康循环的输入：**不再每轮全量重探**，死代理按退避计划越探越稀。
    pub fn due(&self, now: SystemTime) -> Vec<Proxy> {
        self.snapshot
            .load()
            .iter()
            .filter(|entry| entry.is_due(now))
            .map(|entry| entry.to_proxy())
            .collect()
    }

    /// 距离下一个到期探测还有多久；池子为空时返回 `None`。
    pub fn next_check_in(&self, now: SystemTime) -> Option<Duration> {
        self.snapshot
            .load()
            .iter()
            .filter_map(|entry| entry.next_check_at())
            .map(|at| at.duration_since(now).unwrap_or(Duration::ZERO))
            .min()
    }

    /// 使用信息，用于持久化 `state.json`。
    pub fn usage(&self) -> Vec<(ProxyId, u64, Option<SystemTime>)> {
        self.snapshot
            .load()
            .iter()
            .map(|entry| {
                (
                    entry.id.clone(),
                    entry.round.load(Ordering::Relaxed),
                    entry.last_used_at(),
                )
            })
            .collect()
    }

    /// 对快照应用轮换规则，不修改任何状态。
    pub fn plan(&self, reuse_after: Duration, now: SystemTime) -> OwnedPlan {
        let entries = self.snapshot.load_full();
        let plan = selector::plan(entries.as_slice(), self.generation(), reuse_after, now);
        OwnedPlan {
            candidates: plan
                .candidates
                .into_iter()
                .map(|entry| entry.to_proxy())
                .collect(),
            generation: plan.generation,
            reset_round: plan.reset_round,
            healthy: plan.healthy,
            used_this_round: plan.used_this_round,
        }
    }

    /// 挑选一个代理*并*把它标记为已使用。
    ///
    /// 全程不拿写锁：读一次快照（共享的 `Arc`），按轮换规则筛出候选，再用
    /// 条目上的 CAS 认领其中一个——这样即使多个请求同时进来，"一轮内不重复
    /// 分发"依然成立。
    ///
    /// 代理池中没有健康代理时返回 `None`。
    pub fn select(&self, options: SelectionOptions, now: SystemTime) -> Option<Selection> {
        // 每一轮循环都可能因为"别人刚抢走/刚推进轮次"而重来；上限只是防御，
        // 正常情况下第一次就成。
        const MAX_ROUNDS: usize = 8;
        let mut reset_round = false;

        for _ in 0..MAX_ROUNDS {
            let generation = self.generation.load(Ordering::SeqCst);
            let entries = self.snapshot.load_full();
            let plan: Plan<'_, Arc<Entry>> =
                selector::plan(entries.as_slice(), generation, options.reuse_after, now);

            if plan.candidates.is_empty() {
                return None;
            }

            if plan.reset_round {
                // 所有健康代理都已经发过一遍：立刻开新一轮，不等 reuse 窗口。
                if self
                    .generation
                    .compare_exchange(
                        generation,
                        plan.generation,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_err()
                {
                    // 别人先推了一轮，重新规划。
                    continue;
                }
                reset_round = true;
            }

            if let Some(entry) = self.claim(&plan.candidates, options, plan.generation, now) {
                return Some(Selection {
                    proxy: entry.to_proxy(),
                    round: plan.generation,
                    reset_round,
                    healthy: plan.healthy,
                    candidates: plan.candidates.len(),
                });
            }

            // 候选全被别的请求抢走了：重新读一遍快照再来。
            reset_round = false;
        }

        None
    }

    /// 按策略顺序认领一个候选：CAS 成功即算本次分发。
    fn claim(
        &self,
        candidates: &[&Arc<Entry>],
        options: SelectionOptions,
        round: u64,
        now: SystemTime,
    ) -> Option<Arc<Entry>> {
        for index in selector::claim_order(candidates, options, now) {
            let entry = candidates[index];
            let seen = entry.round.load(Ordering::SeqCst);
            // `round` 之前的轮次才算"这一轮还没发过"；相等说明并发请求先抢到了。
            if seen >= round {
                continue;
            }
            if entry
                .round
                .compare_exchange(seen, round, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                entry.used_at_ms.store(millis(Some(now)), Ordering::Relaxed);
                return Some(entry.clone());
            }
        }
        None
    }

    /// 按标识符查一个条目（只读索引，不重建快照）。
    fn entry(&self, id: &ProxyId) -> Option<Arc<Entry>> {
        self.read().get(id).cloned()
    }

    /// 重建读路径的快照。索引变化（新增、裁剪）之后调用一次。
    fn rebuild(&self) {
        let mut entries: Vec<Arc<Entry>> = self.read().values().cloned().collect();
        // 按标识符排序：`snapshot()` 的顺序因此是确定的，随机策略也不依赖
        // HashMap 的迭代顺序。
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        self.snapshot.store(Arc::new(entries));
    }

    /// 获取读锁；锁中毒时仍继续使用内部数据。
    fn read(&self) -> RwLockReadGuard<'_, HashMap<ProxyId, Arc<Entry>>> {
        self.index.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// 获取写锁；锁中毒时仍继续使用内部数据。
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<ProxyId, Arc<Entry>>> {
        self.index.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Unix 毫秒，`None` 与纪元前的时间都记成 `0`（= 从未发生）。
fn millis(time: Option<SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// [`millis`] 的逆运算；`0` 表示"从未发生"。
fn from_millis(millis: u64) -> Option<SystemTime> {
    match millis {
        0 => None,
        millis => Some(UNIX_EPOCH + Duration::from_millis(millis)),
    }
}

/// 拥有所有权的 [`Plan`]，由 [`ProxyPool::plan`] 返回。
#[derive(Debug, Clone)]
pub struct OwnedPlan {
    /// 可被分发的候选代理，优先级最高的在最前。
    pub candidates: Vec<Proxy>,
    /// 该选择所属的轮次（可能是当前轮次加一）。
    pub generation: u64,
    /// 调用方是否必须推进代理池的轮次。
    pub reset_round: bool,
    /// 快照中的健康代理数量。
    pub healthy: usize,
    /// 当前轮次中已经使用过的健康代理数量。
    pub used_this_round: usize,
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn policy() -> HealthPolicy {
        HealthPolicy {
            max_failures: 3,
            ok_delay: Duration::from_secs(300),
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(1800),
        }
    }

    #[test]
    fn a_successful_probe_is_rescheduled_at_the_plain_interval() {
        assert_eq!(policy().next_delay(true, 0), Duration::from_secs(300));
        // 失败次数对成功的调度没有影响（成功会把它清零）。
        assert_eq!(policy().next_delay(true, 7), Duration::from_secs(300));
    }

    #[test]
    fn a_failing_probe_backs_off_geometrically_up_to_the_cap() {
        let policy = policy();
        let delays: Vec<u64> = (1..=12)
            .map(|failures| policy.next_delay(false, failures).as_secs())
            .collect();
        assert_eq!(
            delays,
            vec![10, 20, 40, 80, 160, 320, 640, 1280, 1800, 1800, 1800, 1800]
        );
        // 巨大失败次数不会溢出或回绕。
        assert_eq!(
            policy.next_delay(false, u32::MAX),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn a_proxy_is_due_immediately_until_it_is_scheduled() {
        let entry = Entry::from_proxy(Proxy::new(crate::model::normalize("1.1.1.1:8080").unwrap()));
        let now = SystemTime::now();
        assert!(entry.is_due(now), "never-checked proxies are due at once");
        assert!(entry.next_check_at().is_none());

        entry.apply_probe(true, Some(Duration::from_millis(12)), now, None, &policy());
        assert!(!entry.is_due(now), "just checked: not due again right away");
        assert!(entry.is_due(now + Duration::from_secs(301)));
    }

    /// 下一次探测距现在多久（毫秒精度，所以断言都给一点余量）。
    fn scheduled_in(entry: &Entry, now: SystemTime) -> Duration {
        entry
            .next_check_at()
            .expect("a probe was just applied, so it is scheduled")
            .duration_since(now)
            .unwrap_or(Duration::ZERO)
    }

    fn assert_scheduled(entry: &Entry, now: SystemTime, expected: Duration) {
        let actual = scheduled_in(entry, now);
        assert!(
            actual.abs_diff(expected) < Duration::from_millis(5),
            "expected ~{expected:?} from now, got {actual:?}"
        );
    }

    #[test]
    fn a_failing_proxy_gets_retried_more_and_more_slowly() {
        let entry = Entry::from_proxy(Proxy::new(crate::model::normalize("1.1.1.1:8080").unwrap()));
        let policy = policy();
        let mut now = SystemTime::now();

        // 第一次失败：10 秒后再探（base 5s × 2^1）。
        entry.apply_probe(false, None, now, None, &policy);
        assert_scheduled(&entry, now, Duration::from_secs(10));

        // 第二次：20 秒。
        now += Duration::from_secs(10);
        entry.apply_probe(false, None, now, None, &policy);
        assert_scheduled(&entry, now, Duration::from_secs(20));

        // 成功一次就回到 ok_delay，失败计数清零。
        now += Duration::from_secs(20);
        entry.apply_probe(true, Some(Duration::from_millis(9)), now, None, &policy);
        assert_scheduled(&entry, now, Duration::from_secs(300));
        assert_eq!(entry.failures.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::normalize;
    use crate::selector::Strategy;

    fn url(host: &str) -> Url {
        normalize(&format!("{host}:8080")).unwrap()
    }

    #[test]
    fn merge_counts_new_and_existing() {
        let pool = ProxyPool::new();
        let stats = pool.merge(vec![url("1.1.1.1"), url("2.2.2.2")]);
        assert_eq!(stats.added, 2);
        assert_eq!(stats.existing, 0);

        let stats = pool.merge(vec![url("1.1.1.1"), url("3.3.3.3"), url("1.1.1.1")]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.existing, 2);
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn merge_keeps_usage_and_health() {
        let pool = ProxyPool::new();
        let (id, _) = pool.insert(url("1.1.1.1"));
        pool.restore_usage(&id, 5, Some(SystemTime::now()));
        pool.update_health(&[(
            id.clone(),
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(12)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);

        pool.merge(vec![url("1.1.1.1")]);
        let proxy = pool.get(&id).unwrap();
        assert!(proxy.alive);
        assert_eq!(proxy.generation, 5);
        assert_eq!(proxy.latency_ms(), Some(12));
    }

    #[test]
    fn retain_removes_absent_proxies() {
        let pool = ProxyPool::new();
        let (keep, _) = pool.insert(url("1.1.1.1"));
        pool.insert(url("2.2.2.2"));

        let removed = pool.retain(&HashSet::from([keep.clone()]));
        assert_eq!(removed, 1);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&keep));
    }

    #[test]
    fn failures_mark_dead_after_threshold() {
        let pool = ProxyPool::new();
        let (id, _) = pool.insert(url("1.1.1.1"));
        pool.record_success(
            &id,
            Some(Duration::from_millis(5)),
            SystemTime::now(),
            &HealthPolicy::default(),
        );

        assert_eq!(
            pool.record_failure(&id, &HealthPolicy::default(), SystemTime::now()),
            Some(true)
        );
        assert_eq!(
            pool.record_failure(&id, &HealthPolicy::default(), SystemTime::now()),
            Some(true)
        );
        assert_eq!(
            pool.record_failure(&id, &HealthPolicy::default(), SystemTime::now()),
            Some(false)
        );
        assert_eq!(pool.get(&id).unwrap().failures, 3);

        pool.record_success(
            &id,
            Some(Duration::from_millis(5)),
            SystemTime::now(),
            &HealthPolicy::default(),
        );
        let proxy = pool.get(&id).unwrap();
        assert!(proxy.alive);
        assert_eq!(proxy.failures, 0);

        // A proxy that never answered is dead after its first failure.
        let (fresh, _) = pool.insert(url("2.2.2.2"));
        assert_eq!(
            pool.record_failure(&fresh, &HealthPolicy::default(), SystemTime::now()),
            Some(false)
        );
        assert!(!pool.get(&fresh).unwrap().alive);
    }

    #[test]
    fn select_only_returns_healthy_proxies() {
        let pool = ProxyPool::new();
        let (dead, _) = pool.insert(url("1.1.1.1"));
        let (alive, _) = pool.insert(url("2.2.2.2"));
        pool.update_health(&[
            (
                dead.clone(),
                HealthUpdate {
                    alive: false,
                    latency: None,
                    checked_at: SystemTime::now(),
                    probes: Vec::new(),
                },
            ),
            (
                alive.clone(),
                HealthUpdate {
                    alive: true,
                    latency: Some(Duration::from_millis(1)),
                    checked_at: SystemTime::now(),
                    probes: Vec::new(),
                },
            ),
        ]);

        let selection = pool
            .select(
                SelectionOptions {
                    strategy: Strategy::Random,
                    reuse_after: Duration::from_secs(1800),
                    ..Default::default()
                },
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(selection.proxy.id, alive);
        assert_eq!(selection.healthy, 1);
        assert!(!selection.reset_round);

        // The only healthy proxy was just used: the round rolls over instead of
        // returning nothing.
        let again = pool
            .select(
                SelectionOptions {
                    strategy: Strategy::Random,
                    reuse_after: Duration::from_secs(1800),
                    ..Default::default()
                },
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(again.proxy.id, alive);
        assert!(again.reset_round);
        assert_eq!(again.round, selection.round + 1);
        assert_eq!(pool.generation(), selection.round + 1);

        // A pool with no healthy proxy at all yields nothing.
        let empty = ProxyPool::new();
        assert!(
            empty
                .select(
                    SelectionOptions {
                        strategy: Strategy::Random,
                        reuse_after: Duration::from_secs(1800),
                        ..Default::default()
                    },
                    SystemTime::now()
                )
                .is_none()
        );
    }
}
