//! 内置代理来源。
//!
//! 一份精心整理的公开端点目录，这些端点会分发代理。配置里可以按名字引用
//! 某个来源，而不必重复它的 URL：
//!
//! ```yaml
//! subscribers:
//!   - name: scdn
//!     type: builtin
//!     provider: scdn
//! ```
//!
//! 目录放在代码里（而不是配置模板里），这样端点、它们的载荷格式和注意事项
//! 都集中在一处，`proxygate providers` 也无需检出代码就能描述它们。`builtin`
//! 条目所做的一切都是一次 HTTP 拉取——目录只是省去了复制一个会变化的 URL。
//!
//! 新增一个内置来源＝一条 [`Provider`] 字面量，再加 `config.example.yaml`
//! 里的一行；有测试保证两者同步。

use std::time::Duration;

use crate::config::Format;

/// 一个内置来源。
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// 在配置中作为 `provider:` 使用、并由 `proxygate providers`
    /// 打印出来的 id。
    pub name: &'static str,
    /// 要拉取的端点。查询参数也写在这里。
    pub url: &'static str,
    /// 如何读取响应体。
    pub format: Format,
    /// 文档或落地页。
    pub homepage: &'static str,
    /// 一行关于实际使用情况的说明，由 `proxygate providers` 展示。
    pub notes: &'static str,
    /// 拉取该端点时的超时；当默认的 `refresh.timeout` 不够长时使用。
    /// 可在每个配置条目上覆盖。
    pub timeout: Option<Duration>,
    /// 最多保留这么多个来自该来源的可用代理，按端点返回的顺序取（大型列表
    /// 以最快优先排序，因此取到的是有用的一端）。`None` 表示不限。
    ///
    /// 之所以有这个字段，是因为健康检查必须探测拿到的每一个代理：在默认
    /// 并发下，16000 条要跑十二分钟。可在每个配置条目上覆盖（0 = 不限）。
    pub limit: Option<usize>,
}

/// 所有内置来源，顺序即 `proxygate providers` 的打印顺序。
pub const ALL: &[Provider] = &[
    Provider {
        name: "scdn",
        url: "https://proxy.scdn.io/api/get_proxy.php?protocol=http&count=20",
        format: Format::Json,
        homepage: "https://proxy.scdn.io/api_docs.php",
        notes: "small and fast; rate limits, so keep refresh.interval at 10m or slower",
        timeout: None,
        limit: None,
    },
    Provider {
        name: "freeproxy-cn",
        url: "https://www.freeproxy.com.cn/proxy.json",
        format: Format::Json,
        homepage: "https://www.freeproxy.com.cn/",
        notes: "~120 entries, all HTTP; carries country/region/anonymity per entry",
        timeout: None,
        limit: None,
    },
    Provider {
        name: "rola-ip",
        url: "https://rola-ip.co/proxy-api/api/v1/proxies?page=1&pageSize=500",
        format: Format::Json,
        homepage: "https://rola-ip.co/",
        // pageSize tops out at 500 and page 1 is the largest slice; the whole
        // list is 4,685 over 10 pages. socks4-only entries are skipped, and
        // socks5 ones become socks5h, so the proxy resolves names itself.
        notes: "500 per request, 60 req/min; mixes http, socks5 and socks4, plus transparent entries",
        timeout: None,
        limit: None,
    },
    Provider {
        name: "freeproxy-gh",
        url: "https://charlespikachu.github.io/freeproxy/proxies.json",
        format: Format::Json,
        homepage: "https://github.com/charlespikachu/freeproxy",
        // ~2.5 MB and ~16k entries, ordered fastest-first, served by GitHub
        // Pages. Measured at ~14 KB/s from a slow link, so the whole body takes
        // about three minutes: the timeout has to cover the download, and the
        // cap keeps a health pass in the tens of seconds instead of twelve
        // minutes. Raise `timeout` per config entry if it still times out.
        notes: "~16k entries (mostly socks5), 2.5 MB download taking minutes; capped at 1000",
        timeout: Some(Duration::from_secs(300)),
        limit: Some(1000),
    },
];

/// 当配置要求 `limit: 0`（不限）时使用的默认上限——测试会用到。
pub const NO_LIMIT: Option<usize> = None;

/// 按名称查找一个内置来源。
pub fn find(name: &str) -> Option<&'static Provider> {
    ALL.iter().find(|provider| provider.name == name)
}

/// 所有来源名称，用于错误信息。
pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|provider| provider.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_provider_is_described_completely() {
        assert!(
            !ALL.is_empty(),
            "a catalog with no entries is not a catalog"
        );

        for provider in ALL {
            assert!(!provider.name.is_empty());
            assert!(
                provider
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "provider names are lowercase ids, not `{}`",
                provider.name
            );
            assert!(
                provider.url.starts_with("https://") || provider.url.starts_with("http://"),
                "{} has no usable endpoint: {}",
                provider.name,
                provider.url
            );
            assert!(
                provider.homepage.starts_with("https://"),
                "{} should document where it comes from",
                provider.name
            );
            assert!(
                !provider.notes.trim().is_empty(),
                "{} needs a note about its limits",
                provider.name
            );
            if let Some(limit) = provider.limit {
                assert!(limit > 0, "{} has a zero limit", provider.name);
            }
            assert!(
                !provider.url.contains(char::is_whitespace),
                "{} has whitespace in its URL",
                provider.name
            );
        }
    }

    #[test]
    fn names_are_unique_and_lookup_works() {
        let names: HashSet<&str> = ALL.iter().map(|provider| provider.name).collect();
        assert_eq!(names.len(), ALL.len(), "duplicate provider name");

        for provider in ALL {
            let found = find(provider.name).expect("every catalog entry is findable");
            assert_eq!(found.url, provider.url);
        }
        assert!(find("no-such-provider").is_none());
        assert!(find("").is_none());
        assert!(
            find("SCDN").is_none(),
            "lookup is case sensitive on purpose"
        );
    }

    #[test]
    fn the_catalog_entries_parse_as_urls() {
        for provider in ALL {
            let url =
                url::Url::parse(provider.url).unwrap_or_else(|e| panic!("{}: {e}", provider.name));
            assert!(matches!(url.scheme(), "http" | "https"));
            assert!(url.host_str().is_some());
        }
    }
}
