//! `config.yaml` 的模型、默认值与加载规则。
//!
//! 每个字段都是可选的：缺少配置文件时，会得到一份没有订阅源但可用的配置；
//! 未知键会被忽略，因此较新的配置仍能在较旧的二进制上加载。

use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// 带注释的示例配置，在编译期嵌入，因此已安装的二进制即使旁边没有
/// 代码检出目录，`proxygate --example-config` 也能打印出来。
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.yaml");

/// 指向配置文件的环境变量。
pub const CONFIG_ENV: &str = "PROXYGATE_CONFIG";
/// 覆盖缓存目录的环境变量。
pub const CACHE_DIR_ENV: &str = "PROXYGATE_CACHE_DIR";

/// ProxyGate 的完整配置，对应 `config.yaml` 的顶层。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    /// `server` 段，配置 HTTP 代理网关与 REST API 的监听地址。
    #[serde(default)]
    pub server: ServerConfig,
    /// `subscribers` 段，列出全部订阅源（lua、http、file、exec）。
    #[serde(default)]
    pub subscribers: Vec<SubscriberConfig>,
    /// `refresh` 段，控制订阅源拉取的间隔与超时。
    #[serde(default)]
    pub refresh: RefreshConfig,
    /// `health` 段，控制健康检查的探测目标、超时与并发。
    #[serde(default)]
    pub health: HealthConfig,
    /// `selection` 段，控制从代理池中挑选代理的策略。
    #[serde(default)]
    pub selection: SelectionConfig,
    /// `gateway` 段，控制网关的重试、连接超时与客户端认证。
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// `state` 段，控制状态与缓存的存放目录。
    #[serde(default)]
    pub state: StateConfig,
}

/// `server` 段的配置：网关与 REST API 的监听地址。
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// HTTP 代理网关的监听地址，YAML 键 `server.proxy`，默认 `127.0.0.1:8080`。
    #[serde(default = "default_proxy_addr")]
    pub proxy: String,
    /// REST API 的监听地址，YAML 键 `server.api`，默认 `127.0.0.1:8081`。
    ///
    /// 也可以填写字面值 `same`（或 `proxy`，或与 `server.proxy` 相同的地址），
    /// 让 REST API 与代理共用同一个端口。注意 `gateway.auth` 只保护代理请求，
    /// 因此 API 与该端口共用时仍然对外开放。
    #[serde(default = "default_api_addr")]
    pub api: String,
}

impl ServerConfig {
    /// 实际用于绑定的 API 地址：字面值 `same`（或 `proxy`）表示与代理共用端口。
    ///
    /// 共用一个端口之所以可行，是因为两类请求可以区分：代理请求是 CONNECT 或
    /// 绝对形式的请求目标，API 调用则是 `/api/v1/...`，参见
    /// [`crate::gateway::Gateway::with_api`]。
    pub fn api_address(&self) -> &str {
        match self.api.trim() {
            "same" | "proxy" => self.proxy.trim(),
            other => other,
        }
    }

    /// API 与代理是否服务于同一端口。
    pub fn shares_port(&self) -> bool {
        self.api.trim() == "same"
            || self.api.trim() == "proxy"
            || self.api.trim() == self.proxy.trim()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            proxy: default_proxy_addr(),
            api: default_api_addr(),
        }
    }
}

/// `refresh` 段的配置：订阅源拉取的间隔与超时。
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshConfig {
    /// 订阅源结果被视为新鲜的时间长度，YAML 键 `refresh.interval`，默认 10 分钟。
    #[serde(
        default = "default_refresh_interval",
        deserialize_with = "de::duration"
    )]
    pub interval: Duration,
    /// 针对单个订阅源的超时，YAML 键 `refresh.timeout`，默认 20 秒。
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

