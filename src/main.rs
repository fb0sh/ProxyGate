//! `proxygate` 二进制：解析命令行并交给库处理。
//!
//! 所有行为都实现在库中，以便被文档化与复用——本文件只负责接好日志、
//! 分发与退出码。

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use proxygate::cli::Cli;
use proxygate::commands;

/// 程序入口：解析命令行、初始化日志，然后把命令分发给库。
///
/// 出错时把人类可读的错误写到 STDERR，并以错误映射出的退出码结束
/// 进程，见 [`Error::exit_code`](proxygate::Error::exit_code)。
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli);

    match commands::dispatch(cli).await {
        Ok(code) => code,
        Err(error) => {
            // stdin/stdout belong to the caller (`proxygate get` prints exactly
            // one line there), so diagnostics always go to stderr.
            eprintln!("proxygate: error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// 初始化 STDERR 日志。
///
/// `proxygate get` 需要保持 STDOUT 干净，只留 URL。
fn init_tracing(cli: &Cli) {
    let default_level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
