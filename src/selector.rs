//! 候选过滤与选择策略。
//!
//! 轮换规则位于 [`plan`]：
//!
//! * 只会分发健康的代理；
//! * 在当前轮次中已经使用过的代理会被跳过；
//! * 在剩下的代理中，`reuse_after` 时间内未被使用的优先；
//! * 当所有健康代理都已用过时，立即递增轮次，
//!   而不是等待 `reuse_after` 到期。

use std::time::{Duration, SystemTime};

use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::Proxy;

/// 选择器需要知道的、关于一个候选代理的**只读事实**。
///
/// 之所以抽这层：选择在热点路径上跑，池子里的条目把可变事实存在原子量里
/// （[`crate::pool::Entry`]），而 API、持久化和测试用的是普通的 [`Proxy`]
/// 副本。选择逻辑只关心下面这四个问题，两边都答得上来就够了。
pub trait Candidate {
    /// 当前是否健康。
    fn is_alive(&self) -> bool;
    /// 上一次被分发到的轮次；`0` 表示从未分发过。
    fn round(&self) -> u64;
    /// 最近一次测量到的延迟；`None` 表示未知。
    fn latency(&self) -> Option<Duration>;
    /// 上一次被分发的时间；`None` 表示从未分发过。
    fn last_used_at(&self) -> Option<SystemTime>;
    /// 连续失败次数（打分用）。
    fn failures(&self) -> u32;
}

impl Candidate for Proxy {
    fn is_alive(&self) -> bool {
        self.alive
    }

    fn round(&self) -> u64 {
        self.generation
    }

    fn latency(&self) -> Option<Duration> {
        self.latency
    }

    fn last_used_at(&self) -> Option<SystemTime> {
        self.last_used_at
    }

    fn failures(&self) -> u32 {
        self.failures
    }
}

/// 从候选集合中挑选代理的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// 均匀随机挑选，把负载分散到整个代理池。
    #[default]
    Random,
    /// 选择测量延迟最低的代理。
    Latency,
    /// 在采样出的一小批候选里挑分数最低的：延迟 + 失败惩罚 + 闲置时长。
    ///
    /// 池子几千条时，全量挑"最快"既慢又容易把负载压到少数几条上；采样
    /// （[`SelectionOptions::sample_size`]）之后按分数挑，代价是 O(K)。
    Score,
}

impl Strategy {
    /// 所有可用的选择策略。
    pub const ALL: [Strategy; 3] = [Strategy::Random, Strategy::Latency, Strategy::Score];

    /// 策略的规范小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Strategy::Random => "random",
            Strategy::Latency => "latency",
            Strategy::Score => "score",
        }
    }
}

impl std::str::FromStr for Strategy {
    type Err = Error;

    /// 从字符串解析选择策略，接受 `fastest` 作为 `latency` 的别名。
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "random" => Ok(Strategy::Random),
            "latency" | "fastest" => Ok(Strategy::Latency),
            other => Err(Error::Config(format!(
                "unknown selection strategy `{other}` (expected random, latency or score)"
            ))),
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次选择需要的全部参数。
///
/// 打包在一起是因为它同时被 [`crate::pool::ProxyPool::select`]、
/// [`crate::pool::ProxyPool::plan`] 和 [`claim_order`] 用到，而且
/// [`Strategy::Score`] 的采样大小只有在这里才说得清。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionOptions {
    /// 用什么策略挑。
    pub strategy: Strategy,
    /// 优先避开这段时间内分发过的代理。
    pub reuse_after: Duration,
    /// [`Strategy::Score`] 采样多少个候选；`0` 表示不采样（全量打分）。
    pub sample_size: usize,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            reuse_after: Duration::from_secs(1800),
            sample_size: DEFAULT_SAMPLE_SIZE,
        }
    }
}

/// [`SelectionOptions::sample_size`] 的默认值。
pub const DEFAULT_SAMPLE_SIZE: usize = 32;

/// 延迟未知时的惩罚（毫秒）。比任何真实延迟都差，但不是无穷大——真没别的
/// 可用时它仍然会被选中。
const UNKNOWN_LATENCY_MS: u64 = 5_000;

