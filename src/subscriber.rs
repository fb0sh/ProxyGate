//! 订阅源：代理从哪里来。
//!
//! 只有四种类型：
//!
//! * `http` —— 拉取一个 URL（`proxies.txt`、API 或订阅）；
//! * `file` —— 读取本地文件；
//! * `exec` —— 运行一条命令并读取其 stdout；
//! * `lua` —— 运行一段 Lua 脚本，脚本 `print` 出来的每一行就是一条代理。
//!
//! 无论载荷长什么样，订阅源唯一的职责就是产出代理 URL。`plaintext`、
//! `json` 和 `clash` 三种格式由内置解析器覆盖；剩下的（要签名、要翻页、
//! 要按字段拼串的 API）都交给 `lua`——它既能发请求又能写逻辑，而且不用
//! 为了一个来源去改 ProxyGate 本身。真正需要外部程序的时候才用 `exec`。
//!
//! # Lua 脚本
//!
//! ```yaml
//! subscribers:
//!   - name: my_scraper
//!     type: lua
//!     target_url: https://api.example.com/data.json   # 额外键 -> 全局变量
//!     limit: 500
//!     lua_code: |
//!       for page = 1, 10 do
//!         local data = fetch_json(target_url .. "?page=" .. page)
//!         for _, item in ipairs(data.proxies or {}) do
//!           print(item.ip .. ":" .. item.port)
//!         end
//!       end
//! ```
//!
//! 脚本里可用的全局变量与函数：
//!
//! | 名称 | 说明 |
//! | --- | --- |
//! | 配置里的额外键 | 原样变成全局变量（`target_url`、`token`……），数字、布尔、字符串和表都支持 |
//! | `print(...)` | 每个参数用 tab 连接输出一行；**这些行就是候选代理** |
//! | `fetch(url)` | 同步发一次 GET，返回响应体字符串；非 2xx 会抛错 |
//! | `fetch_json(url)` | 同上，但把响应体解析成 Lua 表 |
//! | `json_encode(v)` / `json_decode(s)` | Lua 值与 JSON 字符串互转 |
//! | `log(...)` | 以 `info` 级别写进 ProxyGate 日志，不影响输出 |
//!
//! 这是一个**沙箱**：不加载 `io`、`os`、`package`、`debug`，`dofile`、
//! `loadfile`、`load`、`require` 也都被摘掉了，脚本只能通过 `fetch` 接触
//! 外部世界。整段脚本受订阅源的 `timeout`（缺省用 `refresh.timeout`）限制，
//! 连 `while true do end` 这种死循环也会被指令钩子掐断。
//!
//! 输出有上限：最多收集 100,000 行，`print` 的单个参数超过 4 KiB 会被截断
//! （末尾加省略号）。超过行数上限的部分会被丢弃，不会让进程吃掉所有内存。
//!
//! 注意：`exec` 会以 ProxyGate 进程的权限运行配置文件里的命令。这是一条
//! 有意留出的逃生通道——请把 `config.yaml` 当作可信输入。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue, Variadic};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value as JsonValue;
use tokio::process::Command;
use url::Url;

