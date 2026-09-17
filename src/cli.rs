//! 命令行界面。
//!
//! 核心子命令只有五个：`get`、`list`、`refresh`、`check`、
//! `serve`；此外还有 `genconfig`、`getua`、`skill`、`providers`
//! 四个辅助子命令。
//!
//! `get` 只往 STDOUT 写一行，也就是代理 URL，因此很容易组合使用：
//!
//! ```text
//! curl -x "$(proxygate get)" https://example.com
//! ```

use std::path::PathBuf;

use clap::builder::PossibleValue;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::selector::Strategy;

/// 把任意代理来源变成统一、始终就绪的代理池。
#[derive(Debug, Parser)]
#[command(
    name = "proxygate",
    version,
    about = "把任意代理来源变成统一、始终就绪的代理池",
    long_about = None,
    disable_help_subcommand = true,
    propagate_version = true
)]
pub struct Cli {
    /// 要执行的子命令。
    #[command(subcommand)]
    pub command: Command,

    /// config.yaml 的路径。
    ///
    /// 默认先找 ./config.yaml，再找
    /// ~/.config/proxygate/config.yaml。
    #[arg(
        short,
        long,
        global = true,
        env = "PROXYGATE_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// 提高日志详细程度：-v 为 info，-vv 为 debug，-vvv 为 trace。
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// 只记录错误日志，并关闭抓取/探测的进度输出。
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// 抓取时把每个拿到的代理都打印出来（默认只给几个样例）。
    ///
    /// 进度与代理列表都写到 stderr，所以不会污染 stdout。
    #[arg(long, global = true)]
    pub proxies: bool,
}

/// 全部子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 向 STDOUT 打印一个健康代理的 URL。
    Get(GetArgs),
    /// 列出当前代理池中的代理。
    List(ListArgs),
    /// 抓取全部订阅源并重建代理池。
    Refresh(RefreshArgs),
    /// 探测代理池中代理的健康状态。
    Check(CheckArgs),
    /// 运行 HTTP 代理网关与 REST API。
    Serve(ServeArgs),
    /// 向 STDOUT 打印带注释的示例配置。
    Genconfig,
    /// 从内置池中打印一个随机 User-Agent。
    Getua(GetUaArgs),
    /// 向 STDOUT 打印面向 agent 的技能文档（SKILL.md）。
    Skill,
    /// 列出内置代理来源以及如何启用它们。
    Providers(ProvidersArgs),
}

/// `get` 的参数。
#[derive(Debug, Args)]
pub struct GetArgs {
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// 本次调用使用的选择策略。
    #[arg(long, value_enum, value_name = "STRATEGY")]
    pub strategy: Option<Strategy>,

    /// 不抓取订阅源，直接使用缓存的代理池。
    #[arg(long)]
    pub no_refresh: bool,

    /// 不探测代理，直接信任缓存的健康检查结果。
    #[arg(long)]
    pub no_check: bool,

    /// 打印代理时对凭据脱敏。
    #[arg(long)]
    pub mask: bool,
}

/// `list` 的参数。
#[derive(Debug, Args)]
pub struct ListArgs {
    /// 只显示健康代理。
    #[arg(long)]
    pub alive: bool,

    /// 同时显示上次检查失败的代理（默认行为）。
    #[arg(long, conflicts_with = "alive")]
    pub all: bool,

    /// 显示真实凭据，而不是 `***:***`。
    #[arg(long)]
    pub show_auth: bool,

    /// 机器可读的输出。
    #[arg(long)]
    pub json: bool,

    /// 不探测代理，直接信任缓存的健康检查结果。
    #[arg(long)]
    pub no_check: bool,

    /// 不抓取订阅源，直接使用缓存的代理池。
    #[arg(long)]
    pub no_refresh: bool,
}

/// `refresh` 的参数。
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// 机器可读的输出。
    #[arg(long)]
    pub json: bool,
}

/// `check` 的参数。
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// 机器可读的输出。
    #[arg(long)]
    pub json: bool,

    /// 本次运行覆盖 health.concurrency。
    #[arg(long, value_name = "N")]
    pub concurrency: Option<usize>,

    /// 只探测当前存活的代理（跳过已知失效的代理）。
    #[arg(long)]
    pub alive_only: bool,
}

/// `providers` 的参数。
#[derive(Debug, Args)]
pub struct ProvidersArgs {
    /// 机器可读的输出。
    #[arg(long)]
    pub json: bool,
}

/// `getua` 的参数。
#[derive(Debug, Args)]
pub struct GetUaArgs {
    /// 输出格式。
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// `serve` 的参数。
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// HTTP 代理网关的监听地址。
    ///
    /// 默认取配置，或 127.0.0.1:8080。
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<String>,

    /// REST API 的监听地址；用 `same` 表示与代理共用一个端口。
    ///
    /// 默认取配置，或 127.0.0.1:8081。
    #[arg(long, value_name = "ADDR")]
    pub api: Option<String>,

    /// 要求网关客户端提供 `user:password`。
    #[arg(long, value_name = "USER:PASS")]
    pub auth: Option<String>,

    /// 启动时不抓取订阅源，直接使用缓存的代理池。
    #[arg(long)]
    pub no_refresh: bool,
}

/// `proxygate get` 的 STDOUT 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// 只输出代理 URL，一行。
    Text,
    /// JSON 形式：`{"proxy": "...", "latency_ms": 83}`。
    Json,
}

impl ValueEnum for Strategy {
    /// 返回全部可选策略。
    fn value_variants<'a>() -> &'a [Self] {
        &Strategy::ALL
    }

    /// 把策略映射为 clap 的取值。
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