/// 每失败一次的惩罚（毫秒）。
const FAILURE_PENALTY_MS: u64 = 500;

/// [`Strategy::Score`] 的打分：**越小越好**。
///
/// 三项：延迟、失败次数、以及距上次分发的时长（越久没被用过、越可能要重新
/// 验证，所以久一点的排后面）。轮换与 `reuse_after` 已经负责把负载摊开，
/// 这里只回答"这一批候选里先试谁"。
pub fn score<C: Candidate>(candidate: &C, now: SystemTime) -> u64 {
    let latency = candidate
        .latency()
        .map(|latency| latency.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(UNKNOWN_LATENCY_MS);
    let failures = u64::from(candidate.failures()) * FAILURE_PENALTY_MS;
    let idle = candidate
        .last_used_at()
        .map(|last| elapsed(last, now).as_secs() / 10)
        .unwrap_or(0);
    latency + failures + idle
}

/// 对代理池快照应用轮换规则的结果。
#[derive(Debug)]
pub struct Plan<'a, C: Candidate> {
    /// 可以被分发的代理，优先级最高的在最前。
    pub candidates: Vec<&'a C>,
    /// 该选择所属的轮次（可能是当前轮次加一）。
    pub generation: u64,
    /// 调用方是否必须推进代理池的轮次。
    pub reset_round: bool,
    /// 快照中的健康代理数量。
    pub healthy: usize,
    /// 当前轮次中已经使用过的健康代理数量。
    pub used_this_round: usize,
}

/// 应用轮换规则。纯函数：不加锁，也不读取时钟。
pub fn plan<'a, C: Candidate>(
    proxies: &'a [C],
    generation: u64,
    reuse_after: Duration,
    now: SystemTime,
) -> Plan<'a, C> {
    let healthy: Vec<&C> = proxies.iter().filter(|proxy| proxy.is_alive()).collect();
    let unused_this_round: Vec<&C> = healthy
        .iter()
        .copied()
        .filter(|proxy| proxy.round() < generation)
        .collect();

    let healthy_count = healthy.len();
    let used_this_round = healthy_count - unused_this_round.len();

    let (generation, reset_round, pool): (u64, bool, Vec<&C>) =
        if unused_this_round.is_empty() && !healthy.is_empty() {
            // Every healthy proxy has been handed out: start the next round right
            // away, even if `reuse_after` has not elapsed yet.
            (generation.saturating_add(1), true, healthy)
        } else {
            (generation, false, unused_this_round)
        };

    // Second tier: proxies that were not used within the reuse window.
    let not_recent: Vec<&C> = pool
        .iter()
        .copied()
        .filter(|proxy| !used_recently(*proxy, reuse_after, now))
        .collect();

    let candidates = if not_recent.is_empty() {
        pool
    } else {
        not_recent
    };

    Plan {
        candidates,
        generation,
        reset_round,
        healthy: healthy_count,
        used_this_round,
    }
}

/// 代理是否在复用时间窗内被分发过。
pub fn used_recently<C: Candidate>(proxy: &C, reuse_after: Duration, now: SystemTime) -> bool {
    proxy
        .last_used_at()
        .map(|last| elapsed(last, now) < reuse_after)
        .unwrap_or(false)
}

/// 计算从 `from` 到 `now` 经过的时间，时钟回拨时返回零。
fn elapsed(from: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(from).unwrap_or(Duration::ZERO)
}

/// 从候选集合中挑选一个下标。
pub fn pick<C: Candidate>(candidates: &[&C], strategy: Strategy) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    match strategy {
        Strategy::Random => Some(rand::rng().random_range(0..candidates.len())),
        Strategy::Latency => candidates
            .iter()
            .enumerate()
            .min_by_key(|(_, proxy)| proxy.latency().unwrap_or(Duration::MAX))
            .map(|(index, _)| index),
        Strategy::Score => {
            let now = SystemTime::now();
            candidates
                .iter()
                .enumerate()
                .min_by_key(|(_, proxy): &(usize, &&C)| score::<C>(proxy, now))
                .map(|(index, _)| index)
        }
    }
}

