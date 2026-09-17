//! 进度事件：库只发事件，显示方式由调用方决定。
//!
//! 抓取订阅源和探测代理都可能跑几分钟（`freeproxy-gh` 那个 2.5 MB 的列表
//! 一次要四分钟），中间什么都不打印是没法用的。但库不该自己往终端写东西，
//! 所以这里只有事件和一个接收器 trait：
//!
//! * 命令行把事件渲染成给人看的进度（[`crate::commands`] 里的
//!   `ConsoleProgress`，写到 stderr，所以不影响 stdout 的数据契约）；
//! * `serve` 把事件转成日志（[`crate::app::LogProgress`]）；
//! * 测试与库内部用 `()`，什么都不做。

use std::time::Duration;

use crate::config::Format;
use crate::subscriber::FetchOutcome;

/// 抓取订阅源过程中的事件。
#[derive(Debug)]
pub enum FetchEvent<'a> {
    /// 开始拉取一个订阅源。
    Started {
        /// 订阅源名称（内置来源是目录里的 id，分页来源带 `#页码`）。
        name: &'a str,
        /// 订阅源类型：`http`、`file`、`exec` 或 `builtin`。
        kind: &'static str,
        /// 这次拉取使用的解析格式。
        format: Format,
    },
    /// 正在下载响应体，按固定间隔发出；只有 HTTP 来源会有。
    Download {
        /// 订阅源名称。
        name: &'a str,
        /// 已经读到的字节数。
        bytes: u64,
        /// 从开始拉起到现在的耗时。
        elapsed: Duration,
    },
    /// 拉取结束。成功与失败都在这里，用 [`FetchOutcome::ok`] 区分。
    Finished(&'a FetchOutcome),
}

/// 健康探测的进度，按固定间隔发出。
#[derive(Debug, Clone, Copy)]
pub struct CheckEvent {
    /// 已经探测完的代理数。
    pub done: usize,
    /// 本轮要探测的总数。
    pub total: usize,
    /// 到目前为止存活的代理数。
    pub alive: usize,
    /// 本轮已耗时。
    pub elapsed: Duration,
}

/// 进度接收器。
///
/// 两个方法都有默认空实现，只关心其中一种的实现者不必写另一个。
///
/// `Send + Sync` 是必需的：`serve` 的刷新循环跑在 `tokio::spawn` 出来的
/// 任务里，事件接收器要能跨线程共享。
pub trait Progress: Send + Sync {
    /// 收到一个抓取事件。
    fn fetch(&self, event: FetchEvent<'_>) {
        let _ = event;
    }

    /// 收到一次探测进度。
    fn check(&self, event: CheckEvent) {
        let _ = event;
    }
}

/// 什么都不做的接收器：库内部的默认值。
impl Progress for () {}
