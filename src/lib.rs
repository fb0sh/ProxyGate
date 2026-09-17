//! ProxyGate 是一个轻量、统一的代理池。
//!
//! 它把任意代理来源转换成始终就绪的代理条目，并在其上提供 CLI、REST
//! API 与 HTTP 代理网关。
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
//! 数据沿着一条单向管道流动：
//!
//! 订阅源 → 归一化 → 代理池 → 健康检查 → 选择器 → CLI / REST API / 网关
//!
//! 设计的重点在于：代理一旦进入代理池，就没有代码再关心它来自哪个订阅源
//! 或内置来源；后续的检查、选择与分发都只面对这一个统一的代理池。
//!
//! # 快速开始
//!
//! 作为库引入：
//!
//! ```console
//! cargo add proxygate
//! ```
//!
//! 或者直接使用已经安装好的二进制：
//!
//! ```console
//! proxygate genconfig > config.yaml   # 生成带注释的示例配置
//! proxygate refresh                   # 拉取全部订阅源并重建代理池
//! curl -x "$(proxygate get)" https://example.com
//! ```
//!
//! 每个子命令的完整参数见 `proxygate --help`。项目源码与主页：
//! <https://github.com/fb0sh/ProxyGate>。
//!
//! # 模块一览
//!
//! - [`api`]：REST API 的共享状态、处理器与路由。
//! - [`app`]：共享运行时，持有代理池、状态存储、HTTP 客户端与健康检查器。
//! - [`checker`]：健康检查器，按探测目标判定代理是否可用。
//! - [`cli`]：基于 clap 的命令行界面定义。
//! - [`commands`]：每个 CLI 子命令对应的库入口函数。
//! - [`config`]：`config.yaml` 的解析、默认值与校验。
//! - [`error`]：全 crate 共用的错误类型及其退出码映射。
//! - [`gateway`]：转发客户端请求的 HTTP 代理网关。
//! - [`model`]：代理 URL 的解析、归一化与渲染。
//! - [`pool`]：进程内的代理池与轮换状态。
//! - [`providers`]：内置代理来源清单。
//! - [`selector`]：选择器，决定从代理池中挑选哪一个代理。
//! - [`state`]：`state.json` 与缓存文件的落盘。
//! - [`subscriber`]：订阅源的配置、抓取与结果汇总。
//! - [`useragent`]：内置的 User-Agent 池。

// 文档注释是这个 crate 的正式参考（docs.rs 上展示的就是它），所以公开
// API 缺少文档会被当成警告。CI 用 `RUSTDOCFLAGS=-D warnings` 构建文档，
// 遗漏会直接让构建失败。
#![warn(missing_docs)]

pub mod api;
pub mod app;
pub mod checker;
pub mod cli;
pub mod commands;
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

/// CLI、REST API 与日志中报告的版本号字符串。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 面向 agent 的技能文档（`SKILL.md`），在编译期嵌入二进制。
///
/// `proxygate skill` 会原样打印它，因此 agent 无需在旁边放一份源码
/// 检出，就能读到完整约定：命令、退出码、API、配置，以及哪些内容
/// 不可信任。
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
