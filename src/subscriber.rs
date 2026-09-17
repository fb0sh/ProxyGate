//! 订阅源：代理从哪里来。
//!
//! 只有三种类型：
//!
//! * `http` —— 拉取一个 URL（`proxies.txt`、API 或订阅）；
//! * `file` —— 读取本地文件；
//! * `exec` —— 运行一条命令并读取其 stdout。
//!
//! 无论载荷长什么样，订阅源唯一的职责就是产出代理 URL。内置解析器覆盖
//! `plaintext`、`json` 和 `clash` 三种格式；其余格式都该交给 `exec` 脚本，
//! 这让本模块（乃至整个核心）保持小巧。
//!
//! 注意：`exec` 会以 ProxyGate 进程的权限运行配置文件里的命令。这是一条
//! 有意留出的逃生通道——请把 `config.yaml` 当作可信输入。
//!
//! 分页来源（目录里声明了页数的内置来源）在这里表现为**多条订阅源**：
//! [`crate::config::Config::normalize`] 已经把 `{page}` 展开成具体页码，
//! 所以本模块不需要知道分页的存在，每一页都是一次普通的 HTTP 拉取，各自
//! 计数、各自失败。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as JsonValue;
use tokio::process::Command;
use url::Url;

use crate::config::{Config, Format, SubscriberConfig};
use crate::error::{Error, Result};
use crate::model::{self, ProxyScheme};

/// 一次订阅源拉取的结果。订阅源失败属于数据而非错误：
/// 一个来源坏掉不能拖停其他来源。
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    /// 订阅源名称。
    pub name: String,
    /// 订阅源类型（`http`、`file` 或 `exec`）。
    pub kind: &'static str,
    /// 解析响应体时使用的格式。
    pub format: Format,
    /// 成功归一化得到的代理。
    pub proxies: Vec<Url>,
    /// 无法转换为代理 URL 的行。
    pub rejected: Vec<String>,
    /// 解析器有意忽略的条目（例如不支持的 clash 类型）。
    pub skipped: usize,
    /// 因来源设有 `limit` 而被丢弃的可用代理。
    pub truncated: usize,
    /// 本次拉取的耗时。
    pub duration: Duration,
    /// 失败原因；成功时为 `None`。
    pub error: Option<String>,
}

impl FetchOutcome {
    /// 本次拉取是否成功。
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// 解析出的代理条数。
    pub fn count(&self) -> usize {
        self.proxies.len()
    }
}

/// 负责拉取所有已配置的订阅源。
pub struct SubscriberSet {
    subscribers: Vec<SubscriberConfig>,
    timeout: Duration,
    client: reqwest::Client,
}

