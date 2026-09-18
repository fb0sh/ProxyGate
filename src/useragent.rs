//! 内置 User-Agent 池。
//!
//! `GET /api/v1/getua` 会返回下列之一：
//! 即内嵌文件 [`assets/user_agents.txt`](../assets/user_agents.txt)：
//! 共 100 个桌面浏览器（Windows、macOS 和 Linux 上的 Chrome、
//! Edge、Firefox、Safari）；移动端 User-Agent 被有意排除在外。
//!
//! 挑选过程均匀且无状态：不轮换，也不记录之前发过什么。
//! 因此同一个字符串可能连续出现两次。
//! 这就是这里的“随机”含义，
//! 也让这个功能不需要代理轮换所需的那套记账。

use std::sync::LazyLock;

use rand::RngExt;

/// 每行一个 User-Agent；`#` 注释和空行会被忽略。
const SOURCE: &str = include_str!("../assets/user_agents.txt");

/// 解析并过滤后的内置 User-Agent 列表。
static USER_AGENTS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    SOURCE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
});

/// 全部内置 User-Agent，按文件顺序排列。
pub fn all() -> &'static [&'static str] {
    &USER_AGENTS
}

/// 内置 User-Agent 的数量。
pub fn count() -> usize {
    USER_AGENTS.len()
}

/// 均匀随机地挑选一个。
///
/// 列表嵌在二进制里并由下面的测试校验，因此这里不会失败。
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
