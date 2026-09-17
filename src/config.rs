//! `config.yaml` model, defaults and loading rules.
//!
//! Every field is optional: a missing config file yields a working
//! configuration with no subscribers, and unknown keys are ignored so that a
//! newer config still loads on an older binary.

use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The annotated example config, embedded at compile time so `proxygate
/// genconfig` works from an installed binary that has no checkout next to it.
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.yaml");

/// Environment variable pointing at the config file.
pub const CONFIG_ENV: &str = "PROXYGATE_CONFIG";
/// Environment variable overriding the cache directory.
pub const CACHE_DIR_ENV: &str = "PROXYGATE_CACHE_DIR";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub subscribers: Vec<SubscriberConfig>,
    #[serde(default)]
    pub refresh: RefreshConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub selection: SelectionConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub state: StateConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address of the HTTP proxy gateway.
    #[serde(default = "default_proxy_addr")]
    pub proxy: String,
    /// Address of the REST API.
    #[serde(default = "default_api_addr")]
    pub api: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            proxy: default_proxy_addr(),
            api: default_api_addr(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshConfig {
    /// How long a subscriber result is considered fresh.
    #[serde(
        default = "default_refresh_interval",
        deserialize_with = "de::duration"
    )]
    pub interval: Duration,
    /// Per-subscriber timeout.
    #[serde(default = "default_refresh_timeout", deserialize_with = "de::duration")]
    pub timeout: Duration,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            interval: default_refresh_interval(),
            timeout: default_refresh_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
    /// Single probe target. Kept as an alias for older configs; use `targets`.
    #[serde(default)]
    pub target: Option<String>,
    /// URLs fetched *through each proxy* to prove it works. Probed
    /// concurrently, one entry per target.
    ///
    /// `None` (the key is absent) means "use the built-in pair"; an explicit
    /// empty list is a configuration error rather than a silent fallback.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// Whether every target must answer (`all`) or just one (`any`) for a proxy
    /// to count as alive.
    #[serde(default)]
    pub require: HealthRequirement,
    /// How long a health result stays fresh.
    #[serde(default = "default_health_interval", deserialize_with = "de::duration")]
    pub interval: Duration,
    /// Per-request timeout for one proxy.
    #[serde(default = "default_health_timeout", deserialize_with = "de::duration")]
    pub timeout: Duration,
    /// Maximum number of proxies checked concurrently.
    #[serde(default = "default_health_concurrency")]
    pub concurrency: usize,
    /// Consecutive failures after which a proxy counts as dead.
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            target: None,
            targets: None,
            require: HealthRequirement::default(),
            interval: default_health_interval(),
            timeout: default_health_timeout(),
            concurrency: default_health_concurrency(),
            max_failures: default_max_failures(),
        }
    }
}