/// `health` 段的配置：健康检查的目标、判定要求与并发。
#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
    /// 单个探测目标。作为旧版配置的别名保留；请改用 `targets`。
    #[serde(default)]
    pub target: Option<String>,
    /// 通过每个代理实际抓取的 URL，用来证明代理可用。并发探测，
    /// 每个目标一项。
    ///
    /// `None`（键缺失）表示“使用内置的一对目标”；显式给出空列表会被
    /// 视为配置错误，而不是静默回退。
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// 代理要被判定为存活，是需要每个目标都应答（`all`），还是只需
    /// 一个目标应答（`any`）。
    #[serde(default)]
    pub require: HealthRequirement,
    /// 健康检查结果保持新鲜的时间，YAML 键 `health.interval`，默认 30 秒。
    #[serde(default = "default_health_interval", deserialize_with = "de::duration")]
    pub interval: Duration,
    /// 针对单个代理的单次请求超时，YAML 键 `health.timeout`，默认 5 秒。
    #[serde(default = "default_health_timeout", deserialize_with = "de::duration")]
    pub timeout: Duration,
    /// 同时检查的代理数量上限，YAML 键 `health.concurrency`，默认 100。
    #[serde(default = "default_health_concurrency")]
    pub concurrency: usize,
    /// 连续失败多少次后代理被判定为死亡，YAML 键 `health.max_failures`，默认 3。
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
    /// 实际生效的探测目标，按顺序为 `targets`，随后是已废弃的单数形式
    /// `target`，并做去空白与去重。
    ///
    /// 两个键都不存在时使用内置的一对目标，因此对健康检查只字未提的
    /// 配置仍然会检查一些有意义的东西。
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

/// `selection` 段的配置：代理选择策略与重用窗口。
#[derive(Debug, Clone, Deserialize)]
pub struct SelectionConfig {
    /// 选择代理所用的策略，YAML 键 `selection.strategy`，默认 `random`。
    #[serde(default)]
    pub strategy: crate::selector::Strategy,
    /// 优先选择在该时间窗口内没有被分发过的代理，YAML 键
    /// `selection.reuse_after`，默认 30 分钟。
    #[serde(default = "default_reuse_after", deserialize_with = "de::duration")]
    pub reuse_after: Duration,
    /// 发放前是否验证选中的代理，YAML 键 `selection.verify`，默认 `true`。
    ///
    /// 池子里的判定可能已经很旧（大池子一轮探测要几分钟，只用 CLI 而
    /// 不跑 `serve` 时甚至可能是几小时前的），所以默认在交出去之前现探一次：
    /// 「手里这个现在能用」是任何时间间隔都给不了的保证。
    #[serde(default = "default_true")]
    pub verify: bool,
    /// 判定比这个时间新就直接用，不重新探，YAML 键
    /// `selection.max_age`，默认 60 秒。
    ///
    /// 这是发放验证的快路径：`refresh` / `check` 刚跑完时，前面这一段
    /// 时间的 `get` 依然是毫秒级。设成 `0s` 表示每次都探。
    #[serde(default = "default_verify_max_age", deserialize_with = "de::duration")]
    pub max_age: Duration,
    /// 现探单个候选时的超时，YAML 键 `selection.verify_timeout`，
    /// 默认 3 秒。
    ///
    /// 比 `health.timeout`（5 秒）短：发放路径上宁可快一点换下一个，也不要
    /// 让调用方等一个大概率没救的代理。
    #[serde(default = "default_verify_timeout", deserialize_with = "de::duration")]
    pub verify_timeout: Duration,
    /// 现探失败后最多再试几个候选，YAML 键 `selection.verify_attempts`，
    /// 默认 3。
    #[serde(default = "default_verify_attempts")]
    pub verify_attempts: usize,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            strategy: crate::selector::Strategy::default(),
            reuse_after: default_reuse_after(),
            verify: true,
            max_age: default_verify_max_age(),
            verify_timeout: default_verify_timeout(),
            verify_attempts: default_verify_attempts(),
        }
    }
}

