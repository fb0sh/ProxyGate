//! Error type shared by every module.

use std::io;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in ProxyGate.
///
/// The variants are deliberately coarse: callers either recover (subscriber and
/// health failures are collected, not returned) or they abort the process with a
/// human readable message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("no proxy available: {0}")]
    NoProxy(String),

    #[error("subscriber `{name}` failed: {message}")]
    Subscriber { name: String, message: String },

    #[error("invalid proxy `{input}`: {reason}")]
    InvalidProxy { input: String, reason: String },

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("{0}")]
    Other(String),
}

/// Human readable description of a `reqwest` failure: classification plus the
/// root cause.
///
/// `reqwest::Error`'s own `Display` stops at "error sending request for url
/// (...)", which hides whether a probe or a subscriber failed because of DNS,
/// a refused connection, a TLS problem or a timeout. Everything that reports an
/// HTTP failure goes through here.
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
    /// Shorthand for `Error::Other` with a formatted message.
    pub fn other(message: impl Into<String>) -> Self {
        Error::Other(message.into())
    }

    /// Shorthand for [`Error::InvalidProxy`].
    pub fn invalid(input: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::InvalidProxy {
            input: input.into(),
            reason: reason.into(),
        }
    }

    /// Process exit code that best describes this failure.
    ///
    /// `3` is reserved for "the pool is empty / nothing usable", which scripts
    /// want to distinguish from a hard error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::NoProxy(_) => 3,
            _ => 1,
        }
    }
}