impl HealthConfig {
    /// The effective probe targets, in order: `targets`, then the deprecated
    /// singular `target`, trimmed and deduplicated.
    ///
    /// When neither key is present the built-in pair is used, so a config that
    /// says nothing about health still checks something meaningful.
    pub fn targets(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |raw: &str| {
            let target = raw.trim();
            if !target.is_empty() && !out.iter().any(|seen| seen == target) {
                out.push(target.to_string());
            }
        };

        for raw in self.targets.iter().flatten() {
            push(raw);
        }
        if let Some(single) = &self.target {
            push(single);
        }
        if self.targets.is_none() && self.target.is_none() {
            return default_health_targets();
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelectionConfig {
    #[serde(default)]
    pub strategy: crate::selector::Strategy,
    /// Prefer proxies that were not handed out during this window.
    #[serde(default = "default_reuse_after", deserialize_with = "de::duration")]
    pub reuse_after: Duration,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            strategy: crate::selector::Strategy::default(),
            reuse_after: default_reuse_after(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// Extra upstream attempts after the first one fails.
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// Timeout for establishing the upstream connection/tunnel.
    #[serde(default = "default_connect_timeout", deserialize_with = "de::duration")]
    pub connect_timeout: Duration,
    /// Optional `user:password` required from gateway clients.
    #[serde(default)]
    pub auth: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            retries: default_retries(),
            connect_timeout: default_connect_timeout(),
            auth: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StateConfig {
    /// Overrides `~/.cache/proxygate` (also settable via `PROXYGATE_CACHE_DIR`).
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// One source of proxies.
///
/// The `type` tag selects the variant. `format` describes how to read the raw
/// payload and defaults to `plaintext` (one URL per line).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubscriberConfig {
    Http {
        #[serde(default)]
        name: String,
        url: String,
        #[serde(default)]
        format: Format,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default, deserialize_with = "de::opt_duration")]
        timeout: Option<Duration>,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    File {
        #[serde(default)]
        name: String,
        path: PathBuf,
        #[serde(default)]
        format: Format,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    /// Runs an external command and reads proxy URLs from its stdout.
    ///
    /// This is the escape hatch for any format ProxyGate does not understand:
    /// the script does the parsing, ProxyGate keeps its own code simple.
    Exec {
        #[serde(default)]
        name: String,
        command: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        format: Format,
        #[serde(default, deserialize_with = "de::opt_duration")]
        timeout: Option<Duration>,
        #[serde(default = "default_true")]
        enabled: bool,
    },
}

impl SubscriberConfig {
    pub fn name(&self) -> &str {
        match self {
            SubscriberConfig::Http { name, .. }
            | SubscriberConfig::File { name, .. }
            | SubscriberConfig::Exec { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            SubscriberConfig::Http { .. } => "http",
            SubscriberConfig::File { .. } => "file",
            SubscriberConfig::Exec { .. } => "exec",
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            SubscriberConfig::Http { enabled, .. }
            | SubscriberConfig::File { enabled, .. }
            | SubscriberConfig::Exec { enabled, .. } => *enabled,
        }
    }

    pub fn format(&self) -> Format {
        match self {
            SubscriberConfig::Http { format, .. }
            | SubscriberConfig::File { format, .. }
            | SubscriberConfig::Exec { format, .. } => *format,
        }
    }

    fn set_name(&mut self, name: String) {
        match self {
            SubscriberConfig::Http { name: n, .. }
            | SubscriberConfig::File { name: n, .. }
            | SubscriberConfig::Exec { name: n, .. } => *n = name,
        }
    }
}

/// How a subscriber payload is turned into proxy URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// One proxy per line; `#` starts a comment.
    #[default]
    Plaintext,
    /// JSON array (or object containing an array) of proxies.
    Json,
    /// Clash / Clash.Meta `proxies:` list.
    Clash,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Plaintext, Format::Json, Format::Clash];

    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Plaintext => "plaintext",
            Format::Json => "json",
            Format::Clash => "clash",
        }
    }
}

impl std::str::FromStr for Format {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "plaintext" | "plain" | "text" | "txt" | "list" => Ok(Format::Plaintext),
            "json" => Ok(Format::Json),
            "clash" | "yaml" | "yml" => Ok(Format::Clash),
            other => Err(Error::Config(format!(
                "unknown subscriber format `{other}` (expected one of plaintext, json, clash)"
            ))),
        }
    }
}

/// What "alive" means when several health targets are configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthRequirement {
    /// Every target must answer. A proxy that cannot reach one of them is dead
    /// even if it reaches the others. Strict: on a network where one of the
    /// targets is hard to reach, this can empty the pool entirely.
    All,
    /// At least one target must answer.
    ///
    /// The default: keeping a proxy that reaches *something* is more useful
    /// than handing out nothing, and the per-target results still show exactly
    /// what each proxy can and cannot reach (`proxygate list`, TARGETS column).
    #[default]
    Any,
}

impl HealthRequirement {
    pub const ALL: [HealthRequirement; 2] = [HealthRequirement::All, HealthRequirement::Any];

    pub const fn as_str(self) -> &'static str {
        match self {
            HealthRequirement::All => "all",
            HealthRequirement::Any => "any",
        }
    }

    /// True when this many passed targets satisfy the requirement.
    pub fn satisfied_by(self, passed: usize, total: usize) -> bool {
        match self {
            HealthRequirement::All => total > 0 && passed == total,
            HealthRequirement::Any => passed > 0,
        }
    }
}

impl std::str::FromStr for HealthRequirement {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "all" | "every" => Ok(HealthRequirement::All),
            "any" | "one" => Ok(HealthRequirement::Any),
            other => Err(Error::Config(format!(
                "unknown health.require `{other}` (expected all or any)"
            ))),
        }
    }
}