/// `selection.max_age` 的默认值：判定比这新就直接用。
fn default_verify_max_age() -> Duration {
    Duration::from_secs(60)
}

/// `selection.verify_timeout` 的默认值。
fn default_verify_timeout() -> Duration {
    Duration::from_secs(3)
}

/// `selection.verify_attempts` 的默认值。
fn default_verify_attempts() -> usize {
    3
}

/// `gateway` 段的配置：重试、连接超时与客户端认证。
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// 第一次失败之后额外尝试的上游次数，YAML 键 `gateway.retries`，默认 2。
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// 建立上游连接或隧道的超时，YAML 键 `gateway.connect_timeout`，默认 10 秒。
    #[serde(default = "default_connect_timeout", deserialize_with = "de::duration")]
    pub connect_timeout: Duration,
    /// 可选，要求网关客户端提供的 `user:password`。YAML 键 `gateway.auth`，
    /// 默认不要求认证。
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

/// `state` 段的配置：状态与缓存的存放位置。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct StateConfig {
    /// 覆盖 `~/.cache/proxygate`（也可通过 `PROXYGATE_CACHE_DIR` 设置）。
    /// YAML 键 `state.dir`，默认未设置。
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// 一个代理来源。
///
/// `type` 标签决定使用哪个变体。`format` 描述如何读取原始响应内容，
/// 默认为 `plaintext`（每行一个 URL）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubscriberConfig {
    /// 从一个 HTTP(S) URL 拉取代理列表，请求可以携带自定义头。
    Http {
        /// 订阅源名称，缺省时自动命名（如 `http-1`）。
        #[serde(default)]
        name: String,
        /// 拉取代理列表的 URL。
        url: String,
        /// 响应内容的解析格式，默认 `plaintext`。
        #[serde(default)]
        format: Format,
        /// 随请求发送的 HTTP 头，默认没有。
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// 该订阅源的拉取超时，缺省时使用 `refresh.timeout`。
        #[serde(default, deserialize_with = "de::opt_duration")]
        timeout: Option<Duration>,
        /// 最多保留多少个可用代理（`0` 表示全部保留），默认不限。
        ///
        /// 健康检查必须探测拿到的每一条，所以在默认并发下大型列表要跑很久；
        /// 这个上限让池子的规模可控。
        #[serde(default)]
        limit: Option<usize>,
        /// 是否启用该订阅源，默认 `true`。
        #[serde(default = "default_true")]
        enabled: bool,
    },
    /// 从本地文件读取代理列表。
    File {
        /// 订阅源名称，缺省时自动命名（如 `file-1`）。
        #[serde(default)]
        name: String,
        /// 本地文件路径。
        path: PathBuf,
        /// 文件内容的解析格式，默认 `plaintext`。
        #[serde(default)]
        format: Format,
        /// 最多保留多少个可用代理（`0` 表示全部保留），默认不限。
        ///
        /// 健康检查必须探测拿到的每一条，所以在默认并发下大型列表要跑很久；
        /// 这个上限让池子的规模可控。
        #[serde(default)]
        limit: Option<usize>,
        /// 是否启用该订阅源，默认 `true`。
        #[serde(default = "default_true")]
        enabled: bool,
    },
    /// 一段 Lua 脚本（`type: lua`）：脚本自己决定去哪里、怎么拿、输出什么。
    ///
    /// ```yaml
    /// subscribers:
    ///   - name: my_scraper
    ///     type: lua
    ///     target_url: https://api.example.com/data.json   # 额外键 -> 脚本全局变量
    ///     lua_code: |
    ///       local data = fetch_json(target_url)
    ///       for _, item in ipairs(data.items) do
    ///         print(item.ip .. ":" .. item.port)
    ///       end
    /// ```
    ///
    /// 脚本能用的东西见 [`crate::subscriber`]：`fetch` / `fetch_json`、
    /// `json_encode` / `json_decode`、`log`，以及 `print`——**print 的每一行
    /// 就是一条候选代理**，再按 `format` 解析（默认 `plaintext`）。
    Lua {
        /// 订阅源名称，缺省时自动命名（如 `lua-1`）。`script_name` 也认。
        #[serde(default, alias = "script_name")]
        name: String,
        /// 内联的 Lua 代码。与 `lua_file` 二选一。
        #[serde(default)]
        lua_code: Option<String>,
        /// 一个 `.lua` 文件的路径。与 `lua_code` 二选一。
        #[serde(default)]
        lua_file: Option<PathBuf>,
        /// `print` 出来的内容按哪种格式解析，默认 `plaintext`。
        #[serde(default)]
        format: Format,
        /// 单次请求的超时；整段脚本也用它作为墙钟上限。
        #[serde(default, deserialize_with = "de::opt_duration")]
        timeout: Option<Duration>,
        /// 最多保留多少个可用代理（`0` 表示全部保留），默认不限。
        #[serde(default)]
        limit: Option<usize>,
        /// 是否启用该订阅源，默认 `true`。
        #[serde(default = "default_true")]
        enabled: bool,
        /// 其余所有键都会成为脚本里的全局变量（数字、布尔、字符串、表）。
        #[serde(flatten)]
        params: BTreeMap<String, serde_yaml::Value>,
    },
    /// 运行外部命令，并从其 stdout 读取代理 URL。
    ///
    /// 这是 ProxyGate 无法理解的任何格式的逃生通道：解析交给脚本完成，
    /// ProxyGate 自身的代码保持简单。
    Exec {
        /// 订阅源名称，缺省时自动命名（如 `exec-1`）。
        #[serde(default)]
        name: String,
        /// 要执行的命令及其参数，第一个元素是可执行文件。
        command: Vec<String>,
        /// 传给该命令的额外环境变量，默认没有。
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// stdout 内容的解析格式，默认 `plaintext`。
        #[serde(default)]
        format: Format,
        /// 命令执行的超时，缺省时使用 `refresh.timeout`。
        #[serde(default, deserialize_with = "de::opt_duration")]
        timeout: Option<Duration>,
        /// 最多保留多少个可用代理（`0` 表示全部保留），默认不限。
        ///
        /// 健康检查必须探测拿到的每一条，所以在默认并发下大型列表要跑很久；
        /// 这个上限让池子的规模可控。
        #[serde(default)]
        limit: Option<usize>,
        /// 是否启用该订阅源，默认 `true`。
        #[serde(default = "default_true")]
        enabled: bool,
    },
}

