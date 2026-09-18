//! `proxygate` 二进制：启动服务。
//!
//! 这里没有命令行客户端——一切通过 HTTP。本文件只做三件事：认两个无副作用的
//! 开关（`--version` / `--help`）、初始化日志、调用
//! [`proxygate::server::run`]。
//!
//! 配置来自 `$PROXYGATE_CONFIG` 或默认查找路径（`./config.yaml`、
//! `~/.config/proxygate/config.yaml`），日志级别来自 `RUST_LOG`
//! （未设置时用 `info`：服务端没有别的进度显示渠道）。

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

/// 程序入口。
#[tokio::main]
async fn main() -> ExitCode {
    // 只认两个开关，不引入命令行的其余概念。`--version` 让 CI 与打包脚本
    // 能便宜地冒烟测试；`--help` 指向真正的手册（`GET /help`）。
    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        match flag.as_str() {
            "--version" | "-V" => {
                println!("proxygate {}", proxygate::VERSION);
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "proxygate {} — 服务端程序，没有命令行客户端。\n\
                     \n\
                     用法：直接运行（无参数）即按配置启动网关与 REST API。\n\
                     配置：$PROXYGATE_CONFIG，或 ./config.yaml、~/.config/proxygate/config.yaml\n\
                     日志：$RUST_LOG（默认 info）\n\
                     文档：服务起来之后 GET /help（markdown，同样适合 agent 读）\n",
                    proxygate::VERSION
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!(
                    "proxygate: unknown argument `{other}`; this program takes no arguments \
                     (see --help). Configuration comes from a config file."
                );
                return ExitCode::from(2);
            }
        }
    }

    init_tracing();

    if let Err(error) = proxygate::server::run(None).await {
        eprintln!("proxygate: error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// 初始化 STDERR 日志。
///
/// 默认 `info`：服务端没有 stdout 契约要保护，而抓取、发放验证、探测的进度
/// 都在日志里。
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
