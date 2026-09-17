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

/// 从候选集合中挑选代理的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// 均匀随机挑选，把负载分散到整个代理池。
    #[default]
    Random,
    /// 选择测量延迟最低的代理。
    Latency,
}

impl Strategy {
    /// 所有可用的选择策略。
    pub const ALL: [Strategy; 2] = [Strategy::Random, Strategy::Latency];

    /// 策略的规范小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Strategy::Random => "random",
            Strategy::Latency => "latency",
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
                "unknown selection strategy `{other}` (expected random or latency)"
            ))),
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 对代理池快照应用轮换规则的结果。
#[derive(Debug)]
pub struct Plan<'a> {
    /// 可以被分发的代理，优先级最高的在最前。
    pub candidates: Vec<&'a Proxy>,
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
pub fn plan<'a>(
    proxies: &'a [Proxy],
    generation: u64,
    reuse_after: Duration,
    now: SystemTime,
) -> Plan<'a> {
    let healthy: Vec<&Proxy> = proxies.iter().filter(|proxy| proxy.alive).collect();
    let unused_this_round: Vec<&Proxy> = healthy
        .iter()
        .copied()
        .filter(|proxy| proxy.generation < generation)
        .collect();

    let healthy_count = healthy.len();
    let used_this_round = healthy_count - unused_this_round.len();

    let (generation, reset_round, pool): (u64, bool, Vec<&Proxy>) =
        if unused_this_round.is_empty() && !healthy.is_empty() {
            // Every healthy proxy has been handed out: start the next round right
            // away, even if `reuse_after` has not elapsed yet.
            (generation.saturating_add(1), true, healthy)
        } else {
            (generation, false, unused_this_round)
        };

    // Second tier: proxies that were not used within the reuse window.
    let not_recent: Vec<&Proxy> = pool
        .iter()
        .copied()
        .filter(|proxy| !used_recently(proxy, reuse_after, now))
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
pub fn used_recently(proxy: &Proxy, reuse_after: Duration, now: SystemTime) -> bool {
    proxy
        .last_used_at
        .map(|last| elapsed(last, now) < reuse_after)
        .unwrap_or(false)
}

/// 计算从 `from` 到 `now` 经过的时间，时钟回拨时返回零。
fn elapsed(from: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(from).unwrap_or(Duration::ZERO)
}

/// 从候选集合中挑选一个下标。
pub fn pick(candidates: &[&Proxy], strategy: Strategy) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    match strategy {
        Strategy::Random => Some(rand::rng().random_range(0..candidates.len())),
        Strategy::Latency => candidates
            .iter()
            .enumerate()
            .min_by_key(|(_, proxy)| proxy.latency.unwrap_or(Duration::MAX))
            .map(|(index, _)| index),
    }
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
    fn ignores_dead_proxies() {
        let proxies = vec![proxy("1.1.1.1", false, None), proxy("2.2.2.2", true, None)];
        let plan = plan(&proxies, 1, Duration::from_secs(1800), SystemTime::now());
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].host(), "2.2.2.2");
        assert!(!plan.reset_round);
    }

    #[test]
    fn empty_pool_yields_no_candidates_and_no_reset() {
        let plan = plan(&[], 1, Duration::from_secs(1800), SystemTime::now());
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
        assert_eq!(pick(&[], Strategy::Random), None);
        assert_eq!(pick(&[], Strategy::Latency), None);
    }
}
