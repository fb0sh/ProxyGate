//! Command line interface.
//!
//! Five subcommands, no more: `get`, `list`, `refresh`, `check`, `serve`.
//!
//! `get` writes exactly one line — the proxy URL — to stdout, so it composes:
//!
//! ```text
//! curl -x "$(proxygate get)" https://example.com
//! ```

use std::path::PathBuf;

use clap::builder::PossibleValue;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::selector::Strategy;

/// Turn any proxy source into a uniform, always-ready proxy pool.
#[derive(Debug, Parser)]
#[command(
    name = "proxygate",
    version,
    about = "Turn any proxy source into a uniform, always-ready proxy pool",
    long_about = None,
    disable_help_subcommand = true,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to config.yaml (default: ./config.yaml, then ~/.config/proxygate/config.yaml)
    #[arg(
        short,
        long,
        global = true,
        env = "PROXYGATE_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// Increase log verbosity: -v for info, -vv for debug, -vvv for trace
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Only log errors
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print one healthy proxy URL to stdout
    Get(GetArgs),
    /// List the proxies currently in the pool
    List(ListArgs),
    /// Fetch every subscriber and rebuild the pool
    Refresh(RefreshArgs),
    /// Probe the health of the proxies in the pool
    Check(CheckArgs),
    /// Run the HTTP proxy gateway and the REST API
    Serve(ServeArgs),
    /// Print the annotated example config to stdout
    Genconfig,
    /// Print a random user agent from the built-in pool
    Getua(GetUaArgs),
    /// Print the agent-facing skill document (SKILL.md) to stdout
    Skill,
    /// List the built-in proxy sources and how to enable them
    Providers(ProvidersArgs),
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Selection strategy for this call
    #[arg(long, value_enum, value_name = "STRATEGY")]
    pub strategy: Option<Strategy>,

    /// Do not fetch subscribers; work with the cached pool
    #[arg(long)]
    pub no_refresh: bool,

    /// Do not probe proxies; trust the cached health results
    #[arg(long)]
    pub no_check: bool,

    /// Print the proxy with credentials masked
    #[arg(long)]
    pub mask: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only show healthy proxies
    #[arg(long)]
    pub alive: bool,

    /// Also show proxies that failed their last check (default)
    #[arg(long, conflicts_with = "alive")]
    pub all: bool,

    /// Show credentials instead of `***:***`
    #[arg(long)]
    pub show_auth: bool,

    /// Machine readable output
    #[arg(long)]
    pub json: bool,

    /// Do not probe proxies; trust the cached health results
    #[arg(long)]
    pub no_check: bool,

    /// Do not fetch subscribers; work with the cached pool
    #[arg(long)]
    pub no_refresh: bool,
}

#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Machine readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Machine readable output
    #[arg(long)]
    pub json: bool,

    /// Override health.concurrency for this run
    #[arg(long, value_name = "N")]
    pub concurrency: Option<usize>,

    /// Only probe proxies that are currently alive (skip known-dead ones)
    #[arg(long)]
    pub alive_only: bool,
}

#[derive(Debug, Args)]
pub struct ProvidersArgs {
    /// Machine readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GetUaArgs {
    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address for the HTTP proxy gateway (default: config or 127.0.0.1:8080)
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,

    /// Address for the REST API (default: config or 127.0.0.1:8081)
    #[arg(long, value_name = "ADDR")]
    pub api: Option<String>,

    /// Require `user:password` from gateway clients
    #[arg(long, value_name = "USER:PASS")]
    pub auth: Option<String>,

    /// Do not fetch subscribers at startup; work with the cached pool
    #[arg(long)]
    pub no_refresh: bool,
}

/// stdout format of `proxygate get`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Just the proxy URL, one line
    Text,
    /// `{"proxy": "...", "latency_ms": 83}`
    Json,
}

impl ValueEnum for Strategy {
    fn value_variants<'a>() -> &'a [Self] {
        &Strategy::ALL
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_get_with_flags() {
        let cli = Cli::try_parse_from([
            "proxygate",
            "--config",
            "/tmp/x.yaml",
            "-vv",
            "get",
            "--format",
            "json",
            "--strategy",
            "latency",
            "--no-check",
        ])
        .unwrap();

        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.yaml")));
        assert_eq!(cli.verbose, 2);
        match cli.command {
            Command::Get(args) => {
                assert_eq!(args.format, OutputFormat::Json);
                assert_eq!(args.strategy, Some(Strategy::Latency));
                assert!(args.no_check);
                assert!(!args.no_refresh);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn serve_defaults_to_config_addresses() {
        let cli = Cli::try_parse_from(["proxygate", "serve"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert!(args.listen.is_none());
                assert!(args.api.is_none());
                assert!(args.auth.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_strategy() {
        assert!(Cli::try_parse_from(["proxygate", "get", "--strategy", "random"]).is_ok());
        assert!(Cli::try_parse_from(["proxygate", "get", "--strategy", "latency"]).is_ok());
        assert!(Cli::try_parse_from(["proxygate", "get", "--strategy", "nope"]).is_err());
    }
}
