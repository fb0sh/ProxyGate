//! 代理池。
//!
//! 一个用 `RwLock` 包住的 `HashMap` 就够了：
//! v0.1 目标是一万个代理。
//! 代理池是*唯一*修改代理的地方：
//! 其他模块只读取快照，在不持锁的情况下完成耗时的网络 I/O，
//! 然后通过一个小的更新方法把结果写回。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

use url::Url;

use crate::model::{ProbeOutcome, Proxy, ProxyId};
use crate::selector::{self, Plan, Strategy};

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

/// 线程安全的代理集合。
#[derive(Debug)]
pub struct ProxyPool {
    /// 受 `RwLock` 保护的代理表，按标识符索引。
    proxies: RwLock<HashMap<ProxyId, Proxy>>,
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
            proxies: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(generation.max(FIRST_GENERATION)),
        }
    }

    /// 用一组已有的代理构造代理池（测试和缓存加载器会用到）。
    pub fn from_proxies<I: IntoIterator<Item = Proxy>>(proxies: I) -> Self {
        let pool = Self::new();
        {
            let mut guard = pool.write();
            for proxy in proxies {
                guard.insert(proxy.id.clone(), proxy);
            }
        }
        pool
    }

    /// 插入一个代理 URL，返回其标识符以及是否为新增。
    pub fn insert(&self, url: Url) -> (ProxyId, bool) {
        let proxy = Proxy::new(url);
        let id = proxy.id.clone();
        let mut guard = self.write();
        let is_new = match guard.get_mut(&id) {
            Some(existing) => {
                // Refresh the URL but keep health and usage history.
                existing.url = proxy.url;
                false
            }
            None => {
                guard.insert(id.clone(), proxy);
                true
            }
        };
        (id, is_new)
    }

    /// 插入一个已构造好的代理；若已存在则保留其使用记录。
    pub fn insert_proxy(&self, proxy: Proxy) -> bool {
        let mut guard = self.write();
        match guard.get_mut(&proxy.id) {
            Some(existing) => {
                existing.url = proxy.url;
                false
            }
            None => {
                guard.insert(proxy.id.clone(), proxy);
                true
            }
        }
    }

    /// 一次合并大量 URL，是订阅源刷新之后的常规路径。
    pub fn merge<I: IntoIterator<Item = Url>>(&self, urls: I) -> MergeStats {
        let mut stats = MergeStats::default();
        let mut guard = self.write();
        for url in urls {
            let proxy = Proxy::new(url);
            match guard.get_mut(&proxy.id) {
                Some(existing) => {
                    existing.url = proxy.url;
                    stats.existing += 1;
                }
                None => {
                    guard.insert(proxy.id.clone(), proxy);
                    stats.added += 1;
                }
            }
        }
        stats
    }

    /// 删除所有标识符不在 `keep` 中的代理，返回删除数量。
    ///
    /// 仅在每个订阅源都成功的刷新之后调用，
    /// 因此部分失败不会误删仍存活的代理。
    pub fn retain(&self, keep: &HashSet<ProxyId>) -> usize {
        let mut guard = self.write();
        let before = guard.len();
        guard.retain(|id, _| keep.contains(id));
        before - guard.len()
    }

    /// 代理池中的代理总数。
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// 代理池是否为空。
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// 返回代理池的规模统计。
    pub fn stats(&self) -> PoolStats {
        let guard = self.read();
        let alive = guard.values().filter(|proxy| proxy.alive).count();
        PoolStats {
            total: guard.len(),
            alive,
            dead: guard.len() - alive,
        }
    }

    /// 代理池是否包含指定标识符的代理。
    pub fn contains(&self, id: &ProxyId) -> bool {
        self.read().contains_key(id)
    }

    /// 按标识符获取代理的副本。
    pub fn get(&self, id: &ProxyId) -> Option<Proxy> {
        self.read().get(id).cloned()
    }

    /// 所有代理的确定性副本（按标识符排序）。
    pub fn snapshot(&self) -> Vec<Proxy> {
        let mut proxies: Vec<Proxy> = self.read().values().cloned().collect();
        proxies.sort_by(|a, b| a.id.cmp(&b.id));
        proxies
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
        let mut guard = self.write();
        let mut applied = 0;
        for (id, update) in updates {
            if let Some(proxy) = guard.get_mut(id) {
                proxy.alive = update.alive;
                proxy.latency = update.latency;
                proxy.last_checked_at = Some(update.checked_at);
                if update.alive {
                    proxy.failures = 0;
                }
                applied += 1;
            }
        }
        applied
    }

    /// 在一个短暂的写锁窗口内应用一整轮健康检查的结果。
    ///
    /// 成功会复活代理并清零失败计数；
    /// 失败规则见 [`ProxyPool::record_failure`]。
    pub fn apply_health_pass(
        &self,
        updates: &[(ProxyId, HealthUpdate)],
        max_failures: u32,
    ) -> PoolStats {
        let threshold = max_failures.max(1);
        let mut guard = self.write();
        for (id, update) in updates {
            let Some(proxy) = guard.get_mut(id) else {
                continue;
            };
            proxy.last_checked_at = Some(update.checked_at);
            // The probes describe the pass that just ran, alive or not: a
            // partially reachable proxy is exactly what an operator wants to see.
            proxy.probes = update.probes.clone();
            if update.alive {
                proxy.alive = true;
                proxy.failures = 0;
                proxy.latency = update.latency;
            } else {
                let was_alive = proxy.alive;
                proxy.failures = proxy.failures.saturating_add(1);
                // A proxy that never worked is dead immediately; a working one
                // survives transient probe failures up to `max_failures`.
                proxy.alive = was_alive && proxy.failures < threshold;
            }
        }
        let alive = guard.values().filter(|proxy| proxy.alive).count();
        PoolStats {
            total: guard.len(),
            alive,
            dead: guard.len() - alive,
        }
    }

    /// 从缓存文件恢复健康事实，
    /// 不改变失败计数的语义（文件中的值就是权威）。
    pub fn restore_health(&self, entries: &[(ProxyId, HealthRestore)]) -> usize {
        let mut guard = self.write();
        let mut applied = 0;
        for (id, entry) in entries {
            if let Some(proxy) = guard.get_mut(id) {
                proxy.alive = entry.alive;
                proxy.latency = entry.latency;
                proxy.failures = entry.failures;
                proxy.last_checked_at = entry.checked_at;
                proxy.probes = entry.probes.clone();
                applied += 1;
            }
        }
        applied
    }

    /// 记录一次在健康检查器之外的成功使用：
    /// 网关在客户端请求成功时会这样做。
    pub fn record_success(&self, id: &ProxyId, latency: Option<Duration>, checked_at: SystemTime) {
        let mut guard = self.write();
        if let Some(proxy) = guard.get_mut(id) {
            proxy.alive = true;
            proxy.failures = 0;
            if latency.is_some() {
                proxy.latency = latency;
            }
            proxy.last_checked_at = Some(checked_at);
        }
    }

    /// 记录一次失败，
    /// 并在连续失败达到 `max_failures` 次后把代理标记为死亡，
    /// 返回新的存活标志。
    ///
    /// 从未成功过的代理在一次失败后即判为死亡。
    pub fn record_failure(&self, id: &ProxyId, max_failures: u32) -> Option<bool> {
        let mut guard = self.write();
        let proxy = guard.get_mut(id)?;
        let was_alive = proxy.alive;
        proxy.failures = proxy.failures.saturating_add(1);
        // `max_failures` of 0 means "one failure is enough".
        let threshold = max_failures.max(1);
        proxy.alive = was_alive && proxy.failures < threshold;
        Some(proxy.alive)
    }

    /// 在当前轮次中把代理标记为已使用。
    pub fn mark_used(&self, id: &ProxyId, used_at: SystemTime) -> bool {
        let generation = self.generation();
        self.mark_used_in_round(id, generation, used_at)
    }

    /// 在指定轮次中把代理标记为已使用，返回代理是否存在。
    pub fn mark_used_in_round(&self, id: &ProxyId, generation: u64, used_at: SystemTime) -> bool {
        let mut guard = self.write();
        match guard.get_mut(id) {
            Some(proxy) => {
                proxy.generation = generation;
                proxy.last_used_at = Some(used_at);
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
        let mut guard = self.write();
        match guard.get_mut(id) {
            Some(proxy) => {
                proxy.generation = generation;
                proxy.last_used_at = last_used_at;
                true
            }
            None => false,
        }
    }

    /// 使用信息，用于持久化 `state.json`。
    pub fn usage(&self) -> Vec<(ProxyId, u64, Option<SystemTime>)> {
        self.read()
            .values()
            .map(|proxy| (proxy.id.clone(), proxy.generation, proxy.last_used_at))
            .collect()
    }

    /// 对快照应用轮换规则，不修改任何状态。
    pub fn plan(&self, reuse_after: Duration, now: SystemTime) -> OwnedPlan {
        let generation = self.generation();
        let snapshot = self.snapshot();
        let plan = selector::plan(&snapshot, generation, reuse_after, now);
        OwnedPlan {
            candidates: plan.candidates.into_iter().cloned().collect(),
            generation: plan.generation,
            reset_round: plan.reset_round,
            healthy: plan.healthy,
            used_this_round: plan.used_this_round,
        }
    }

    /// 原子地挑选一个代理*并*把它标记为已使用。
    ///
    /// 代理池中没有健康代理时返回 `None`。
    pub fn select(
        &self,
        strategy: Strategy,
        reuse_after: Duration,
        now: SystemTime,
    ) -> Option<Selection> {
        let mut guard = self.write();
        let generation = self.generation.load(Ordering::SeqCst);
        let snapshot: Vec<Proxy> = guard.values().cloned().collect();
        let plan: Plan<'_> = selector::plan(&snapshot, generation, reuse_after, now);

        let index = selector::pick(&plan.candidates, strategy)?;
        let chosen = plan.candidates[index].id.clone();
        let round = plan.generation;

        if plan.reset_round {
            self.generation.store(round, Ordering::SeqCst);
        }

        let proxy = guard.get_mut(&chosen)?;
        proxy.generation = round;
        proxy.last_used_at = Some(now);
        let selected = proxy.clone();

        Some(Selection {
            proxy: selected,
            round,
            reset_round: plan.reset_round,
            healthy: plan.healthy,
            candidates: plan.candidates.len(),
        })
    }

    /// 获取读锁；锁中毒时仍继续使用内部数据。
    fn read(&self) -> RwLockReadGuard<'_, HashMap<ProxyId, Proxy>> {
        self.proxies.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// 获取写锁；锁中毒时仍继续使用内部数据。
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<ProxyId, Proxy>> {
        self.proxies.write().unwrap_or_else(PoisonError::into_inner)
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
mod tests {
    use super::*;
    use crate::model::normalize;

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
        pool.record_success(&id, Some(Duration::from_millis(5)), SystemTime::now());

        assert_eq!(pool.record_failure(&id, 3), Some(true));
        assert_eq!(pool.record_failure(&id, 3), Some(true));
        assert_eq!(pool.record_failure(&id, 3), Some(false));
        assert_eq!(pool.get(&id).unwrap().failures, 3);

        pool.record_success(&id, Some(Duration::from_millis(5)), SystemTime::now());
        let proxy = pool.get(&id).unwrap();
        assert!(proxy.alive);
        assert_eq!(proxy.failures, 0);

        // A proxy that never answered is dead after its first failure.
        let (fresh, _) = pool.insert(url("2.2.2.2"));
        assert_eq!(pool.record_failure(&fresh, 3), Some(false));
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
                Strategy::Random,
                Duration::from_secs(1800),
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
                Strategy::Random,
                Duration::from_secs(1800),
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
                    Strategy::Random,
                    Duration::from_secs(1800),
                    SystemTime::now()
                )
                .is_none()
        );
    }
}
