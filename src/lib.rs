//! ProxyGate 是一个轻量、统一的代理池。
//!
//! 它把任意代理来源转换成始终就绪的代理条目，并在其上提供 REST API 与
//! HTTP 代理网关。这是个**服务端程序**：跑起来之后所有操作都通过 HTTP，
//! 没有命令行客户端（`GET /help` 返回本 crate 里的 `SKILL.md`）。
//!
//! ```text
//! Subscribers (http / file / exec)
//!         |
//!         v
//!     Normalizer  ->  Pool  ->  Checker
//!                       |  \
//!                       |   Selector
//!                       v
//!            REST API / HTTP proxy gateway
//! ```
//!
//! 数据沿着一条单向管道流动：
//!
//! 订阅源 → 归一化 → 代理池 → 健康检查 → 选择器 → REST API / 网关
//!
//! 设计的重点在于：代理一旦进入代理池，就没有代码再关心它来自哪个订阅源
//! 或内置来源；后续的检查、选择与分发都只面对这一个统一的代理池。
//!
//! # 快速开始
//!
//! 作为库引入，自己决定怎么跑：
//!
//! ```console
//! cargo add proxygate
//! ```
//!
//! 或者直接用发出去的二进制（它只做一件事：按配置启动服务）：
//!
//! ```console
//! curl -s http://127.0.0.1:8081/api/v1/config > config.yaml   # 带注释的示例配置
//! proxygate                                                  # 启动，无参数
//! curl -sf http://127.0.0.1:8081/api/v1/get                  # 拿一个代理
//! curl -x "$(curl -sf http://127.0.0.1:8081/api/v1/get)" https://example.com
//! ```
//!
//! 完整端点列表见 `GET /help`，也就是本仓库的 `SKILL.md`。
//! 项目源码与主页：<https://github.com/fb0sh/ProxyGate>。
//!
//! # 模块一览
//!
//! - [`api`]：REST API 的共享状态、处理器与路由。
//! - [`app`]：共享运行时，持有代理池、状态存储、HTTP 客户端与健康检查器。
//! - [`checker`]：健康检查器，按探测目标判定代理是否可用。
//! - [`config`]：`config.yaml` 的解析、默认值与校验。
//! - [`error`]：全 crate 共用的错误类型及其退出码映射。
//! - [`gateway`]：转发客户端请求的 HTTP 代理网关。
//! - [`model`]：代理 URL 的解析、归一化与渲染。
//! - [`pool`]：进程内的代理池与轮换状态。
//! - [`progress`]：抓取与探测的进度事件（库只发事件，显示由调用方决定）。
//! - [`providers`]：内置代理来源清单。
//! - [`selector`]：选择器，决定从代理池中挑选哪一个代理。
//! - [`server`]：把网关、REST API 与后台循环跑起来（程序的唯一入口）。
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
pub mod config;
pub mod error;
pub mod gateway;
pub mod model;
pub mod pool;
pub mod progress;
pub mod providers;
pub mod selector;
pub mod server;
pub mod state;
pub mod subscriber;
pub mod useragent;

pub use error::{Error, Result};
pub use model::Proxy;
pub use pool::ProxyPool;

/// REST API、日志与 `--version` 报告的版本号字符串。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 面向 agent 与人的手册（`SKILL.md`），在编译期嵌入二进制。
///
/// `GET /help` 原样返回它，因此 agent 或人类不需要一份源码检出，就能读到
/// 完整约定：端点、状态码、配置、以及哪些内容不可信任。
pub const SKILL: &str = include_str!("../SKILL.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_document_is_usable_on_its_own() {
        // An agent reads this without any other file, so it has to carry the
        // whole contract: frontmatter, the commands, the exit codes, the API.
        //
        // `.gitattributes` pins the checkout to LF, but the assertions still
        // normalise: a CRLF checkout (a source archive built without that file,
        // a contributor with `core.autocrlf=true`) should not fail a test about
        // the document's *content*.
        let skill = SKILL.replace("\r\n", "\n");
        assert!(skill.starts_with("---\n"), "missing skill frontmatter");
        assert!(skill.contains("\nname: proxygate"), "missing skill name");
        assert!(skill.contains("description:"), "missing skill description");

        for needle in [
            "/api/v1/get",
            "/api/v1/getua",
            "/api/v1/refresh",
            "/api/v1/check",
            "/api/v1/providers",
            "/api/v1/config",
            "/help",
            "curl -sf",
            "curl -x \"$(curl -sf",
            "Status codes",
            "selection:",
            "require:",
        ] {
            assert!(skill.contains(needle), "SKILL.md does not mention {needle}");
        }

        assert!(
            skill.len() > 2_000 && skill.len() < 20_000,
            "SKILL.md is {} bytes; too thin or too long for an agent to load",
            skill.len()
        );
    }
}