impl std::fmt::Display for HealthRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Config {
    /// Loads configuration: an explicit path wins, then `$PROXYGATE_CONFIG`,
    /// then `./config.yaml`, then `~/.config/proxygate/config.yaml`.
    ///
    /// Returns the config plus the path it came from, if any.
    pub fn load(explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let path = match explicit {
            Some(path) => Some(path.to_path_buf()),
            None => {
                let from_env = std::env::var_os(CONFIG_ENV).map(PathBuf::from);
                match from_env {
                    Some(path) => Some(path),
                    None => Self::default_paths().into_iter().find(|p| p.is_file()),
                }
            }
        };

        let Some(path) = path else {
            let mut config = Self::default();
            config.normalize()?;
            return Ok((config, None));
        };

        let raw = std::fs::read_to_string(&path).map_err(|e| {
            Error::Config(format!("cannot read config file {}: {e}", path.display()))
        })?;

        let mut config: Config = serde_yaml::from_str(&raw).map_err(|e| {
            Error::Config(format!("cannot parse config file {}: {e}", path.display()))
        })?;
        config.normalize()?;
        Ok((config, Some(path)))
    }

    /// Candidate config locations, in priority order.
    pub fn default_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("config.yaml")];
        if let Some(home) = home_dir() {
            paths.push(home.join(".config").join("proxygate").join("config.yaml"));
        }
        paths
    }

    /// Fills in derived values and rejects configurations that cannot work.
    pub fn normalize(&mut self) -> Result<()> {
        for (index, subscriber) in self.subscribers.iter_mut().enumerate() {
            if subscriber.name().trim().is_empty() {
                subscriber.set_name(format!("{}-{}", subscriber.kind(), index + 1));
            }
        }

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for subscriber in &self.subscribers {
            *seen.entry(subscriber.name()).or_default() += 1;
        }
        if let Some((name, _)) = seen.iter().find(|(_, count)| **count > 1) {
            return Err(Error::Config(format!(
                "duplicate subscriber name `{name}`; names must be unique"
            )));
        }

        self.validate()?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        for (label, address) in [
            ("server.proxy", &self.server.proxy),
            ("server.api", &self.server.api),
        ] {
            if address.trim().is_empty() {
                return Err(Error::Config(format!("{label} must not be empty")));
            }
            if address.to_socket_addrs().is_err() {
                return Err(Error::Config(format!(
                    "{label} `{address}` is not a valid `host:port` address"
                )));
            }
        }

        if self.health.concurrency == 0 {
            return Err(Error::Config(
                "health.concurrency must be at least 1".into(),
            ));
        }
        let targets = self.health.targets();
        if targets.is_empty() {
            return Err(Error::Config(
                "health.targets must list at least one URL".into(),
            ));
        }
        for target in &targets {
            match url::Url::parse(target) {
                Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                Ok(parsed) => {
                    return Err(Error::Config(format!(
                        "health target `{target}` must be http or https, not `{}`",
                        parsed.scheme()
                    )));
                }
                Err(error) => {
                    return Err(Error::Config(format!(
                        "health target `{target}` is not a valid URL: {error}"
                    )));
                }
            }
        }
        if self.health.timeout.is_zero() {
            return Err(Error::Config(
                "health.timeout must be greater than zero".into(),
            ));
        }
        if self.refresh.interval.is_zero() {
            return Err(Error::Config(
                "refresh.interval must be greater than zero".into(),
            ));
        }
        if self.refresh.timeout.is_zero() {
            return Err(Error::Config(
                "refresh.timeout must be greater than zero".into(),
            ));
        }
        if self.gateway.connect_timeout.is_zero() {
            return Err(Error::Config(
                "gateway.connect_timeout must be greater than zero".into(),
            ));
        }
        if self.gateway.retries > 10 {
            return Err(Error::Config(
                "gateway.retries must be 10 or less (each retry costs a client timeout)".into(),
            ));
        }
        if let Some(auth) = &self.gateway.auth {
            parse_basic_auth(auth)?;
        }

        for subscriber in &self.subscribers {
            match subscriber {
                SubscriberConfig::Http { url, .. } => {
                    let parsed = url::Url::parse(url).map_err(|e| {
                        Error::Config(format!(
                            "subscriber `{}` has an invalid url `{url}`: {e}",
                            subscriber.name()
                        ))
                    })?;
                    if !matches!(parsed.scheme(), "http" | "https") {
                        return Err(Error::Config(format!(
                            "subscriber `{}` url must be http or https",
                            subscriber.name()
                        )));
                    }
                }
                SubscriberConfig::File { path, .. } => {
                    if path.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "subscriber `{}` needs a path",
                            subscriber.name()
                        )));
                    }
                }
                SubscriberConfig::Exec { command, .. } => {
                    if command.is_empty() || command[0].trim().is_empty() {
                        return Err(Error::Config(format!(
                            "subscriber `{}` needs a non-empty command",
                            subscriber.name()
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    /// Cache directory: `state.dir`, then `$PROXYGATE_CACHE_DIR`, then
    /// `~/.cache/proxygate`.
    pub fn cache_dir(&self) -> PathBuf {
        if let Some(dir) = &self.state.dir {
            return dir.clone();
        }
        if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        home_dir()
            .map(|home| home.join(".cache").join("proxygate"))
            .unwrap_or_else(|| PathBuf::from(".proxygate"))
    }

    /// Resolved gateway credentials, if any.
    pub fn gateway_credentials(&self) -> Result<Option<(String, String)>> {
        self.gateway
            .auth
            .as_deref()
            .map(parse_basic_auth)
            .transpose()
    }
}

/// Splits `user:password` (the password may be empty and may contain colons).
pub fn parse_basic_auth(value: &str) -> Result<(String, String)> {
    let (user, password) = value.split_once(':').ok_or_else(|| {
        Error::Config(format!(
            "invalid credentials `{value}`: expected `user:password`"
        ))
    })?;
    if user.is_empty() {
        return Err(Error::Config(
            "invalid credentials: the user name must not be empty".into(),
        ));
    }
    Ok((user.to_string(), password.to_string()))
}

/// `~` expansion helper used for config and cache paths.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Parses `30s`, `10m`, `2h`, `1d`, `250ms`, `1h30m` or a plain number of
/// seconds.
pub fn parse_duration(input: &str) -> std::result::Result<Duration, String> {
    let value = input.trim();
    if value.is_empty() {
        return Err("empty duration".to_string());
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Ok(Duration::from_secs(seconds));
    }

    let chars: Vec<char> = value.chars().collect();
    let mut total = Duration::ZERO;
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(format!("invalid duration `{input}`"));
        }
        let number: u64 = chars[start..index]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| format!("invalid duration `{input}`"))?;
        if index >= chars.len() {
            return Err(format!("invalid duration `{input}`: missing unit"));
        }
        let unit = chars[index];
        index += 1;
        let part = match unit {
            's' => Duration::from_secs(number),
            'm' if chars.get(index) == Some(&'s') => {
                index += 1;
                Duration::from_millis(number)
            }
            'm' => Duration::from_secs(number * 60),
            'h' => Duration::from_secs(number * 60 * 60),
            'd' => Duration::from_secs(number * 60 * 60 * 24),
            'w' => Duration::from_secs(number * 60 * 60 * 24 * 7),
            other => return Err(format!("invalid duration unit `{other}` in `{input}`")),
        };
        total += part;
    }
    Ok(total)
}

/// Compact human readable duration (`30s`, `10m`, `1h30m`).
pub fn humanize_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if duration.subsec_millis() > 0 && seconds == 0 {
        return format!("{}ms", duration.as_millis());
    }
    if seconds == 0 {
        return "0s".to_string();
    }
    let mut remaining = seconds;
    let mut out = String::new();
    for (unit, size) in [("d", 86_400u64), ("h", 3_600), ("m", 60), ("s", 1)] {
        let count = remaining / size;
        if count > 0 {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{count}{unit}"));
            remaining -= count * size;
        }
    }
    out
}