use crate::config::{Config, Format, SubscriberConfig};
use crate::error::{Error, Result};
use crate::model::{self, ProxyScheme};
use crate::progress::{FetchEvent, Progress};

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
        self.fetch_all_reporting(&()).await
    }

    /// 与 [`SubscriberSet::fetch_all`] 相同，但把进度发给 `progress`。
    ///
    /// 用 `FuturesUnordered` 而不是 `join_all`：前者在**每个**订阅源完成时
    /// 立刻返回，调用方因此能立刻看到是哪个来源、拿到了多少，而不是等最慢
    /// 的那个（`freeproxy-gh` 要四分钟）一起返回。
    pub async fn fetch_all_reporting(&self, progress: &dyn Progress) -> Vec<FetchOutcome> {
        self.fetch_all_streaming(progress, |_| {}).await
    }

    /// 与 [`SubscriberSet::fetch_all_reporting`] 相同，但每个订阅源一完成就
    /// 调一次 `on_finished`。
    ///
    /// 调用方用它做**增量落盘**：一轮全量刷新可能跑四分钟，中途被 Ctrl-C
    /// 或断电不该把已经拿到的代理全丢掉。
    pub async fn fetch_all_streaming(
        &self,
        progress: &dyn Progress,
        mut on_finished: impl FnMut(&FetchOutcome),
    ) -> Vec<FetchOutcome> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        let mut pending = FuturesUnordered::new();
        for subscriber in self.active() {
            pending.push(self.fetch_one_reporting(subscriber, progress));
        }

        let mut outcomes = Vec::with_capacity(pending.len());
        while let Some(outcome) = pending.next().await {
            on_finished(&outcome);
            outcomes.push(outcome);
        }
        outcomes
    }

    /// 拉取单个订阅源，并把任何失败都转换为 `FetchOutcome::error`。
    pub async fn fetch_one(&self, subscriber: &SubscriberConfig) -> FetchOutcome {
        self.fetch_one_reporting(subscriber, &()).await
    }

    /// 与 [`SubscriberSet::fetch_one`] 相同，但把进度发给 `progress`。
    ///
    /// 无论成功失败都会发出一个 [`FetchEvent::Finished`]，所以调用方不需要
    /// 自己在每条返回路径上补事件。
    pub async fn fetch_one_reporting(
        &self,
        subscriber: &SubscriberConfig,
        progress: &dyn Progress,
    ) -> FetchOutcome {
        progress.fetch(FetchEvent::Started {
            name: subscriber.name(),
            kind: subscriber.kind(),
            format: subscriber.format(),
        });

        let outcome = self.fetch_one_inner(subscriber, progress).await;

        progress.fetch(FetchEvent::Finished(&outcome));
        outcome
    }

    /// 真正的拉取：取响应体、解析、归一化、截断。
    async fn fetch_one_inner(
        &self,
        subscriber: &SubscriberConfig,
        progress: &dyn Progress,
    ) -> FetchOutcome {
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

        let payload = match self.read_payload(subscriber, progress).await {
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
        outcome.truncated = apply_limit(&mut outcome.proxies, effective_limit(subscriber));

        outcome.duration = started.elapsed();
        outcome
    }

    /// 按订阅源类型读取响应体。
    async fn read_payload(
        &self,
        subscriber: &SubscriberConfig,
        progress: &dyn Progress,
    ) -> Result<String> {
        match subscriber {
            SubscriberConfig::Http {
                url,
                headers,
                timeout,
                ..
            } => {
                self.fetch_http(subscriber, url, headers, *timeout, progress)
                    .await
            }
            SubscriberConfig::Lua {
                lua_code,
                lua_file,
                params,
                timeout,
                ..
            } => {
                let code = match (lua_code.as_deref(), lua_file.as_deref()) {
                    (Some(code), _) => code.to_string(),
                    (None, Some(path)) => {
                        tokio::fs::read_to_string(path)
                            .await
                            .map_err(|e| Error::Subscriber {
                                name: subscriber.name().to_string(),
                                message: format!("cannot read {}: {e}", path.display()),
                            })?
                    }
                    (None, None) => {
                        return Err(Error::Subscriber {
                            name: subscriber.name().to_string(),
                            message: "needs `lua_code` or `lua_file`".into(),
                        });
                    }
                };
                run_lua(
                    subscriber,
                    params,
                    &code,
                    timeout.unwrap_or(self.timeout),
                    &self.client,
                )
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
    ///
    /// 响应体是流式读的，而且每隔 [`DOWNLOAD_REPORT_INTERVAL`] 发一次
    /// [`FetchEvent::Download`]：`freeproxy-gh` 的 2.5 MB 在这里要四分钟，
    /// 不报进度就只像是卡住了。
    async fn fetch_http(
        &self,
        subscriber: &SubscriberConfig,
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Option<Duration>,
        progress: &dyn Progress,
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
        use futures_util::StreamExt;

        let name = subscriber.name().to_string();
        let started = Instant::now();
        let mut body: Vec<u8> = Vec::new();
        let mut last_report = Instant::now();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| Error::Subscriber {
                name: name.clone(),
                message: format!(
                    "cannot read the response body: {}",
                    crate::error::describe_reqwest_error(&error)
                ),
            })?;
            body.extend_from_slice(&chunk);

            if last_report.elapsed() >= DOWNLOAD_REPORT_INTERVAL {
                progress.fetch(FetchEvent::Download {
                    name: subscriber.name(),
                    bytes: body.len() as u64,
                    elapsed: started.elapsed(),
                });
                last_report = Instant::now();
            }
        }

        String::from_utf8(body).map_err(|error| Error::Subscriber {
            name,
            message: format!("the response body is not UTF-8: {error}"),
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

/// Lua 沙箱里每执行这么多条 VM 指令调一次钩子，用来检查脚本是否超时。
///
/// 只有钩子能掐断 `while true do end`：`tokio::time::timeout` 需要脚本先
/// 让出执行权，而纯计算的死循环永远不会让出。
const LUA_HOOK_INTERVAL: u32 = 10_000;

/// `print` 最多收集多少行，防止脚本把内存写爆。
const LUA_MAX_LINES: usize = 100_000;

/// `print` 的单个参数最长保留多少字节，超出部分截断。
const LUA_MAX_ARG: usize = 4 * 1024;

/// 运行一段 Lua 订阅源脚本，返回它 `print` 出来的全部内容（换行连接）。
///
/// 脚本可用的全局变量与函数见本模块的文档。这里是**一次性**沙箱：每次刷新
/// 都新建一个 Lua 状态，脚本之间不共享任何东西——一个来源的脚本改坏了全
/// 局变量，不会影响另一个来源。
async fn run_lua(
    subscriber: &SubscriberConfig,
    params: &BTreeMap<String, serde_yaml::Value>,
    code: &str,
    timeout: Duration,
    client: &reqwest::Client,
) -> Result<String> {
    let name = subscriber.name().to_string();
    let fail = |message: String| Error::Subscriber {
        name: name.clone(),
        message,
    };

    // 不加载 `io`、`os`、`package`、`debug`：脚本的唯一出口是 `fetch`。
    // 注意 base 库没有开关，所以下面还要把 `dofile` 之类逐个摘掉。
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE,
        LuaOptions::default(),
    )
    .map_err(|e| fail(format!("cannot start a Lua state: {e}")))?;

    for (key, value) in params {
        // `key:` with nothing after it means "no value", i.e. leave the global
        // unset rather than handing the script a null sentinel.
        if value.is_null() {
            continue;
        }
        let json = serde_json::to_value(value)
            .map_err(|e| fail(format!("cannot pass `{key}` to Lua: {e}")))?;
        let value = lua
            .to_value(&json)
            .map_err(|e| fail(format!("cannot pass `{key}` to Lua: {e}")))?;
        lua.globals()
            .set(key.as_str(), value)
            .map_err(|e| fail(format!("cannot set `{key}` in Lua: {e}")))?;
    }

    // base 库里会读写文件或再加载代码的函数一律摘掉。
    for forbidden in ["dofile", "loadfile", "load", "require"] {
        lua.globals()
            .set(forbidden, LuaValue::Nil)
            .map_err(|e| fail(format!("cannot harden the Lua sandbox: {e}")))?;
    }

    // `print` 是唯一的输出通道：每次调用收集一行。
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    let print = lua
        .create_function(move |_, args: Variadic<LuaValue>| {
            let mut parts = Vec::with_capacity(args.len());
            for arg in args {
                parts.push(truncate_text(arg.to_string()?, LUA_MAX_ARG));
            }
            let mut sink = sink.lock().unwrap_or_else(|e| e.into_inner());
            if sink.len() < LUA_MAX_LINES {
                sink.push(parts.join("\t"));
            }
            Ok(())
        })
        .map_err(|e| fail(format!("cannot define `print` for Lua: {e}")))?;
    lua.globals()
        .set("print", print)
        .map_err(|e| fail(format!("cannot define `print` for Lua: {e}")))?;

    // `fetch` / `fetch_json`：脚本访问外部世界的唯一方式，共用订阅源的
    // HTTP 客户端（同一份 UA、连接池与 TLS 配置）。
    let fetch_client = client.clone();
    let fetch = lua
        .create_async_function(move |_, url: String| {
            let client = fetch_client.clone();
            async move {
                http_get(&client, &url, timeout)
                    .await
                    .map_err(mlua::Error::runtime)
            }
        })
        .map_err(|e| fail(format!("cannot define `fetch` for Lua: {e}")))?;
    lua.globals()
        .set("fetch", fetch)
        .map_err(|e| fail(format!("cannot define `fetch` for Lua: {e}")))?;

    let fetch_json_client = client.clone();
    let fetch_json = lua
        .create_async_function(move |lua, url: String| {
            let client = fetch_json_client.clone();
            async move {
                let body = http_get(&client, &url, timeout)
                    .await
                    .map_err(mlua::Error::runtime)?;
                let value: JsonValue = serde_json::from_str(&body).map_err(|e| {
                    mlua::Error::runtime(format!("`{url}` did not return JSON: {e}"))
                })?;
                lua.to_value(&value)
            }
        })
        .map_err(|e| fail(format!("cannot define `fetch_json` for Lua: {e}")))?;
    lua.globals()
        .set("fetch_json", fetch_json)
        .map_err(|e| fail(format!("cannot define `fetch_json` for Lua: {e}")))?;

    let encode = lua
        .create_function(|lua, value: LuaValue| {
            let json: JsonValue = lua.from_value(value)?;
            serde_json::to_string(&json).map_err(mlua::Error::runtime)
        })
        .map_err(|e| fail(format!("cannot define `json_encode` for Lua: {e}")))?;
    lua.globals()
        .set("json_encode", encode)
        .map_err(|e| fail(format!("cannot define `json_encode` for Lua: {e}")))?;

    let decode = lua
        .create_function(|lua, text: String| {
            let json: JsonValue = serde_json::from_str(&text).map_err(mlua::Error::runtime)?;
            lua.to_value(&json)
        })
        .map_err(|e| fail(format!("cannot define `json_decode` for Lua: {e}")))?;
    lua.globals()
        .set("json_decode", decode)
        .map_err(|e| fail(format!("cannot define `json_decode` for Lua: {e}")))?;

    let log_name = name.clone();
    let log = lua
        .create_function(move |_, args: Variadic<LuaValue>| {
            let mut parts = Vec::with_capacity(args.len());
            for arg in args {
                parts.push(truncate_text(arg.to_string()?, LUA_MAX_ARG));
            }
            tracing::info!(subscriber = %log_name, "{}", parts.join(" "));
            Ok(())
        })
        .map_err(|e| fail(format!("cannot define `log` for Lua: {e}")))?;
    lua.globals()
        .set("log", log)
        .map_err(|e| fail(format!("cannot define `log` for Lua: {e}")))?;

    // 指令钩子：纯计算的死循环只有它能掐断。
    //
    // 必须用 `set_global_hook`：`exec_async` 会把脚本放进一条新协程执行，
    // 而 `set_hook` 只管当前线程。
    let deadline = Instant::now() + timeout;
    let expired = format!("script timed out after {timeout:?}");
    let hook_expired = expired.clone();
    lua.set_global_hook(
        mlua::HookTriggers::new().every_nth_instruction(LUA_HOOK_INTERVAL),
        move |_, _| {
            if Instant::now() >= deadline {
                Err(mlua::Error::runtime(hook_expired.clone()))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    )
    .map_err(|e| fail(format!("cannot arm the Lua watchdog: {e}")))?;

    let chunk = lua.load(code).set_name(format!("subscriber `{name}`"));
    match tokio::time::timeout(timeout, chunk.exec_async()).await {
        Err(_) => Err(fail(expired)),
        Ok(Err(error)) => {
            if Instant::now() >= deadline {
                Err(fail(expired))
            } else {
                Err(fail(format!("Lua error: {error}")))
            }
        }
        Ok(Ok(())) => {
            let lines = lines.lock().unwrap_or_else(|e| e.into_inner());
            Ok(lines.join("\n"))
        }
    }
}

/// 一次同步 GET（对 Lua 而言是同步的），返回响应体文本。
async fn http_get(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("{url}: {}", crate::error::describe_reqwest_error(&e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    response
        .text()
        .await
        .map_err(|e| format!("{url}: {}", crate::error::describe_reqwest_error(&e)))
}

/// 截断一段文本到 `max` 字节，按字符边界切，末尾加省略号。
fn truncate_text(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_string();
    truncated.push('…');
    truncated
}

/// 来源的生效条数上限，见 [`SubscriberConfig::limit`]。
pub fn effective_limit(subscriber: &SubscriberConfig) -> Option<usize> {
    subscriber.limit()
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

/// 下载响应体时，两次进度报告之间的最小间隔。
const DOWNLOAD_REPORT_INTERVAL: Duration = Duration::from_secs(10);

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
    fn limits_come_from_the_lua_subscriber_config() {
        use crate::config::SubscriberConfig;

        let lua = |limit| SubscriberConfig::Lua {
            name: "x".into(),
            lua_code: Some("print('1.1.1.1:8080')".into()),
            lua_file: None,
            format: Format::Plaintext,
            timeout: None,
            limit,
            enabled: true,
            params: Default::default(),
        };

        // 只有脚本声明了 `limit` 才有上限。
        assert_eq!(effective_limit(&lua(None)), None);
        assert_eq!(effective_limit(&lua(Some(25))), Some(25));
        // `0` 表示不限。
        assert_eq!(effective_limit(&lua(Some(0))), None);

        // 其他种类没有 `limit` 字段，永远不限。
        let file = SubscriberConfig::File {
            name: "f".into(),
            path: "x".into(),
            format: Format::Plaintext,
            limit: None,
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
                limit: None,
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

    /// 记录收到的事件，用来断言进度真的发出去了。
    #[derive(Default)]
    struct Recorder {
        started: std::sync::Mutex<Vec<String>>,
        finished: std::sync::Mutex<Vec<String>>,
    }

    impl Progress for Recorder {
        fn fetch(&self, event: FetchEvent<'_>) {
            match event {
                FetchEvent::Started { name, .. } => {
                    self.started.lock().unwrap().push(name.to_string())
                }
                FetchEvent::Finished(outcome) => self.finished.lock().unwrap().push(format!(
                    "{}={}",
                    outcome.name,
                    outcome.count()
                )),
                FetchEvent::Download { .. } => {}
            }
        }
    }

    #[tokio::test]
    async fn fetch_reports_started_and_finished_for_every_subscriber() {
        let dir = std::env::temp_dir().join(format!("proxygate-progress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.txt");
        std::fs::write(&good, "1.2.3.4:8080\n").unwrap();

        let config = Config {
            subscribers: vec![
                SubscriberConfig::File {
                    name: "good".into(),
                    path: good,
                    format: Format::Plaintext,
                    limit: None,
                    enabled: true,
                },
                SubscriberConfig::Exec {
                    name: "bad".into(),
                    command: shell("exit 3"),
                    env: BTreeMap::new(),
                    format: Format::Plaintext,
                    timeout: None,
                    limit: None,
                    enabled: true,
                },
            ],
            ..Config::default()
        };

        let recorder = std::sync::Arc::new(Recorder::default());
        let set = SubscriberSet::new(&config).unwrap();
        let outcomes = set.fetch_all_reporting(recorder.as_ref()).await;

        assert_eq!(outcomes.len(), 2);
        let mut started = recorder.started.lock().unwrap().clone();
        started.sort();
        assert_eq!(started, vec!["bad".to_string(), "good".to_string()]);

        // 失败也要有 Finished 事件，否则进度里会永远少一行。
        let mut finished = recorder.finished.lock().unwrap().clone();
        finished.sort();
        assert_eq!(finished, vec!["bad=0".to_string(), "good=1".to_string()]);
    }

    /// 用当前平台的 shell 跑一小段脚本，让 exec 订阅源的测试两边都能跑。
    fn shell(script: &str) -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), script.into()]
        } else {
            vec!["sh".into(), "-c".into(), script.into()]
        }
    }

    #[tokio::test]
    async fn exec_subscriber_uses_stdout() {
        let config = Config {
            subscribers: vec![SubscriberConfig::Exec {
                name: "custom".into(),
                // 注释行/空行的处理由 `parses_plaintext_lists` 覆盖，
                // 这里只验证 stdout 被当作载荷。
                command: shell("echo 1.2.3.4:8080"),
                env: BTreeMap::new(),
                format: Format::Plaintext,
                timeout: None,
                limit: None,
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
                    command: shell("exit 3"),
                    env: BTreeMap::new(),
                    format: Format::Plaintext,
                    timeout: None,
                    limit: None,
                    enabled: true,
                },
                SubscriberConfig::Exec {
                    name: "missing".into(),
                    command: vec!["/definitely/not/a/binary".into()],
                    env: BTreeMap::new(),
                    format: Format::Plaintext,
                    timeout: None,
                    limit: None,
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
                command: shell("echo 1.2.3.4:8080"),
                env: BTreeMap::new(),
                format: Format::Plaintext,
                timeout: None,
                limit: None,
                enabled: false,
            }],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        assert!(set.is_empty());
        assert!(set.fetch_all().await.is_empty());
    }

    /// 构造一个 `lua` 订阅源，超时给 5 秒，参数留空。
    fn lua_subscriber(code: &str) -> SubscriberConfig {
        SubscriberConfig::Lua {
            name: "lua".into(),
            lua_code: Some(code.into()),
            lua_file: None,
            format: Format::Plaintext,
            timeout: Some(Duration::from_secs(5)),
            limit: None,
            enabled: true,
            params: BTreeMap::new(),
        }
    }

    /// 用一个只含该订阅源的配置跑一次拉取。
    async fn run_one(subscriber: SubscriberConfig) -> FetchOutcome {
        let config = Config {
            subscribers: vec![subscriber],
            ..Config::default()
        };
        let mut outcomes = SubscriberSet::new(&config).unwrap().fetch_all().await;
        outcomes.remove(0)
    }

    /// 把归一化后的代理渲染成规范的 `scheme://host:port` 形式，便于比较。
    fn rendered(outcome: &FetchOutcome) -> Vec<String> {
        outcome
            .proxies
            .iter()
            .map(|proxy| model::render_url(proxy, true))
            .collect()
    }

    #[tokio::test]
    async fn lua_prints_become_proxies() {
        let outcome = run_one(lua_subscriber(
            r#"
            for i = 1, 3 do
              print("10.0.0." .. i .. ":8080")
            end
            print("socks5://1.2.3.4:1080")
            "#,
        ))
        .await;

        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.kind, "lua");
        assert_eq!(
            rendered(&outcome),
            vec![
                "http://10.0.0.1:8080".to_string(),
                "http://10.0.0.2:8080".to_string(),
                "http://10.0.0.3:8080".to_string(),
                "socks5://1.2.3.4:1080".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn lua_output_is_parsed_with_the_configured_format() {
        let mut subscriber =
            lua_subscriber(r#"print('{"data":{"proxies":["10.1.1.1:8080","10.1.1.2:8080"]}}')"#);
        if let SubscriberConfig::Lua { format, .. } = &mut subscriber {
            *format = Format::Json;
        }
        let outcome = run_one(subscriber).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.count(), 2);
    }

    /// 直接运行一段脚本，返回它 `print` 出来的原始内容。
    ///
    /// 需要看清脚本到底输出了什么（而不是它被归一化成哪些代理）时用它。
    async fn lua_payload(code: &str, params: &[(&str, serde_yaml::Value)]) -> String {
        let subscriber = lua_subscriber(code);
        let SubscriberConfig::Lua { timeout, .. } = &subscriber else {
            unreachable!("`lua_subscriber` builds a Lua subscriber");
        };
        let mut merged: BTreeMap<String, serde_yaml::Value> = BTreeMap::new();
        for (key, value) in params {
            merged.insert((*key).to_string(), value.clone());
        }
        run_lua(
            &subscriber,
            &merged,
            code,
            timeout.unwrap(),
            &reqwest::Client::new(),
        )
        .await
        .expect("the script should run")
    }

    #[tokio::test]
    async fn lua_sees_extra_config_keys_as_globals() {
        // 额外键变成全局变量：字符串、数字、以及整个表都能用。
        let payload = lua_payload(
            r#"
            print(target_url)
            print(host .. ":" .. tostring(port))
            print("flag=" .. tostring(deep.flag))
            "#,
            &[
                ("target_url", serde_yaml::Value::from("hello")),
                ("host", serde_yaml::Value::from("10.1.2.3")),
                (
                    "port",
                    serde_yaml::Value::Number(serde_yaml::Number::from(8080)),
                ),
                ("deep", serde_yaml::from_str("flag: true").unwrap()),
            ],
        )
        .await;

        assert_eq!(payload, "hello\n10.1.2.3:8080\nflag=true");
    }

    #[tokio::test]
    async fn the_lua_sandbox_has_no_file_or_process_access() {
        let payload = lua_payload(
            r#"
            local missing = 0
            local names = {"io", "os", "package", "debug", "dofile", "loadfile", "load", "require"}
            for _, name in ipairs(names) do
              if _G[name] == nil then missing = missing + 1 end
            end
            print("missing=" .. missing)
            "#,
            &[],
        )
        .await;

        assert_eq!(payload, "missing=8");
    }

    #[tokio::test]
    async fn lua_can_fetch_json_from_an_endpoint() {
        let app = axum::Router::new().route(
            "/list",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "data": {"proxies": ["10.9.8.7:3128", "10.9.8.6:3128"]}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let outcome = run_one(lua_subscriber(&format!(
            r#"
            local data = fetch_json("http://{address}/list")
            for _, item in ipairs(data.data.proxies) do
              print(item)
            end
            print("encoded=" .. json_encode({{1, 2}}))
            "#,
        )))
        .await;
        server.abort();

        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(
            rendered(&outcome),
            vec![
                "http://10.9.8.7:3128".to_string(),
                "http://10.9.8.6:3128".to_string()
            ]
        );
        // 非代理行被拒绝，错误信息里带上原始那一行。
        assert_eq!(outcome.rejected.len(), 1);
        assert!(
            outcome.rejected[0].contains("encoded=[1,2]"),
            "{}",
            outcome.rejected[0]
        );
    }

    #[tokio::test]
    async fn lua_errors_are_reported_not_fatal() {
        let outcome = run_one(lua_subscriber("error('boom')")).await;
        assert!(!outcome.ok());
        let error = outcome.error.unwrap();
        assert!(error.contains("boom"), "{error}");
    }

    #[tokio::test]
    async fn a_failing_fetch_surfaces_as_a_subscriber_error() {
        // 端口 1 上没有任何东西在监听：脚本该失败，异常要带上 URL。
        let outcome = run_one(lua_subscriber(r#"print(fetch("http://127.0.0.1:1/nope"))"#)).await;
        assert!(!outcome.ok());
        let error = outcome.error.unwrap();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }

    #[tokio::test]
    async fn a_runaway_lua_script_is_stopped() {
        let mut subscriber = lua_subscriber("while true do end");
        if let SubscriberConfig::Lua { timeout, .. } = &mut subscriber {
            *timeout = Some(Duration::from_millis(300));
        }

        let started = Instant::now();
        let outcome = run_one(subscriber).await;
        let error = outcome.error.expect("a runaway script must fail");
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the watchdog took {:?} to fire",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_lua_limit_truncates_the_output() {
        let mut subscriber =
            lua_subscriber("for i = 1, 10 do print('10.0.0.' .. i .. ':8080') end");
        if let SubscriberConfig::Lua { limit, .. } = &mut subscriber {
            *limit = Some(3);
        }
        let outcome = run_one(subscriber).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.count(), 3);
        assert_eq!(outcome.truncated, 7);
    }

    #[tokio::test]
    async fn lua_output_is_collected_line_by_line() {
        // 一行的各个参数用 tab 连接；超长参数按字符边界截断后加省略号；
        // `print()` 打印空行。
        let long = "x".repeat(LUA_MAX_ARG * 2);
        let payload = lua_payload(&format!("print('a', 'b')\nprint('{long}')\nprint()"), &[]).await;

        let lines: Vec<&str> = payload.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "a\tb");
        assert_eq!(lines[1].chars().count(), LUA_MAX_ARG + 1);
        assert!(lines[1].ends_with('…'), "the long line should be elided");
        assert_eq!(lines[2], "");
    }
}
