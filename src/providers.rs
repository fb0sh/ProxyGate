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
}

/// Every built-in source, in the order they appear in the example config.
pub const ALL: &[Provider] = &[Provider {
    name: "scdn",
    url: "https://proxy.scdn.io/api/get_proxy.php?protocol=http&count=20",
    format: Format::Json,
    homepage: "https://proxy.scdn.io/api_docs.php",
    notes: "free list, rate limits; bare host:port entries are read as HTTP proxies",
}];

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