impl SubscriberConfig {
    /// 订阅源名称；未命名时由 [`Config::normalize`] 填充。
    pub fn name(&self) -> &str {
        match self {
            SubscriberConfig::Http { name, .. }
            | SubscriberConfig::Lua { name, .. }
            | SubscriberConfig::File { name, .. }
            | SubscriberConfig::Exec { name, .. } => name,
        }
    }

    /// 订阅源的种类字符串：`lua`、`http`、`file` 或 `exec`。
    pub fn kind(&self) -> &'static str {
        match self {
            SubscriberConfig::Http { .. } => "http",
            SubscriberConfig::Lua { .. } => "lua",
            SubscriberConfig::File { .. } => "file",
            SubscriberConfig::Exec { .. } => "exec",
        }
    }

    /// 该订阅源是否启用。
    pub fn enabled(&self) -> bool {
        match self {
            SubscriberConfig::Http { enabled, .. }
            | SubscriberConfig::Lua { enabled, .. }
            | SubscriberConfig::File { enabled, .. }
            | SubscriberConfig::Exec { enabled, .. } => *enabled,
        }
    }

    /// 响应内容的解析格式。
    ///
    /// `lua` 条目解析的是脚本 `print` 出来的内容，默认 `plaintext`。
    pub fn format(&self) -> Format {
        match self {
            SubscriberConfig::Http { format, .. }
            | SubscriberConfig::File { format, .. }
            | SubscriberConfig::Exec { format, .. }
            | SubscriberConfig::Lua { format, .. } => *format,
        }
    }

    /// 生效的条数上限：`0` 表示不限，未设置时也不限。
    pub fn limit(&self) -> Option<usize> {
        let limit = match self {
            SubscriberConfig::Http { limit, .. }
            | SubscriberConfig::Lua { limit, .. }
            | SubscriberConfig::File { limit, .. }
            | SubscriberConfig::Exec { limit, .. } => *limit,
        };
        match limit {
            Some(0) | None => None,
            Some(explicit) => Some(explicit),
        }
    }

    /// 覆盖订阅源名称。
    fn set_name(&mut self, name: String) {
        match self {
            SubscriberConfig::Http { name: n, .. }
            | SubscriberConfig::Lua { name: n, .. }
            | SubscriberConfig::File { name: n, .. }
            | SubscriberConfig::Exec { name: n, .. } => *n = name,
        }
    }
}

