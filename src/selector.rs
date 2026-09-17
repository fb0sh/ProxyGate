//! Candidate filtering and selection strategies.
//!
//! The rotation rules live in [`plan`]:
//!
//! * only healthy proxies are ever handed out;
//! * a proxy that was already used in the current round is skipped;
//! * among the remaining ones, proxies unused within `reuse_after` win;
//! * when the whole healthy pool has been used, the round is incremented
//!   immediately instead of waiting for `reuse_after` to expire.

use std::time::{Duration, SystemTime};

use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::Proxy;

/// How a proxy is chosen from the candidate set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Uniform pick — spreads load across the pool.
    #[default]
    Random,
    /// Lowest measured latency.
    Latency,
}

impl Strategy {
    pub const ALL: [Strategy; 2] = [Strategy::Random, Strategy::Latency];

    pub const fn as_str(self) -> &'static str {
        match self {
            Strategy::Random => "random",
            Strategy::Latency => "latency",
        }
    }
}

impl std::str::FromStr for Strategy {
    type Err = Error;

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

/// The result of applying the rotation rules to a pool snapshot.
#[derive(Debug)]
pub struct Plan<'a> {
    /// Proxies that may be handed out, best tier first.
    pub candidates: Vec<&'a Proxy>,
    /// Round the selection belongs to (may be the current round plus one).
    pub generation: u64,
    /// True when the caller must advance the pool generation.
    pub reset_round: bool,
    /// Healthy proxies seen in the snapshot.
    pub healthy: usize,
    /// Healthy proxies that were already used in the current round.
    pub used_this_round: usize,
}

/// Applies the rotation rules. Pure function: no locking, no clock reads.
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

/// True when the proxy was handed out within the reuse window.
pub fn used_recently(proxy: &Proxy, reuse_after: Duration, now: SystemTime) -> bool {
    proxy
        .last_used_at
        .map(|last| elapsed(last, now) < reuse_after)
        .unwrap_or(false)
}

fn elapsed(from: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(from).unwrap_or(Duration::ZERO)
}

/// Picks one index out of the candidate set.
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