mod de {
    use super::{Duration, parse_duration};
    use serde::de::{self, Visitor};
    use serde::{Deserialize, Deserializer};
    use std::fmt;

    struct DurationVisitor;

    impl Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a duration such as `30s`, `10m`, `1h30m` or a number of seconds")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Duration, E> {
            parse_duration(value).map_err(E::custom)
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Duration, E> {
            Ok(Duration::from_secs(value))
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Duration, E> {
            u64::try_from(value)
                .map(Duration::from_secs)
                .map_err(|_| E::custom("duration must not be negative"))
        }

        fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Duration, E> {
            if value < 0.0 {
                return Err(E::custom("duration must not be negative"));
            }
            Ok(Duration::from_secs_f64(value))
        }
    }

    pub fn duration<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Duration, D::Error> {
        deserializer.deserialize_any(DurationVisitor)
    }

    pub fn opt_duration<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Duration>, D::Error> {
        Ok(Option::<DurationSeed>::deserialize(deserializer)?.map(|seed| seed.0))
    }

    /// Newtype so that `Option`-wrapped durations reuse the visitor above.
    struct DurationSeed(Duration);

    impl<'de> Deserialize<'de> for DurationSeed {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            deserializer
                .deserialize_any(DurationVisitor)
                .map(DurationSeed)
        }
    }
}