/// 订阅源响应内容如何转换为代理 URL。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// 每行一个代理；`#` 开始的行是注释。
    #[default]
    Plaintext,
    /// 代理组成的 JSON 数组（或包含数组的对象）。
    Json,
    /// Clash / Clash.Meta 的 `proxies:` 列表。
    Clash,
}

impl Format {
    /// 全部格式，便于遍历与提示。
    pub const ALL: [Format; 3] = [Format::Plaintext, Format::Json, Format::Clash];

    /// 格式的小写名称，与 YAML 中使用的值一致。
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

/// 配置了多个健康检查目标时，“存活”的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthRequirement {
    /// 每个目标都必须应答。某个目标不可达的代理即为死亡，即使它能
    /// 到达其他目标。该选项很严格：在某些目标难以访问的网络上，它可能
    /// 让代理池被清空。
    All,
    /// 至少有一个目标应答即可。
    ///
    /// 这是默认值：保留一个能到达*某些东西*的代理，比什么都不分发更
    /// 有用，而且每目标的结果仍然精确显示每个代理能到达什么、不能到达
    /// 什么（`GET /api/v1/proxies` 的 `probes` 字段）。
    #[default]
    Any,
}

impl HealthRequirement {
    /// 全部判定要求，便于遍历与提示。
    pub const ALL: [HealthRequirement; 2] = [HealthRequirement::All, HealthRequirement::Any];