impl SubscriberSet {
    /// 依据配置构建订阅源集合，并创建共享的 HTTP 客户端。
    pub fn new(config: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("proxygate/", env!("CARGO_PKG_VERSION")))
            .timeout(config.refresh.timeout)
            .build()?;
        Ok(Self {
            subscribers: config.subscribers.clone(),
            timeout: config.refresh.timeout,
            client,
        })
    }

    /// 只包含已启用的订阅源。
    pub fn active(&self) -> impl Iterator<Item = &SubscriberConfig> {
        self.subscribers.iter().filter(|s| s.enabled())
    }

    /// 没有任何已启用的订阅源时返回 `true`。
    pub fn is_empty(&self) -> bool {
        self.active().next().is_none()
    }

    /// 配置中订阅源的总数，含未启用的。
    pub fn configured(&self) -> usize {
        self.subscribers.len()
    }

    /// 已启用的订阅源数量。
    pub fn enabled(&self) -> usize {
        self.active().count()
    }

    /// 已启用订阅源的名称。
    pub fn names(&self) -> Vec<&str> {
        self.active().map(|s| s.name()).collect()
    }

    /// 并发拉取所有订阅源。
    pub async fn fetch_all(&self) -> Vec<FetchOutcome> {
        let futures = self
            .active()
            .map(|subscriber| self.fetch_one(subscriber))
            .collect::<Vec<_>>();
        futures_util::future::join_all(futures).await
    }

    /// 拉取单个订阅源，并把任何失败都转换为 `FetchOutcome::error`。
    pub async fn fetch_one(&self, subscriber: &SubscriberConfig) -> FetchOutcome {
        let started = Instant::now();
        let mut outcome = FetchOutcome {
            name: subscriber.name().to_string(),
            kind: subscriber.kind(),
            format: subscriber.format(),
            proxies: Vec::new(),
            rejected: Vec::new(),
            skipped: 0,
            truncated: 0,
            duration: Duration::ZERO,
            error: None,
        };

        let payload = match self.read_payload(subscriber).await {
            Ok(payload) => payload,
            Err(error) => {
                // The outcome already carries the name, so unwrap the variant
                // that repeats it in its Display.
                outcome.error = Some(match error {
                    Error::Subscriber { message, .. } => message,
                    other => other.to_string(),
                });
                outcome.duration = started.elapsed();
                return outcome;
            }
        };

        match parse_payload(&payload, subscriber.format()) {
            Ok(parsed) => {
                outcome.skipped = parsed.skipped;
                for candidate in parsed.candidates {
                    match model::normalize(&candidate) {
                        Ok(url) => outcome.proxies.push(url),
                        Err(error) => outcome.rejected.push(error.to_string()),
                    }
                }
            }
            Err(error) => outcome.error = Some(error.to_string()),
        }

        // Keep the cap last, so it applies to usable proxies rather than to
        // whatever the source happened to list first.
        outcome.truncated = apply_limit(&mut outcome.proxies, self.limit_for(subscriber));

        outcome.duration = started.elapsed();
        outcome
    }

    /// 每个来源的生效条数上限：配置值优先，`0` 表示不限，
    /// 否则采用目录自带的上限。
    fn limit_for(&self, subscriber: &SubscriberConfig) -> Option<usize> {
        let SubscriberConfig::Builtin {
            provider, limit, ..
        } = subscriber
        else {
            return None;
        };
        match limit {
            Some(0) => None,
            Some(explicit) => Some(*explicit),
            None => crate::providers::find(provider).and_then(|entry| entry.limit),
        }
    }

    /// 按订阅源类型读取响应体。
    async fn read_payload(&self, subscriber: &SubscriberConfig) -> Result<String> {
        match subscriber {
            SubscriberConfig::Http {
                url,
                headers,
                timeout,
                ..
            } => self.fetch_http(subscriber, url, headers, *timeout).await,
            // A builtin is an HTTP subscriber whose endpoint and format come
            // from the catalog, so it takes the same path.
            SubscriberConfig::Builtin {
                provider,
                url,
                timeout,
                ..
            } => {
                let entry = crate::providers::find(provider).ok_or_else(|| Error::Subscriber {
                    name: subscriber.name().to_string(),
                    message: format!(
                        "unknown builtin provider `{provider}` (available: {})",
                        crate::providers::names().join(", ")
                    ),
                })?;
                let url = url.as_deref().unwrap_or(entry.url);
                // The catalog may know this endpoint needs longer than the
                // global `refresh.timeout`; an explicit config value wins.
                self.fetch_http(subscriber, url, &BTreeMap::new(), timeout.or(entry.timeout))
                    .await
            }
            SubscriberConfig::File { path, .. } => tokio::fs::read_to_string(path)
                .await
                .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display()))),
            SubscriberConfig::Exec {
                command,
                env,
                timeout,
                ..
            } => run_command(command, env, timeout.unwrap_or(self.timeout)).await,
        }
    }

    /// 发起一次 HTTP GET 请求并返回响应体文本。
    async fn fetch_http(
        &self,
        subscriber: &SubscriberConfig,
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<String> {
        let mut request = self
            .client
            .get(url)
            .timeout(timeout.unwrap_or(self.timeout));
        if !headers.is_empty() {
            let mut map = HeaderMap::new();
            for (name, value) in headers {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|e| Error::Subscriber {
                        name: subscriber.name().to_string(),
                        message: format!("invalid header name `{name}`: {e}"),
                    })?;
                let value = HeaderValue::from_str(value).map_err(|e| Error::Subscriber {
                    name: subscriber.name().to_string(),
                    message: format!("invalid header value for `{name}`: {e}"),
                })?;
                map.insert(name, value);
            }
            request = request.headers(map);
        }

        let response = request.send().await.map_err(|error| Error::Subscriber {
            name: subscriber.name().to_string(),
            message: crate::error::describe_reqwest_error(&error),
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Subscriber {
                name: subscriber.name().to_string(),
                message: format!("HTTP {status}"),
            });
        }
        response.text().await.map_err(|error| Error::Subscriber {
            name: subscriber.name().to_string(),
            message: format!(
                "cannot read the response body: {}",
                crate::error::describe_reqwest_error(&error)
            ),
        })
    }
}