fn default_proxy_addr() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_api_addr() -> String {
    "127.0.0.1:8081".to_string()
}

fn default_refresh_interval() -> Duration {
    Duration::from_secs(600)
}

fn default_refresh_timeout() -> Duration {
    Duration::from_secs(20)
}

/// Two targets on purpose: one that only works if the proxy has real
/// international connectivity, and one domestic endpoint to prove the tunnel is
/// not simply broken for everything else.
///
/// The probe runs *through the proxy*, so an unreachable-here target is fine —
/// it is the proxy that has to get there. By default `require: any` accepts a
/// proxy that reaches either one, and the per-target results record which.
fn default_health_targets() -> Vec<String> {
    vec![
        "https://www.google.com/generate_204".to_string(),
        "https://cn.bing.com/".to_string(),
    ]
}

fn default_health_interval() -> Duration {
    Duration::from_secs(30)
}

fn default_health_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_health_concurrency() -> usize {
    100
}

fn default_max_failures() -> u32 {
    3
}

fn default_reuse_after() -> Duration {
    Duration::from_secs(30 * 60)
}

fn default_retries() -> u32 {
    2
}

fn default_connect_timeout() -> Duration {
    Duration::from_secs(10)
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("10").is_ok());
    }

    #[test]
    fn humanizes_durations() {
        assert_eq!(humanize_duration(Duration::from_secs(30)), "30s");
        assert_eq!(humanize_duration(Duration::from_secs(600)), "10m");
        assert_eq!(humanize_duration(Duration::from_secs(5400)), "1h30m");
        assert_eq!(humanize_duration(Duration::ZERO), "0s");
        assert_eq!(humanize_duration(Duration::from_millis(250)), "250ms");
    }

    #[test]
    fn defaults_probe_google_and_a_domestic_target() {
        let config = Config::default();
        assert_eq!(
            config.health.targets(),
            vec![
                "https://www.google.com/generate_204".to_string(),
                "https://cn.bing.com/".to_string(),
            ]
        );
        assert_eq!(config.health.require, HealthRequirement::Any);
    }

    #[test]
    fn health_targets_accept_a_list_and_a_requirement() {
        let mut config: Config = serde_yaml::from_str(
            r#"
health:
  targets:
    - https://www.google.com/generate_204
    - https://cn.bing.com/
  require: any
"#,
        )
        .unwrap();
        config.normalize().unwrap();

        assert_eq!(config.health.targets().len(), 2);
        assert_eq!(config.health.require, HealthRequirement::Any);
        assert!(config.health.require.satisfied_by(1, 2));
        assert!(!HealthRequirement::All.satisfied_by(1, 2));
        assert!(HealthRequirement::All.satisfied_by(2, 2));
        assert!(!HealthRequirement::All.satisfied_by(0, 0));
    }

    #[test]
    fn the_singular_target_still_works_and_is_merged() {
        let mut config: Config =
            serde_yaml::from_str("health:\n  target: https://cn.bing.com/\n").unwrap();
        config.normalize().unwrap();
        assert_eq!(config.health.targets(), vec!["https://cn.bing.com/"]);

        // Both keys at once: union, order preserved, no duplicates.
        let mut config: Config = serde_yaml::from_str(
            r#"
health:
  target: https://cn.bing.com/
  targets:
    - https://www.google.com/generate_204
    - https://cn.bing.com/
"#,
        )
        .unwrap();
        config.normalize().unwrap();
        assert_eq!(
            config.health.targets(),
            vec![
                "https://www.google.com/generate_204".to_string(),
                "https://cn.bing.com/".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_bad_health_targets() {
        let config: Config = serde_yaml::from_str("health:\n  targets: []\n").unwrap();
        assert!(
            config.clone().normalize().is_err(),
            "an empty list is useless"
        );

        let mut config: Config =
            serde_yaml::from_str("health:\n  targets: [\"ftp://example.com/\"]\n").unwrap();
        assert!(
            config.normalize().is_err(),
            "only http(s) targets make sense"
        );
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        // `config.example.yaml` is embedded and printed by `proxygate genconfig`,
        // so it has to parse and validate at all times.
        let mut config: Config =
            serde_yaml::from_str(EXAMPLE_CONFIG).expect("config.example.yaml must parse");
        config
            .normalize()
            .expect("config.example.yaml must validate");

        assert!(
            !config.subscribers.is_empty(),
            "the example shows how to subscribe"
        );
        assert_eq!(
            config.health.targets().len(),
            2,
            "the example shows both probes"
        );
        assert!(EXAMPLE_CONFIG.contains("genconfig") || EXAMPLE_CONFIG.contains("subscribers"));

        // The generated config must not ask clients for credentials: it is meant
        // to be dropped in and used on the loopback address, and gateway auth
        // belongs on the command line (`--auth`) rather than in a checked-in
        // file. Re-adding an `auth:` line here makes this test fail.
        assert!(
            config.gateway.auth.is_none(),
            "config.example.yaml must not configure gateway.auth"
        );
        for line in EXAMPLE_CONFIG.lines() {
            assert!(
                !line.trim_start().starts_with("auth:"),
                "config.example.yaml must not carry an auth directive: {line}"
            );
        }
    }

    #[test]
    fn defaults_are_usable() {
        let config = Config::default();
        assert_eq!(config.server.proxy, "127.0.0.1:8080");
        assert_eq!(config.server.api, "127.0.0.1:8081");
        assert_eq!(config.selection.reuse_after, Duration::from_secs(1800));
        assert!(config.subscribers.is_empty());
    }

    #[test]
    fn parses_the_documented_example() {
        let raw = r#"
server:
  proxy: 127.0.0.1:8080
  api: 127.0.0.1:8081

subscribers:
  - name: provider
    type: http
    url: https://example.com/proxies.txt
  - name: local
    type: file
    path: ./proxies.txt
    format: clash
  - name: custom
    type: exec
    command: [python, ./subscribers/custom.py]

refresh:
  interval: 10m

health:
  target: https://cp.cloudflare.com/generate_204
  interval: 30s
  timeout: 5s
  concurrency: 100

selection:
  strategy: latency
  reuse_after: 30m

gateway:
  retries: 2
  auth: admin:secret
"#;
        let mut config: Config = serde_yaml::from_str(raw).unwrap();
        config.normalize().unwrap();

        assert_eq!(config.subscribers.len(), 3);
        assert_eq!(config.subscribers[0].format(), Format::Plaintext);
        assert_eq!(config.subscribers[1].format(), Format::Clash);
        assert_eq!(config.refresh.interval, Duration::from_secs(600));
        assert_eq!(config.health.concurrency, 100);
        assert_eq!(
            config.selection.strategy,
            crate::selector::Strategy::Latency
        );
        assert_eq!(
            config.gateway_credentials().unwrap(),
            Some(("admin".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn rejects_duplicate_and_invalid_entries() {
        let mut config = Config {
            subscribers: vec![
                SubscriberConfig::File {
                    name: "same".into(),
                    path: "a.txt".into(),
                    format: Format::Plaintext,
                    enabled: true,
                },
                SubscriberConfig::File {
                    name: "same".into(),
                    path: "b.txt".into(),
                    format: Format::Plaintext,
                    enabled: true,
                },
            ],
            ..Config::default()
        };
        assert!(config.normalize().is_err());

        let mut config = Config {
            health: HealthConfig {
                concurrency: 0,
                ..HealthConfig::default()
            },
            ..Config::default()
        };
        assert!(config.clone().normalize().is_err());

        config.health.concurrency = 4;
        assert!(config.normalize().is_ok());
    }

    #[test]
    fn names_unnamed_subscribers() {
        let mut config: Config =
            serde_yaml::from_str("subscribers:\n  - type: file\n    path: ./a.txt\n").unwrap();
        config.normalize().unwrap();
        assert_eq!(config.subscribers[0].name(), "file-1");
    }
}