    /// 判定要求的小写名称，与 YAML 中使用的值一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            HealthRequirement::All => "all",
            HealthRequirement::Any => "any",
        }
    }

    /// 给定通过的目标数与目标总数，判断是否满足该判定要求。
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
    /// 加载配置：显式路径优先，其次是 `$PROXYGATE_CONFIG`，然后是
    /// `./config.yaml`，最后是 `~/.config/proxygate/config.yaml`。
    ///
    /// 返回配置以及它的来源路径（如果有）。
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

    /// 候选配置位置，按优先级排序。
    pub fn default_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("config.yaml")];
        if let Some(home) = home_dir() {
            paths.push(home.join(".config").join("proxygate").join("config.yaml"));
        }
        paths
    }

    /// 填充派生值，并拒绝无法工作的配置。
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

    /// 校验各字段的取值范围与地址格式。
    fn validate(&self) -> Result<()> {
        if self.server.proxy.trim().is_empty() {
            return Err(Error::Config("server.proxy must not be empty".into()));
        }
        if self.server.proxy.trim().to_socket_addrs().is_err() {
            return Err(Error::Config(format!(
                "server.proxy `{}` is not a valid `host:port` address",
                self.server.proxy
            )));
        }
        let api = self.server.api_address();
        if api.is_empty() {
            return Err(Error::Config("server.api must not be empty".into()));
        }
        if api.to_socket_addrs().is_err() {
            return Err(Error::Config(format!(
                "server.api `{}` is not a valid `host:port` address (or `same` to share the proxy port)",
                self.server.api
            )));
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
                SubscriberConfig::Lua {
                    lua_code, lua_file, ..
                } => match (lua_code.as_deref(), lua_file.as_deref()) {
                    (Some(code), None) if code.trim().is_empty() => {
                        return Err(Error::Config(format!(
                            "subscriber `{}` has an empty `lua_code`",
                            subscriber.name()
                        )));
                    }
                    (None, Some(path)) if path.as_os_str().is_empty() => {
                        return Err(Error::Config(format!(
                            "subscriber `{}` has an empty `lua_file`",
                            subscriber.name()
                        )));
                    }
                    // 两个都给：`lua_code` 胜出（YAML 里同时写多半是复制粘贴），
                    // 但这是配置错误，直接说清楚。
                    (Some(_), Some(_)) => {
                        return Err(Error::Config(format!(
                            "subscriber `{}` sets both `lua_code` and `lua_file`; pick one",
                            subscriber.name()
                        )));
                    }
                    (None, None) => {
                        return Err(Error::Config(format!(
                            "subscriber `{}` needs `lua_code` or `lua_file`",
                            subscriber.name()
                        )));
                    }
                    _ => {}
                },
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

    /// 缓存目录：依次取 `state.dir`、`$PROXYGATE_CACHE_DIR`，然后是平台的
    /// 用户缓存目录（Windows 上是 `%LOCALAPPDATA%\proxygate`，其它平台是
    /// `~/.cache/proxygate`）。
    ///
    /// 都没拿到时退回当前目录下的 `.proxygate`，而不是直接失败：只读环境
    /// 里缓存会退化成"永不新鲜"，但程序仍然能跑。
    pub fn cache_dir(&self) -> PathBuf {
        if let Some(dir) = &self.state.dir {
            return dir.clone();
        }
        if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        platform_cache_dir().unwrap_or_else(|| PathBuf::from(".proxygate"))
    }

    /// 解析后的网关凭据（如果有）。
    pub fn gateway_credentials(&self) -> Result<Option<(String, String)>> {
        self.gateway
            .auth
            .as_deref()
            .map(parse_basic_auth)
            .transpose()
    }
}

/// 拆分 `user:password`（密码可以为空，且可以包含冒号）。
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

/// 用于配置与缓存路径的 `~` 展开辅助函数。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        // Windows 上没有 `HOME`，`USERPROFILE` 才是那个意思。
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

/// 平台约定的用户缓存目录。
///
/// Windows 上的进程通常没有 `HOME`，所以不能只认那一个变量，否则缓存放到了
/// 当前目录里；那里用 `%LOCALAPPDATA%`，与其它程序的习惯一致。
fn platform_cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(local).join("proxygate"));
        }
    }
    home_dir().map(|home| home.join(".cache").join("proxygate"))
}

/// 解析 `30s`、`10m`、`2h`、`1d`、`250ms`、`1h30m`，或纯数字表示的
/// 秒数。
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

/// 紧凑的人类可读时长（`30s`、`10m`、`1h30m`）。
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

/// serde 反序列化辅助：把 YAML 标量解析为 [`Duration`]。
mod de {
    use super::{Duration, parse_duration};
    use serde::de::{self, Visitor};
    use serde::{Deserialize, Deserializer};
    use std::fmt;

    /// 把 YAML 标量解释为 [`Duration`] 的 serde 访问者。
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

    /// 反序列化一个必需的时长字段。
    pub fn duration<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Duration, D::Error> {
        deserializer.deserialize_any(DurationVisitor)
    }

    /// 反序列化一个可缺省的时长字段。
    pub fn opt_duration<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Duration>, D::Error> {
        Ok(Option::<DurationSeed>::deserialize(deserializer)?.map(|seed| seed.0))
    }