/// 运行一个 `exec` 订阅源并返回其 stdout。
async fn run_command(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<String> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::Other("empty exec command".into()))?;

    let mut child = Command::new(program);
    child
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If the timeout fires the future is dropped; make sure the child dies.
        .kill_on_drop(true);

    let label = command.join(" ");
    let output = tokio::time::timeout(timeout, child.output())
        .await
        .map_err(|_| Error::Other(format!("`{label}` timed out after {timeout:?}")))?
        .map_err(|e| Error::Other(format!("cannot run `{label}`: {e}")))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        tracing::debug!(command = %label, "exec subscriber wrote to stderr: {}", stderr.trim());
    }

    if !output.status.success() {
        let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(Error::Other(format!(
            "`{label}` exited with {}{}",
            output.status,
            if tail.is_empty() {
                String::new()
            } else {
                format!(" (stderr: {tail})")
            }
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 每个来源的生效条数上限：配置值优先，`0` 表示不限，
/// 否则采用目录自带的上限。只有 `builtin` 条目才有上限。
pub fn effective_limit(subscriber: &SubscriberConfig) -> Option<usize> {
    let SubscriberConfig::Builtin {
        provider, limit, ..
    } = subscriber
    else {
        return None;
    };
    match limit {
        Some(0) => None,
        Some(explicit) => Some(*explicit),
        None => crate::providers::find(provider).and_then(|entry| entry.limit),
    }
}

/// 把代理列表截断到 `limit`，并返回被丢弃的条数。
pub fn apply_limit(proxies: &mut Vec<Url>, limit: Option<usize>) -> usize {
    match limit {
        Some(limit) if proxies.len() > limit => {
            let dropped = proxies.len() - limit;
            proxies.truncate(limit);
            dropped
        }
        _ => 0,
    }
}

/// 把一段响应体交给某个内置格式解析器处理得到的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedPayload {
    /// 候选代理字符串，尚未归一化。
    pub candidates: Vec<String>,
    /// 因协议不受支持而跳过的条目。
    pub skipped: usize,
}

/// 把订阅源响应体解析为候选代理字符串。
pub fn parse_payload(text: &str, format: Format) -> Result<ParsedPayload> {
    match format {
        Format::Plaintext => Ok(parse_plaintext(text)),
        Format::Json => {
            let value: JsonValue = serde_json::from_str(text)?;
            Ok(parse_json_value(&value))
        }
        Format::Clash => {
            // Clash files are YAML; convert once and reuse the JSON walker.
            let value: serde_yaml::Value = serde_yaml::from_str(text)?;
            let value: JsonValue = serde_json::to_value(value)
                .map_err(|e| Error::Other(format!("unsupported clash payload: {e}")))?;
            Ok(parse_json_value(&value))
        }
    }
}

/// 每行一个代理；空行与 `#` 注释会被忽略。
fn parse_plaintext(text: &str) -> ParsedPayload {
    let mut parsed = ParsedPayload::default();
    for line in text.lines() {
        if model::is_ignorable_line(line) {
            continue;
        }
        let candidate = model::strip_inline_comment(line).trim();
        if !candidate.is_empty() {
            parsed.candidates.push(candidate.to_string());
        }
    }
    parsed
}

/// 遍历一段 JSON/YAML 数据以寻找代理。
///
/// 可识别的形态：字符串或对象组成的数组、带 `proxies` 或 `data`
/// 数组的对象，或者单个代理对象。
fn parse_json_value(value: &JsonValue) -> ParsedPayload {
    let mut parsed = ParsedPayload::default();
    walk(value, &mut parsed, 0);
    parsed
}

/// 数据嵌套深度的上限，超过就计为跳过。
const MAX_DEPTH: usize = 6;

/// 其数组值存放代理的容器字段名。
const CONTAINER_KEYS: [&str; 5] = ["proxies", "data", "items", "list", "result"];

/// 存放代理端点（主机名或完整 URL）的主机字段名。
const HOST_KEYS: [&str; 6] = ["server", "host", "hostname", "ip", "address", "addr"];
/// 直接存放完整代理 URL 的字段名。
const URL_KEYS: [&str; 4] = ["url", "proxy", "uri", "address_url"];

/// 递归遍历一个 JSON 值，把识别到的代理追加到 `parsed`。
fn walk(value: &JsonValue, parsed: &mut ParsedPayload, depth: usize) {
    if depth > MAX_DEPTH {
        parsed.skipped += 1;
        return;
    }
    match value {
        JsonValue::Array(items) => {
            for item in items {
                walk(item, parsed, depth + 1);
            }
        }
        JsonValue::String(text) => {
            let cleaned = model::clean_line(text);
            if !cleaned.is_empty() {
                parsed.candidates.push(cleaned.to_string());
            }
        }
        JsonValue::Object(map) => {
            let host = lookup(map, &HOST_KEYS);
            let url = lookup(map, &URL_KEYS);

            // Recognised containers first, but only on something that is not
            // itself a proxy entry: `{"proxies":[...]}`, `{"data":[...]}`.
            if host.is_none() && url.is_none() {
                let mut nested = false;
                for key in CONTAINER_KEYS {
                    if let Some(inner) = map.get(key) {
                        if inner.is_array() {
                            walk(inner, parsed, depth + 1);
                            nested = true;
                        }
                    }
                }
                if nested {
                    return;
                }
            }

            // A full URL under a known key, or a proxy described by fields.
            if let Some(url) = url {
                parsed.candidates.push(url);
                return;
            }
            if let Some(candidate) = proxy_from_fields(map) {
                parsed.candidates.push(candidate);
                return;
            }

            // It has a host but named a protocol ProxyGate cannot tunnel
            // (socks4, vmess, ...). Count it and stop here: descending into its
            // fields would mine metadata — `"protocols": ["socks4"]` is a list of
            // protocol names, not a list of proxies.
            if host.is_some() {
                parsed.skipped += 1;
                return;
            }

            // Otherwise this is an envelope (`{"code":200,"data":{"proxies":[...]}}`)
            // or a metadata object: descend into whatever is nested inside. A
            // leaf object that describes nothing is counted, not silently lost.
            let mut descended = false;
            for inner in map.values() {
                if inner.is_array() || inner.is_object() {
                    walk(inner, parsed, depth + 1);
                    descended = true;
                }
            }
            if !descended {
                parsed.skipped += 1;
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => parsed.skipped += 1,
    }
}

/// 按顺序查找一组键，返回第一个命中且非空的字符串值。
fn lookup(map: &serde_json::Map<String, JsonValue>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(|value| value.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// 依据大多数 JSON API 和 Clash 使用的字段名拼出一个代理 URL。
///
/// 对 ProxyGate 无法使用的协议（Shadowsocks、VMess、Trojan……）返回
/// `None`，这样它们会被记为跳过而不是拒绝。
fn proxy_from_fields(map: &serde_json::Map<String, JsonValue>) -> Option<String> {
    let host = lookup(map, &HOST_KEYS)?;

    // Which protocol the entry claims. A list source may express it as a string
    // (`"protocol": "HTTP"`), as a list (`"protocols": ["https"]`), or as a
    // joined string (`"socks4+socks5"`); all three are common in the wild.
    let scheme = scheme_from_fields(map)?;

    let port = map
        .get("port")
        .and_then(|value| match value {
            JsonValue::Number(number) => number.as_u64().map(|port| port as u16),
            JsonValue::String(text) => text.trim().parse::<u16>().ok(),
            _ => None,
        })
        .unwrap_or_else(|| scheme.default_port());

    let username = lookup(map, &["username", "user"]);
    let password = lookup(map, &["password", "pass"]);
    let auth = match (&username, &password) {
        (Some(user), Some(pass)) => format!("{}:{}@", encode_userinfo(user), encode_userinfo(pass)),
        (Some(user), None) => format!("{}@", encode_userinfo(user)),
        _ => String::new(),
    };

    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };

    Some(format!("{}://{auth}{host}:{port}", scheme.as_str()))
}

/// 条目声明的协议，ProxyGate 无法使用时为 `None`。
///
/// 这些列表里的命名很随意：`https` 指的是“能够 CONNECT 到 HTTPS 的
/// HTTP 代理”，而不是“到代理的 TLS”；`socks5` 会被改写为
/// [`ProxyScheme::Socks5h`]，由*代理*去解析域名。这一点很关键：在 DNS
/// 被污染的网络里，客户端自己解析 `www.google.com` 会把伪造的地址交给
/// 代理，而由代理远端解析则能正常工作。
fn scheme_from_fields(map: &serde_json::Map<String, JsonValue>) -> Option<ProxyScheme> {
    let mut named: Vec<String> = Vec::new();
    for key in ["type", "scheme", "protocol", "protocols", "proxy_type"] {
        if let Some(value) = map.get(key) {
            collect_scheme_names(value, &mut named);
        }
    }

    // Nothing said: a bare `host:port` is an HTTP proxy by convention.
    if named.is_empty() {
        return Some(ProxyScheme::Http);
    }
    if named
        .iter()
        .any(|name| matches!(name.as_str(), "http" | "https" | "ssl"))
    {
        return Some(ProxyScheme::Http);
    }
    if named
        .iter()
        .any(|name| matches!(name.as_str(), "socks5" | "socks5h" | "socks"))
    {
        return Some(ProxyScheme::Socks5h);
    }
    None
}

/// 把协议字段（字符串、列表或拼接字符串）摊平成小写名称，
/// 因此 `["http", "socks5"]` 和 `"socks4+socks5"` 都能工作。
fn collect_scheme_names(value: &JsonValue, out: &mut Vec<String>) {
    match value {
        JsonValue::String(text) => {
            for name in text
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|name| !name.is_empty())
            {
                out.push(name.to_ascii_lowercase());
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                collect_scheme_names(item, out);
            }
        }
        _ => {}
    }
}

/// 对会破坏 URL userinfo 段的字符做百分号编码。
fn encode_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => {
                out.push('%');
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{other:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn parses_plaintext_lists() {
        let payload = "\u{feff}# comment\n\nhttp://1.2.3.4:8080\n1.2.3.4:8080\r\nsocks5://user:pass@5.6.7.8:1080  # fast\n";
        let parsed = parse_payload(payload, Format::Plaintext).unwrap();
        assert_eq!(
            parsed.candidates,
            vec![
                "http://1.2.3.4:8080".to_string(),
                "1.2.3.4:8080".to_string(),
                "socks5://user:pass@5.6.7.8:1080".to_string(),
            ]
        );
    }

    #[test]
    fn parses_json_arrays_of_strings() {
        let parsed =
            parse_payload(r#"["http://1.2.3.4:8080", "5.6.7.8:3128"]"#, Format::Json).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        assert!(model::normalize(&parsed.candidates[1]).is_ok());
    }

    #[test]
    fn parses_json_objects_into_urls() {
        let payload = r#"
        {
          "data": [
            {"ip": "1.2.3.4", "port": 8080, "username": "u", "password": "p"},
            {"host": "5.6.7.8", "port": "3128", "protocol": "socks5"},
            {"server": "9.9.9.9", "port": 1080, "type": "ss", "cipher": "aes-256-gcm"},
            {"url": "http://7.7.7.7:8000"}
          ]
        }
        "#;
        let parsed = parse_payload(payload, Format::Json).unwrap();
        assert_eq!(
            parsed.candidates,
            vec![
                "http://u:p@1.2.3.4:8080".to_string(),
                "socks5h://5.6.7.8:3128".to_string(),
                "http://7.7.7.7:8000".to_string(),
            ]
        );
        // The Shadowsocks entry is counted, not silently dropped.
        assert_eq!(parsed.skipped, 1);
    }

    #[test]
    fn parses_a_nested_data_envelope() {
        // The shape served by proxy.scdn.io (and by a lot of other panel APIs):
        // the proxies sit in an array inside an object inside an object.
        let payload = r#"{"code":200,"message":"success","data":{"proxies":["47.237.113.119:16044","8.138.147.110:8008"],"count":2}}"#;
        let parsed = parse_payload(payload, Format::Json).unwrap();
        assert_eq!(
            parsed.candidates,
            vec![
                "47.237.113.119:16044".to_string(),
                "8.138.147.110:8008".to_string(),
            ]
        );
        assert_eq!(parsed.skipped, 0, "an envelope is not a skipped proxy");
        for candidate in &parsed.candidates {
            assert!(crate::model::normalize(candidate).is_ok());
        }
    }

    #[test]
    fn descends_through_arbitrary_envelopes() {
        let payload = r#"{"ok":true,"payload":{"page":1,"result":{"list":[{"host":"1.2.3.4","port":8080}]}}}"#;
        let parsed = parse_payload(payload, Format::Json).unwrap();
        assert_eq!(parsed.candidates, vec!["http://1.2.3.4:8080".to_string()]);
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn reads_the_protocol_field_in_every_shape_lists_use() {
        // Real payloads name the protocol as a string, as a list, as a joined
        // string, and in mixed case.
        let payload = r#"[
            {"ip": "1.1.1.1", "port": 80, "protocol": "HTTP"},
            {"ip": "2.2.2.2", "port": 80, "protocol": "HTTPS"},
            {"ip": "3.3.3.3", "port": 1080, "protocol": "Socks5"},
            {"ip": "4.4.4.4", "port": 1080, "protocols": ["socks5"]},
            {"ip": "5.5.5.5", "port": 8080, "protocols": ["https"]},
            {"ip": "6.6.6.6", "port": 8080, "protocols": ["http", "socks5"]},
            {"ip": "7.7.7.7", "port": 1080, "protocol": "socks4+socks5"},
            {"ip": "8.8.8.8", "port": 1080, "protocol": "SOCKS4"},
            {"ip": "9.9.9.9", "port": 1080, "protocols": ["socks4"]},
            {"ip": "10.10.10.10", "port": 3128}
        ]"#;
        let parsed = parse_payload(payload, Format::Json).unwrap();
        let rendered: Vec<String> = parsed
            .candidates
            .iter()
            .map(|candidate| model::render_url(&model::normalize(candidate).unwrap(), true))
            .collect();

        assert_eq!(
            rendered,
            vec![
                "http://1.1.1.1:80".to_string(),
                // `https` in a list means "can CONNECT to HTTPS", not TLS-to-proxy.
                "http://2.2.2.2:80".to_string(),
                // socks5 becomes socks5h: the proxy resolves names, which is what
                // makes blocked destinations work from a poisoned-DNS network.
                "socks5h://3.3.3.3:1080".to_string(),
                "socks5h://4.4.4.4:1080".to_string(),
                "http://5.5.5.5:8080".to_string(),
                // An entry offering both: HTTP wins, it is the safer default.
                "http://6.6.6.6:8080".to_string(),
                "socks5h://7.7.7.7:1080".to_string(),
                // The last one said nothing, so it keeps the host:port convention.
                "http://10.10.10.10:3128".to_string(),
            ]
        );
        assert_eq!(parsed.skipped, 2, "socks4-only entries are not usable");
    }

    #[test]
    fn limits_are_resolved_from_config_then_catalog() {
        use crate::config::SubscriberConfig;

        let builtin = |limit| SubscriberConfig::Builtin {
            name: "x".into(),
            provider: "freeproxy-gh".into(),
            url: None,
            format: None,
            timeout: None,
            limit,
            enabled: true,
        };

        // No config value: the catalog's cap applies.
        assert_eq!(effective_limit(&builtin(None)), Some(1000));
        // A config value wins.
        assert_eq!(effective_limit(&builtin(Some(25))), Some(25));
        // `0` means "no cap".
        assert_eq!(effective_limit(&builtin(Some(0))), None);

        // A provider without a catalog cap stays uncapped.
        let uncapped = SubscriberConfig::Builtin {
            name: "x".into(),
            provider: "scdn".into(),
            url: None,
            format: None,
            timeout: None,
            limit: None,
            enabled: true,
        };
        assert_eq!(effective_limit(&uncapped), None);

        // Other kinds never have a cap.
        let file = SubscriberConfig::File {
            name: "f".into(),
            path: "x".into(),
            format: Format::Plaintext,
            enabled: true,
        };
        assert_eq!(effective_limit(&file), None);
    }

    #[test]
    fn the_limit_cuts_the_usable_end_of_the_list() {
        let mut proxies: Vec<url::Url> = (1..=10)
            .map(|i| model::normalize(&format!("10.0.0.{i}:8080")).unwrap())
            .collect();

        assert_eq!(apply_limit(&mut proxies, None), 0);
        assert_eq!(proxies.len(), 10);
        assert_eq!(apply_limit(&mut proxies, Some(50)), 0);
        assert_eq!(proxies.len(), 10);

        assert_eq!(apply_limit(&mut proxies, Some(4)), 6);
        assert_eq!(proxies.len(), 4);
        assert_eq!(
            proxies[0].host_str(),
            Some("10.0.0.1"),
            "keeps the first entries"
        );
    }

    #[test]
    fn parses_clash_proxies() {
        let payload = r#"
proxies:
  - name: "a"
    type: http
    server: 1.2.3.4
    port: 8080
    username: user
    password: "p@ss word"
  - name: "b"
    type: socks5
    server: 5.6.7.8
    port: 1080
  - name: "c"
    type: vmess
    server: 9.9.9.9
    port: 443
    uuid: whatever
"#;
        let parsed = parse_payload(payload, Format::Clash).unwrap();
        assert_eq!(parsed.candidates.len(), 2);
        assert_eq!(parsed.skipped, 1);

        let first = crate::model::Proxy::new(model::normalize(&parsed.candidates[0]).unwrap());
        assert_eq!(first.username().as_deref(), Some("user"));
        assert_eq!(first.password().as_deref(), Some("p@ss word"));
        assert_eq!(first.to_masked_string(), "http://***:***@1.2.3.4:8080");
        assert_eq!(
            first.to_full_string(),
            "http://user:p%40ss%20word@1.2.3.4:8080"
        );

        // A clash entry naming socks5 becomes socks5h: the proxy resolves names.
        let second = model::normalize(&parsed.candidates[1]).unwrap();
        assert_eq!(model::render_url(&second, true), "socks5h://5.6.7.8:1080");
    }

    #[test]
    fn reports_broken_payloads() {
        assert!(parse_payload("{not json", Format::Json).is_err());
        assert!(parse_payload("proxies: [", Format::Clash).is_err());
    }

    #[tokio::test]
    async fn reads_a_file_subscriber() {
        let dir = std::env::temp_dir().join(format!("proxygate-sub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxies.txt");
        std::fs::write(&path, "1.2.3.4:8080\n5.6.7.8:3128\nnot a proxy\n").unwrap();

        let config = Config {
            subscribers: vec![SubscriberConfig::File {
                name: "local".into(),
                path: path.clone(),
                format: Format::Plaintext,
                enabled: true,
            }],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        let outcomes = set.fetch_all().await;

        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0];
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.count(), 2);
        // `not a proxy` parses as a hostname without a port, which normalize rejects.
        assert_eq!(outcome.rejected.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_subscriber_uses_stdout() {
        let config = Config {
            subscribers: vec![SubscriberConfig::Exec {
                name: "custom".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo 1.2.3.4:8080; echo '# ignored'".into(),
                ],
                env: BTreeMap::new(),
                format: Format::Plaintext,
                timeout: None,
                enabled: true,
            }],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        let outcomes = set.fetch_all().await;
        assert!(outcomes[0].ok(), "{:?}", outcomes[0].error);
        assert_eq!(outcomes[0].count(), 1);
    }

    #[tokio::test]
    async fn exec_failures_are_reported_not_fatal() {
        let config = Config {
            subscribers: vec![
                SubscriberConfig::Exec {
                    name: "broken".into(),
                    command: vec!["/bin/sh".into(), "-c".into(), "exit 3".into()],
                    env: BTreeMap::new(),
                    format: Format::Plaintext,
                    timeout: None,
                    enabled: true,
                },
                SubscriberConfig::Exec {
                    name: "missing".into(),
                    command: vec!["/definitely/not/a/binary".into()],
                    env: BTreeMap::new(),
                    format: Format::Plaintext,
                    timeout: None,
                    enabled: true,
                },
            ],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        let outcomes = set.fetch_all().await;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|outcome| !outcome.ok()));
    }

    #[tokio::test]
    async fn disabled_subscribers_are_skipped() {
        let config = Config {
            subscribers: vec![SubscriberConfig::Exec {
                name: "off".into(),
                command: vec!["/bin/sh".into(), "-c".into(), "echo 1.2.3.4:8080".into()],
                env: BTreeMap::new(),
                format: Format::Plaintext,
                timeout: None,
                enabled: false,
            }],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        assert!(set.is_empty());
        assert!(set.fetch_all().await.is_empty());
    }
}
