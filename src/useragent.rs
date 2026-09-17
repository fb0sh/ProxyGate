//! Built-in user agent pool.
//!
//! `proxygate getua` and `GET /api/v1/getua` hand out one of the user agents
//! embedded in [`assets/user_agents.txt`](../assets/user_agents.txt): 100
//! desktop browsers (Chrome, Edge, Firefox, Safari on Windows, macOS and
//! Linux). Mobile agents are deliberately not included.
//!
//! The pick is uniform and stateless: no rotation, no memory of what was handed
//! out before, so the same string can come up twice in a row. That is what
//! "random" means here, and it keeps the feature free of the bookkeeping the
//! proxy rotation needs.

use std::sync::LazyLock;

use rand::RngExt;

/// One user agent per line; `#` comments and blank lines are ignored.
const SOURCE: &str = include_str!("../assets/user_agents.txt");

static USER_AGENTS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    SOURCE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
});

/// Every built-in user agent, in file order.
pub fn all() -> &'static [&'static str] {
    &USER_AGENTS
}

/// How many user agents are built in.
pub fn count() -> usize {
    USER_AGENTS.len()
}

/// Picks one at random, uniformly.
///
/// The list is embedded in the binary and validated by the tests below, so this
/// cannot fail.
pub fn random() -> &'static str {
    let pool = all();
    debug_assert!(!pool.is_empty(), "the embedded user agent list is empty");
    pool[rand::rng().random_range(0..pool.len())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ships_one_hundred_agents() {
        assert_eq!(count(), 100, "the pool is advertised as 100 agents");
    }

    #[test]
    fn comments_and_blank_lines_are_not_agents() {
        assert!(
            all().iter().all(|agent| !agent.starts_with('#')),
            "a comment leaked into the pool"
        );
        assert!(all().iter().all(|agent| !agent.is_empty()));
        // The file *does* carry comments; they must be stripped, not counted.
        assert!(SOURCE.lines().any(|line| line.starts_with('#')));
        assert_eq!(
            SOURCE
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            count() + SOURCE.lines().filter(|line| line.starts_with('#')).count()
        );
    }

    #[test]
    fn every_agent_is_distinct_and_plausible() {
        let unique: HashSet<&&str> = all().iter().collect();
        assert_eq!(
            unique.len(),
            all().len(),
            "duplicate user agents in the file"
        );

        for agent in all() {
            assert!(
                agent.starts_with("Mozilla/5.0 ("),
                "not a user agent: {agent}"
            );
            assert!(
                !agent.contains('\n') && !agent.contains('\r'),
                "a line was not split cleanly: {agent:?}"
            );
            assert!(
                (60..=255).contains(&agent.len()),
                "implausible length ({}): {agent}",
                agent.len()
            );
        }
    }

    #[test]
    fn the_pool_covers_the_desktop_platforms() {
        let joined = all().join("\n");
        for marker in [
            "Windows NT",
            "Macintosh",
            "X11; Linux",
            "Chrome/",
            "Edg/",
            "Firefox/",
            "Safari/",
        ] {
            assert!(joined.contains(marker), "the pool has no {marker}");
        }
    }

    #[test]
    fn the_pool_is_desktop_only() {
        for agent in all() {
            for marker in ["Mobile", "Android", "iPhone", "iPad", "iPod"] {
                assert!(
                    !agent.contains(marker),
                    "a mobile user agent is in a desktop-only pool: {agent}"
                );
            }
            assert!(
                !agent.contains("SamsungBrowser/"),
                "Samsung Internet is mobile-only: {agent}"
            );
        }
    }

    #[test]
    fn random_reaches_different_agents() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            let agent = random();
            assert!(all().contains(&agent), "random() returned an outsider");
            seen.insert(agent);
        }
        assert!(
            seen.len() > 50,
            "500 draws over 100 agents should cover far more than {}",
            seen.len()
        );
    }
}