    /// 包装类型，使被 `Option` 包裹的时长复用上面的访问者。
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

/// `server.proxy` 的默认值：`127.0.0.1:8080`。
fn default_proxy_addr() -> String {
    "127.0.0.1:8080".to_string()
}

/// `server.api` 的默认值：`127.0.0.1:8081`。
fn default_api_addr() -> String {
    "127.0.0.1:8081".to_string()
}

/// `refresh.interval` 的默认值：10 分钟。
fn default_refresh_interval() -> Duration {
    Duration::from_secs(600)
}

/// `refresh.timeout` 的默认值：20 秒。
fn default_refresh_timeout() -> Duration {
    Duration::from_secs(20)
}

/// 特意选用两个目标：一个只有在代理具备真正的国际连通性时才可用，
/// 另一个国内端点用来证明隧道并非对所有目标都不通。
///
/// 探测是*通过代理*发起的，所以某个目标在本机不可达没有关系——需要
/// 能到达它的是代理。默认 `require: any` 接受能到达其中任意一个的代理，
/// 每目标的结果会记录实际到达了哪个。
fn default_health_targets() -> Vec<String> {
    vec![
        "https://www.google.com/generate_204".to_string(),
        "https://cn.bing.com/".to_string(),
    ]
}

/// `health.interval` 的默认值：30 秒。
fn default_health_interval() -> Duration {
    Duration::from_secs(30)
}

/// `health.timeout` 的默认值：5 秒。
fn default_health_timeout() -> Duration {
    Duration::from_secs(5)
}

/// `health.concurrency` 的默认值：100。
fn default_health_concurrency() -> usize {
    100
}

/// `health.max_failures` 的默认值：3。
fn default_max_failures() -> u32 {
    3
}

/// `selection.reuse_after` 的默认值：30 分钟。
fn default_reuse_after() -> Duration {
    Duration::from_secs(30 * 60)
}

/// `gateway.retries` 的默认值：2。
fn default_retries() -> u32 {
    2
}

/// `gateway.connect_timeout` 的默认值：10 秒。
fn default_connect_timeout() -> Duration {
    Duration::from_secs(10)
}

/// `enabled` 字段的默认值：`true`。
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

        // 两个键同时出现：取并集，顺序保留，并去重。
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
        // `config.example.yaml` 会被嵌入并由 `proxygate --example-config`
        // 打印，因此它必须始终能解析并通过校验。
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
        // 示例里必须真的出现一段脚本，否则「怎么用 lua 订阅源」就没地方看。
        assert!(EXAMPLE_CONFIG.contains("type: lua"));
        assert!(EXAMPLE_CONFIG.contains("lua_code: |"));