/// 按策略给出**认领顺序**（下标）。
///
/// 池子用 CAS 认领一个候选：抢不到就试下一个，所以除了"选谁"，还需要"接下来
/// 试谁"。随机策略从一个随机位置开始往后走（不重复、每个候选都可能排第一），
/// 延迟策略按延迟从低到高。
pub fn claim_order<C: Candidate>(
    candidates: &[&C],
    options: SelectionOptions,
    now: SystemTime,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    match options.strategy {
        Strategy::Random => {
            if candidates.len() > 1 {
                order.rotate_left(rand::rng().random_range(0..candidates.len()));
            }
        }
        Strategy::Latency => {
            order.sort_by_key(|index| candidates[*index].latency().unwrap_or(Duration::MAX))
        }
        Strategy::Score => {
            // 先采样 K 个：池子几千条时，全量打分（还带排序）的代价换不来
            // 等比例的收益。采样之外的候选排在后面，CAS 抢不到时还能继续试。
            let (mut sampled, rest) = sample_indices(candidates.len(), options.sample_size);
            sampled.sort_by_key(|index| score(candidates[*index], now));
            sampled.extend(rest);
            order = sampled;
        }
    }
    order
}

/// 蓄水池采样出 `size` 个下标，返回（采样、其余）。
///
/// `size` 为 0 或不小于总数时不采样：直接全量打分（`其余` 为空）。
fn sample_indices(total: usize, size: usize) -> (Vec<usize>, Vec<usize>) {
    if size == 0 || size >= total {
        return ((0..total).collect(), Vec::new());
    }

    let mut rng = rand::rng();
    let mut sampled: Vec<usize> = (0..size).collect();
    for index in size..total {
        let pick = rng.random_range(0..=index);
        if pick < size {
            sampled[pick] = index;
        }
    }

    let mut chosen = vec![false; total];
    for index in &sampled {
        chosen[*index] = true;
    }
    let rest = (0..total).filter(|index| !chosen[*index]).collect();
    (sampled, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::normalize;

    fn proxy(host: &str, alive: bool, latency_ms: Option<u64>) -> Proxy {
        let mut proxy = Proxy::new(normalize(&format!("{host}:8080")).unwrap());
        proxy.alive = alive;
        proxy.latency = latency_ms.map(Duration::from_millis);
        proxy
    }

    #[test]
    fn score_prefers_fast_proven_and_recently_seen_proxies() {
        let now = SystemTime::now();
        let mut slow = proxy("1.1.1.1", true, Some(500));
        let fast = proxy("2.2.2.2", true, Some(20));
        let mut failing = proxy("3.3.3.3", true, Some(10));
        failing.failures = 4;
        let mut stale = proxy("4.4.4.4", true, Some(10));
        stale.last_used_at = Some(now - Duration::from_secs(3600));
        slow.last_used_at = Some(now);

        // 延迟占主导：10ms 的两个里，失败次数少的那个赢。
        assert!(score(&fast, now) < score(&failing, now));
        assert!(score(&fast, now) < score(&slow, now));
        // 闲置时长是第三项：同样 10ms、都没失败过，闲置 1 小时的排后面。
        let mut recent = proxy("5.5.5.5", true, Some(10));
        recent.last_used_at = Some(now);
        assert!(score(&recent, now) < score(&stale, now));

        // 延迟未知的排在任何有测量值的后面，但不是无穷大。
        let unknown = proxy("6.6.6.6", true, None);
        assert!(score(&slow, now) < score(&unknown, now));
    }

    #[test]
    fn score_sampling_keeps_the_order_a_prefix_of_the_candidates() {
        // 采样只是"先看这么多"，其余候选仍然在后面排队等 CAS 重试。
        let proxies: Vec<Proxy> = (1..=100)
            .map(|i| proxy(&format!("10.0.0.{i}"), true, Some(i)))
            .collect();
        let refs: Vec<&Proxy> = proxies.iter().collect();
        let options = SelectionOptions {
            strategy: Strategy::Score,
            reuse_after: Duration::from_secs(1800),
            sample_size: 8,
        };
        let order = claim_order(&refs, options, SystemTime::now());
        assert_eq!(order.len(), refs.len(), "每个候选都要能轮到");
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..refs.len()).collect::<Vec<_>>());

        // 前 8 个是从采样里按分数挑的，全在池子最"快"的那一档附近。
        let best: usize = order[0];
        assert!(proxies[best].latency.unwrap() <= Duration::from_millis(32));

        // sample_size = 0 表示不采样：第一个就是全局最优。
        let all = claim_order(
            &refs,
            SelectionOptions {
                sample_size: 0,
                ..options
            },
            SystemTime::now(),
        );
        assert_eq!(all[0], 0, "10.0.0.1 的延迟最低");
    }

    #[test]
    fn ignores_dead_proxies() {
        let proxies = vec![proxy("1.1.1.1", false, None), proxy("2.2.2.2", true, None)];
        let plan = plan(&proxies, 1, Duration::from_secs(1800), SystemTime::now());
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].host(), "2.2.2.2");
        assert!(!plan.reset_round);
    }

    #[test]
    fn empty_pool_yields_no_candidates_and_no_reset() {
        let empty: [Proxy; 0] = [];
        let plan = plan(&empty, 1, Duration::from_secs(1800), SystemTime::now());
        assert!(plan.candidates.is_empty());
        assert!(!plan.reset_round);
        assert_eq!(plan.generation, 1);
    }

    #[test]
    fn skips_proxies_used_in_the_current_round() {
        let mut a = proxy("1.1.1.1", true, None);
        a.generation = 1;
        let b = proxy("2.2.2.2", true, None);

        let proxies = vec![a, b];
        let plan = plan(&proxies, 1, Duration::from_secs(1800), SystemTime::now());
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].host(), "2.2.2.2");
        assert_eq!(plan.used_this_round, 1);
    }

    #[test]
    fn resets_the_round_when_everything_was_used() {
        let now = SystemTime::now();
        let mut a = proxy("1.1.1.1", true, None);
        a.generation = 7;
        a.last_used_at = Some(now);
        let mut b = proxy("2.2.2.2", true, None);
        b.generation = 7;
        b.last_used_at = Some(now);

        let proxies = vec![a, b];
        let plan = plan(&proxies, 7, Duration::from_secs(1800), now);
        assert!(plan.reset_round);
        assert_eq!(plan.generation, 8);
        // Recent use does not stop the new round from handing them out again.
        assert_eq!(plan.candidates.len(), 2);
    }

    #[test]
    fn prefers_proxies_outside_the_reuse_window() {
        let now = SystemTime::now();
        let mut recent = proxy("1.1.1.1", true, None);
        recent.generation = 1;
        recent.last_used_at = Some(now - Duration::from_secs(60));

        let mut old = proxy("2.2.2.2", true, None);
        old.last_used_at = Some(now - Duration::from_secs(60 * 60));

        let proxies = vec![recent, old];
        let plan = plan(&proxies, 2, Duration::from_secs(1800), now);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].host(), "2.2.2.2");
    }

    #[test]
    fn random_reaches_every_candidate() {
        let proxies = [proxy("1.1.1.1", true, None), proxy("2.2.2.2", true, None)];
        let candidates: Vec<&Proxy> = proxies.iter().collect();
        let mut seen = [false; 2];
        for _ in 0..200 {
            seen[pick(&candidates, Strategy::Random).unwrap()] = true;
        }
        assert!(seen[0] && seen[1]);
    }

    #[test]
    fn latency_picks_the_fastest_known() {
        let proxies = [
            proxy("1.1.1.1", true, Some(200)),
            proxy("2.2.2.2", true, Some(50)),
            proxy("3.3.3.3", true, None),
        ];
        let candidates: Vec<&Proxy> = proxies.iter().collect();
        assert_eq!(pick(&candidates, Strategy::Latency), Some(1));

        // Unknown latency sorts last, so it is only used when nothing else is known.
        let unknown = [proxy("3.3.3.3", true, None)];
        let candidates: Vec<&Proxy> = unknown.iter().collect();
        assert_eq!(pick(&candidates, Strategy::Latency), Some(0));
    }

    #[test]
    fn pick_of_nothing_is_none() {
        let empty: [&Proxy; 0] = [];
        assert_eq!(pick(&empty, Strategy::Random), None);
        assert_eq!(pick(&empty, Strategy::Latency), None);
    }
}
