//! 各模块共用的错误类型。

use std::io;

/// 全 crate 通用的结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// ProxyGate 中可能出现的全部错误。
///
/// 这些变体有意做得比较粗：调用方要么自行恢复（订阅源与健康检查的失败会被
/// 收集起来，而不是作为错误返回），要么带着人类可读的信息中止进程。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 配置文件无效，或缺少必需字段。
    #[error("configuration error: {0}")]
    Config(String),

    /// 代理池中没有可用的代理。
    #[error("no proxy available: {0}")]
    NoProxy(String),

    /// 某个订阅源抓取失败。
    #[error("subscriber `{name}` failed: {message}")]
    Subscriber {
        /// 配置里给这个订阅源起的名字。
        name: String,
        /// 面向用户的原因，尽可能带上底层错误（连接被拒绝、HTTP 404 等）。
        message: String,
    },

    /// 输入文本无法解析为合法的代理 URL。
    #[error("invalid proxy `{input}`: {reason}")]
    InvalidProxy {
        /// 原始输入文本，原样回显以便定位是列表里的哪一行。
        input: String,
        /// 判定为非法的原因。
        reason: String,
    },

    /// HTTP 请求失败。
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// 本地文件或 STDIN/STDOUT 的 I/O 失败。
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// URL 解析失败。
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    /// JSON 解析或序列化失败。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML 解析或序列化失败。
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// 其它未分类的错误。
    #[error("{0}")]
    Other(String),
}

/// 以人类可读的方式描述一次 `reqwest` 失败：分类加上根因。
///
/// `reqwest::Error` 自身的 `Display` 只说到
/// "error sending request for url (...)" 为止，看不出某个
/// 探测目标或订阅源是因为 DNS、连接被拒、TLS 问题还是超时而
/// 失败。所有上报 HTTP 失败的地方都会经过这里。
pub fn describe_reqwest_error(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect"
    } else if error.is_body() {
        "body"
    } else {
        "request"
    };

    let mut root: &(dyn std::error::Error + 'static) = error;
    while let Some(source) = std::error::Error::source(root) {
        root = source;
    }

    if root.to_string() == error.to_string() {
        format!("{kind}: {error}")
    } else {
        format!("{kind}: {error} ({root})")
    }
}

impl Error {
    /// `Error::Other` 的简写，接受一个已格式化好的消息。
    pub fn other(message: impl Into<String>) -> Self {
        Error::Other(message.into())
    }

    /// [`Error::InvalidProxy`] 的简写。
    pub fn invalid(input: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::InvalidProxy {
            input: input.into(),
            reason: reason.into(),
        }
    }

    /// 最能描述本次失败的进程退出码。
    ///
    /// `3` 保留给“代理池为空 / 没有可用代理”，脚本可以借此把它与硬错误区分
    /// 开来。
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::NoProxy(_) => 3,
            _ => 1,
        }
    }
}
