//! Built-in proxy sources.
//!
//! A curated catalog of public endpoints that hand out proxies. A config can
//! refer to one by name instead of repeating its URL:
//!
//! ```yaml
//! subscribers:
//!   - name: scdn
//!     type: builtin
//!     provider: scdn
//! ```
//!
//! The catalog lives in code (not in the config template) so the endpoints,
//! their payload format and their caveats stay in one place, and so
//! `proxygate providers` can describe them without a checkout. Everything a
//! `builtin` entry does is an HTTP fetch — the catalog only saves you from
//! copying a URL that changes.
//!
//! Adding a provider is one [`Provider`] literal plus a line in
//! `config.example.yaml`; a test keeps the two in sync.

use std::time::Duration;

use crate::config::Format;

/// One curated source.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    /// The id used as `provider:` in config and printed by `proxygate providers`.
    pub name: &'static str,
    /// Endpoint to fetch. Query parameters belong here.
    pub url: &'static str,
    /// How to read the response body.
    pub format: Format,
    /// Documentation or landing page.
    pub homepage: &'static str,
    /// One line of operational reality, shown by `proxygate providers`.
    pub notes: &'static str,
    /// Timeout for fetching this endpoint, when the default `refresh.timeout`
    /// is too short for it. Overridable per config entry.
    pub timeout: Option<Duration>,
    /// Keep at most this many usable proxies from this source, taking them in
    /// the order the endpoint returns them (the big lists are ordered
    /// fastest-first, so this keeps the useful end). `None` means no cap.
    ///
    /// This exists because the health checker has to probe every proxy it is
    /// given: 16,000 of them is a twelve minute pass at the default
    /// concurrency. Overridable per config entry (0 = no cap).
    pub limit: Option<usize>,
}

/// Every built-in source, in the order `proxygate providers` prints them.
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

/// Default cap when a config asks for `limit: 0` (unlimited) — used by tests.
pub const NO_LIMIT: Option<usize> = None;

/// Looks a provider up by name.
pub fn find(name: &str) -> Option<&'static Provider> {
    ALL.iter().find(|provider| provider.name == name)
}

/// Every provider name, for error messages.
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
