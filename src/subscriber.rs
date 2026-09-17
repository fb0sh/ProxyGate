//! Subscribers: where proxies come from.
//!
//! Three kinds only:
//!
//! * `http` — fetch a URL (`proxies.txt`, an API, a subscription);
//! * `file` — read a local file;
//! * `exec` — run a command and read its stdout.
//!
//! Whatever the payload looks like, a subscriber's only job is to produce proxy
//! URLs. Built-in parsers cover `plaintext`, `json` and `clash`; everything else
//! belongs in an `exec` script, which keeps this module (and the whole core)
//! small.
//!
//! Note that `exec` runs a command from the config file with the privileges of
//! the ProxyGate process. It is an intentional escape hatch — treat
//! `config.yaml` as trusted input.

use std::process::Stdio;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as JsonValue;
use tokio::process::Command;
use url::Url;

use crate::config::{Config, Format, SubscriberConfig};
use crate::error::{Error, Result};
use crate::model::{self, ProxyScheme};

/// Outcome of one subscriber fetch. Subscriber failures are data, not errors:
/// one broken provider must not stop the others.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub name: String,
    pub kind: &'static str,
    pub format: Format,
    pub proxies: Vec<Url>,
    /// Lines that could not be turned into a proxy URL.
    pub rejected: Vec<String>,
    /// Entries the parser deliberately ignored (e.g. unsupported clash types).
    pub skipped: usize,
    pub duration: Duration,
    pub error: Option<String>,
}

impl FetchOutcome {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    pub fn count(&self) -> usize {
        self.proxies.len()
    }
}

/// Fetches every configured subscriber.
pub struct SubscriberSet {
    subscribers: Vec<SubscriberConfig>,
    timeout: Duration,
    client: reqwest::Client,
}

impl SubscriberSet {
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

    /// Only the subscribers that are enabled.
    pub fn active(&self) -> impl Iterator<Item = &SubscriberConfig> {
        self.subscribers.iter().filter(|s| s.enabled())
    }

    pub fn is_empty(&self) -> bool {
        self.active().next().is_none()
    }

    pub fn configured(&self) -> usize {
        self.subscribers.len()
    }

    pub fn enabled(&self) -> usize {
        self.active().count()
    }

    pub fn names(&self) -> Vec<&str> {
        self.active().map(|s| s.name()).collect()
    }

    /// Fetches all subscribers concurrently.
    pub async fn fetch_all(&self) -> Vec<FetchOutcome> {
        let futures = self
            .active()
            .map(|subscriber| self.fetch_one(subscriber))
            .collect::<Vec<_>>();
        futures_util::future::join_all(futures).await
    }

    /// Fetches one subscriber, converting every failure into `FetchOutcome::error`.
    pub async fn fetch_one(&self, subscriber: &SubscriberConfig) -> FetchOutcome {
        let started = Instant::now();
        let mut outcome = FetchOutcome {
            name: subscriber.name().to_string(),
            kind: subscriber.kind(),
            format: subscriber.format(),
            proxies: Vec::new(),
            rejected: Vec::new(),
            skipped: 0,
            duration: Duration::ZERO,
            error: None,
        };

        let payload = match self.read_payload(subscriber).await {
            Ok(payload) => payload,
            Err(error) => {
                outcome.error = Some(error.to_string());
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

        outcome.duration = started.elapsed();
        outcome
    }

    async fn read_payload(&self, subscriber: &SubscriberConfig) -> Result<String> {
        match subscriber {
            SubscriberConfig::Http {
                url,
                headers,
                timeout,
                ..
            } => {
                let mut request = self
                    .client
                    .get(url)
                    .timeout(timeout.unwrap_or(self.timeout));
                if !headers.is_empty() {
                    let mut map = HeaderMap::new();
                    for (name, value) in headers {
                        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                            Error::Subscriber {
                                name: subscriber.name().to_string(),
                                message: format!("invalid header name `{name}`: {e}"),
                            }
                        })?;
                        let value =
                            HeaderValue::from_str(value).map_err(|e| Error::Subscriber {
                                name: subscriber.name().to_string(),
                                message: format!("invalid header value for `{name}`: {e}"),
                            })?;
                        map.insert(name, value);
                    }
                    request = request.headers(map);
                }

                let response = request.send().await?;
                let status = response.status();
                if !status.is_success() {
                    return Err(Error::Other(format!("HTTP {status}")));
                }
                Ok(response.text().await?)
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
}

/// Runs an `exec` subscriber and returns its stdout.
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

/// Result of running a payload through one of the built-in format parsers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedPayload {
    /// Candidate proxy strings, not yet normalized.
    pub candidates: Vec<String>,
    /// Entries skipped because their protocol is not supported.
    pub skipped: usize,
}

/// Parses a subscriber payload into candidate proxy strings.
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

/// One proxy per line; blank lines and `#` comments are ignored.
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

/// Walks a JSON/YAML blob looking for proxies.
///
/// Recognised shapes: an array of strings/objects, an object with a `proxies`
/// or `data` array, or a single proxy object.
fn parse_json_value(value: &JsonValue) -> ParsedPayload {
    let mut parsed = ParsedPayload::default();
    walk(value, &mut parsed, 0);
    parsed
}

const MAX_DEPTH: usize = 6;

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
            // Recognised containers first: `{"proxies":[...]}`, `{"data":[...]}`.
            let mut nested = false;
            for key in ["proxies", "data", "items", "list", "result"] {
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

            // A full URL under a known key, or a proxy described by fields.
            let explicit = lookup(map, &["url", "proxy", "uri", "address_url"]);
            if let Some(url) = explicit {
                parsed.candidates.push(url);
                return;
            }
            if let Some(candidate) = proxy_from_fields(map) {
                parsed.candidates.push(candidate);
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

fn lookup(map: &serde_json::Map<String, JsonValue>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(|value| value.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// Builds a proxy URL from the field names used by most JSON APIs and by Clash.
///
/// Returns `None` for entries whose protocol ProxyGate cannot use (Shadowsocks,
/// VMess, Trojan, ...) so that they are reported as skipped rather than rejected.
fn proxy_from_fields(map: &serde_json::Map<String, JsonValue>) -> Option<String> {
    let host = lookup(
        map,
        &["server", "host", "hostname", "ip", "address", "addr"],
    )?;
    let scheme =
        lookup(map, &["type", "scheme", "protocol", "proxy_type"]).unwrap_or_else(|| "http".into());
    let scheme = ProxyScheme::parse(&scheme)?;

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

/// Percent-encodes the characters that would break a URL's userinfo section.
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
                "socks5://5.6.7.8:3128".to_string(),
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

        let second = model::normalize(&parsed.candidates[1]).unwrap();
        assert_eq!(model::render_url(&second, true), "socks5://5.6.7.8:1080");
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
