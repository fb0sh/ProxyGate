//! ProxyGate — a small, uniform proxy pool with a CLI, a REST API and an HTTP
//! proxy gateway in front of it.
//!
//! ```text
//! Subscribers (http / file / exec)
//!         |
//!         v
//!     Normalizer  ->  Pool  ->  Checker
//!                       |  \
//!                       |   Selector
//!                       v
//!              CLI get / REST API / HTTP gateway
//! ```
//!
//! The whole point of the design is that from the moment a proxy enters the
//! pool, nothing cares where it came from.

pub mod api;
pub mod checker;
pub mod cli;
pub mod config;
pub mod error;
pub mod gateway;
pub mod model;
pub mod pool;
pub mod providers;
pub mod selector;
pub mod state;
pub mod subscriber;
pub mod useragent;

pub use error::{Error, Result};
pub use model::Proxy;
pub use pool::ProxyPool;

/// Version string reported by the CLI, the API and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The agent-facing skill document (`SKILL.md`), embedded at compile time.
///
/// `proxygate skill` prints this verbatim so an agent can read the whole
/// contract — commands, exit codes, API, config, and what not to trust — without
/// a checkout next to the binary.
pub const SKILL: &str = include_str!("../SKILL.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_document_is_usable_on_its_own() {
        // An agent reads this without any other file, so it has to carry the
        // whole contract: frontmatter, the commands, the exit codes, the API.
        assert!(SKILL.starts_with("---\n"), "missing skill frontmatter");
        assert!(SKILL.contains("\nname: proxygate"), "missing skill name");
        assert!(SKILL.contains("description:"), "missing skill description");

        for needle in [
            "proxygate get",
            "proxygate getua",
            "proxygate serve",
            "proxygate genconfig",
            "proxygate providers",
            "curl -x \"$(proxygate get)\"",
            "exit code",
            "/api/v1/getua",
            "health:",
            "require:",
        ] {
            assert!(SKILL.contains(needle), "SKILL.md does not mention {needle}");
        }

        assert!(
            SKILL.len() > 2_000 && SKILL.len() < 20_000,
            "SKILL.md is {} bytes; too thin or too long for an agent to load",
            SKILL.len()
        );
    }
}
