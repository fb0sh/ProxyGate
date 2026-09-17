//! The proxy pool.
//!
//! A `HashMap` behind an `RwLock` is enough for the 10k proxies v0.1 targets.
//! The pool is the *only* place that mutates proxies: every other module reads a
//! snapshot, does its slow work (network I/O) without holding a lock, and then
//! hands the results back through a small update method.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime};

use url::Url;

use crate::model::{ProbeOutcome, Proxy, ProxyId};
use crate::selector::{self, Plan, Strategy};

/// Round numbering starts at 1 so that a freshly inserted proxy (`generation`
/// 0) counts as "not used yet".
pub const FIRST_GENERATION: u64 = 1;

/// Outcome of merging a batch of URLs into the pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub added: usize,
    pub existing: usize,
}

impl MergeStats {
    pub fn total(&self) -> usize {
        self.added + self.existing
    }
}

/// Health facts written back after a check.
#[derive(Debug, Clone)]
pub struct HealthUpdate {
    pub alive: bool,
    pub latency: Option<Duration>,
    pub checked_at: SystemTime,
    /// Per-target outcome of the pass that produced this update.
    pub probes: Vec<ProbeOutcome>,
}

/// Health facts read back from the on-disk cache (absolute values, no counting).
#[derive(Debug, Clone)]
pub struct HealthRestore {
    pub alive: bool,
    pub latency: Option<Duration>,
    pub failures: u32,
    pub checked_at: Option<SystemTime>,
    pub probes: Vec<ProbeOutcome>,
}

/// A proxy handed out by [`ProxyPool::select`].
#[derive(Debug, Clone)]
pub struct Selection {
    pub proxy: Proxy,
    /// Round the proxy was assigned to.
    pub round: u64,
    /// True when this selection started a new round.
    pub reset_round: bool,
    /// Healthy proxies in the pool when the selection was made.
    pub healthy: usize,
    /// Candidates considered for this selection.
    pub candidates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolStats {
    pub total: usize,
    pub alive: usize,
    pub dead: usize,
}

/// Thread-safe collection of proxies.
#[derive(Debug)]
pub struct ProxyPool {
    proxies: RwLock<HashMap<ProxyId, Proxy>>,
    generation: AtomicU64,
}

impl Default for ProxyPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyPool {
    pub fn new() -> Self {
        Self::with_generation(FIRST_GENERATION)
    }

    pub fn with_generation(generation: u64) -> Self {
        Self {
            proxies: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(generation.max(FIRST_GENERATION)),
        }
    }

    /// Builds a pool from an existing set of proxies (used by tests and by the
    /// cache loader).
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

    /// Inserts a proxy URL. Returns its id and whether it was new.
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

    /// Inserts an already built proxy, keeping usage fields if it exists.
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

    /// Merges many URLs at once — the normal path after a subscriber refresh.
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

    /// Drops every proxy whose id is not in `keep`. Returns how many were
    /// removed. Only called after a refresh in which every subscriber
    /// succeeded, so a partial failure cannot evict live proxies.
    pub fn retain(&self, keep: &HashSet<ProxyId>) -> usize {
        let mut guard = self.write();
        let before = guard.len();
        guard.retain(|id, _| keep.contains(id));
        before - guard.len()
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    pub fn stats(&self) -> PoolStats {
        let guard = self.read();
        let alive = guard.values().filter(|proxy| proxy.alive).count();
        PoolStats {
            total: guard.len(),
            alive,
            dead: guard.len() - alive,
        }
    }

    pub fn contains(&self, id: &ProxyId) -> bool {
        self.read().contains_key(id)
    }

    pub fn get(&self, id: &ProxyId) -> Option<Proxy> {
        self.read().get(id).cloned()
    }

    /// Deterministic copy of every proxy (sorted by id).
    pub fn snapshot(&self) -> Vec<Proxy> {
        let mut proxies: Vec<Proxy> = self.read().values().cloned().collect();
        proxies.sort_by(|a, b| a.id.cmp(&b.id));
        proxies
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn set_generation(&self, generation: u64) {
        self.generation
            .store(generation.max(FIRST_GENERATION), Ordering::SeqCst);
    }

    /// Applies health results in one short write-lock window.
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

    /// Applies the outcome of a full health pass in one short write-lock window.
    ///
    /// A success revives the proxy and clears its failure counter; see
    /// [`ProxyPool::record_failure`] for the failure rule.
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

    /// Restores health facts from the cache file, without touching the failure
    /// counters' semantics (the values in the file are the truth).
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

    /// Records a successful use of a proxy outside the checker (the gateway
    /// does this when a client request succeeds).
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

    /// Records a failure and marks the proxy dead once `max_failures`
    /// consecutive failures were seen. Returns the new alive flag.
    ///
    /// A proxy that never worked is dead after a single failure.
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

    /// Marks a proxy as used in the current round.
    pub fn mark_used(&self, id: &ProxyId, used_at: SystemTime) -> bool {
        let generation = self.generation();
        self.mark_used_in_round(id, generation, used_at)
    }

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

    /// Restores persisted usage (`state.json`) after the proxies were loaded.
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

    /// Usage facts, for persisting `state.json`.
    pub fn usage(&self) -> Vec<(ProxyId, u64, Option<SystemTime>)> {
        self.read()
            .values()
            .map(|proxy| (proxy.id.clone(), proxy.generation, proxy.last_used_at))
            .collect()
    }

    /// Applies the rotation rules to a snapshot without mutating anything.
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

    /// Picks a proxy *and* marks it as used, atomically.
    ///
    /// Returns `None` when the pool holds no healthy proxy.
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

    fn read(&self) -> RwLockReadGuard<'_, HashMap<ProxyId, Proxy>> {
        self.proxies.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<ProxyId, Proxy>> {
        self.proxies.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An owned [`Plan`], returned by [`ProxyPool::plan`].
#[derive(Debug, Clone)]
pub struct OwnedPlan {
    pub candidates: Vec<Proxy>,
    pub generation: u64,
    pub reset_round: bool,
    pub healthy: usize,
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