        // 示例配置不得向客户端索取凭据：它应当可以直接用在回环地址上，而
        // 网关认证是部署时的决定，不该写进签入仓库的文件。在这里重新加回
        // 一行 `auth:` 会让该测试失败。
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
    fn the_api_can_share_the_proxy_port() {
        let mut config: Config =
            serde_yaml::from_str("server:\n  proxy: 127.0.0.1:8080\n  api: same\n").unwrap();
        config.normalize().unwrap();
        assert!(config.server.shares_port());
        assert_eq!(config.server.api_address(), "127.0.0.1:8080");

        // `proxy` 被接受为同一个词，直接写出地址也一样。
        for spelling in ["proxy", "127.0.0.1:8080"] {
            let mut config: Config = serde_yaml::from_str(&format!(
                "server:\n  proxy: 127.0.0.1:8080\n  api: {spelling}\n"
            ))
            .unwrap();
            config.normalize().unwrap();
            assert!(config.server.shares_port(), "{spelling}");
            assert_eq!(config.server.api_address(), "127.0.0.1:8080");
        }

        // 分开的端口仍然分开，默认值也不共用端口。
        let mut config: Config =
            serde_yaml::from_str("server:\n  proxy: 127.0.0.1:8080\n  api: 127.0.0.1:8081\n")
                .unwrap();
        config.normalize().unwrap();
        assert!(!config.server.shares_port());
        assert!(!Config::default().server.shares_port());

        // 格式错误的地址仍然会被拒绝，且错误信息会提到 `same`。
        let mut broken: Config = serde_yaml::from_str("server:\n  api: not-an-address\n").unwrap();
        let error = broken.normalize().unwrap_err().to_string();
        assert!(
            error.contains("same") || error.contains("not-an-address"),
            "{error}"
        );
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
                    limit: None,
                    enabled: true,
                },
                SubscriberConfig::File {
                    name: "same".into(),
                    path: "b.txt".into(),
                    format: Format::Plaintext,
                    limit: None,
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

    #[test]
    fn a_lua_subscriber_takes_its_extra_keys_as_parameters() {
        let raw = r#"
subscribers:
  - script_name: my_scraper
    type: lua
    target_url: https://api.example.com/data.json
    page_size: 500
    debug: true
    lua_code: |
      print("1.2.3.4:8080")
"#;
        let mut config: Config = serde_yaml::from_str(raw).unwrap();
        config.normalize().unwrap();

        let subscriber = &config.subscribers[0];
        // `script_name` 是 `name` 的别名。
        assert_eq!(subscriber.name(), "my_scraper");
        assert_eq!(subscriber.kind(), "lua");
        assert_eq!(subscriber.format(), Format::Plaintext);

        let SubscriberConfig::Lua { params, limit, .. } = subscriber else {
            panic!("expected a lua subscriber");
        };
        assert_eq!(*limit, None);
        let keys: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["debug", "page_size", "target_url"]);
        assert_eq!(
            params["target_url"].as_str(),
            Some("https://api.example.com/data.json")
        );
        assert_eq!(params["page_size"].as_u64(), Some(500));
        assert_eq!(params["debug"].as_bool(), Some(true));
    }

    #[test]
    fn a_lua_subscriber_needs_exactly_one_source() {
        // 两种都不给。
        let mut config: Config = serde_yaml::from_str("subscribers:\n  - type: lua\n").unwrap();
        let error = config.normalize().unwrap_err().to_string();
        assert!(error.contains("needs `lua_code` or `lua_file`"), "{error}");

        // 两种都给：说清楚，不猜。
        let mut config: Config = serde_yaml::from_str(
            "subscribers:\n  - type: lua\n    lua_code: print(1)\n    lua_file: ./a.lua\n",
        )
        .unwrap();
        let error = config.normalize().unwrap_err().to_string();
        assert!(error.contains("sets both"), "{error}");

        // `lua_code` 是空白。
        let mut config: Config =
            serde_yaml::from_str("subscribers:\n  - type: lua\n    lua_code: \"   \"\n").unwrap();
        let error = config.normalize().unwrap_err().to_string();
        assert!(error.contains("empty `lua_code`"), "{error}");

        // 只有 `lua_file` 是合法的。
        let mut config: Config =
            serde_yaml::from_str("subscribers:\n  - type: lua\n    lua_file: ./a.lua\n").unwrap();
        config.normalize().unwrap();
        assert_eq!(config.subscribers[0].name(), "lua-1");
    }

    #[test]
    fn a_limit_of_zero_means_no_limit() {
        let mut config: Config = serde_yaml::from_str(
            "subscribers:\n  - type: http\n    url: https://example.com/a.txt\n    limit: 0\n",
        )
        .unwrap();
        config.normalize().unwrap();
        assert_eq!(config.subscribers[0].limit(), None);

        let mut config: Config = serde_yaml::from_str(
            "subscribers:\n  - type: http\n    url: https://example.com/a.txt\n    limit: 25\n",
        )
        .unwrap();
        config.normalize().unwrap();
        assert_eq!(config.subscribers[0].limit(), Some(25));
    }
}
