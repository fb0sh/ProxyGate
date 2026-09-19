//! 订阅源：代理从哪里来。
//!
//! 只有一种：一段 **Lua 脚本**（[`SubscriberConfig`]）。脚本自己决定去哪里取、
//! 怎么翻页、怎么按字段拼装，最后**返回一组代理表**——`type`、`ip`、`port`，
//! 外加可选的 `auth`。
//!
//! 之所以不给「拉一个 URL」「跑一条命令」各留一个类型：那些来源真正的差别都在
//! 「怎么取、怎么拼」上，而那正是脚本擅长的事。少一个类型，就少一份要跟着
//! 各个面板 API 变的解析代码。
//!
//! # 脚本
//!
//! ```yaml
//! subscribers:
//!   - name: my_scraper
//!     target_url: https://api.example.com/data.json   # 不是 ProxyGate 的键 -> 脚本全局变量
//!     limit: 500                                      # 最多保留多少个可用代理
//!     timeout: 30s
//!     lua_code: |
//!       local result = {}
//!       for page = 1, 10 do
//!         local data = fetch_json(target_url .. "?page=" .. page)
//!         for _, item in ipairs(data.proxies or {}) do
//!           table.insert(result, { type = "http", ip = item.ip, port = item.port, auth = "" })
//!         end
//!       end
//!       return result
//! ```
//!
//! 脚本里能用的东西：
//!
//! | 名称 | 说明 |
//! | --- | --- |
//! | 配置里的额外键 | 原样变成全局变量（`target_url`、`token`……），数字、布尔、字符串和表都支持 |
//! | `fetch(url)` | 同步发一次 GET，返回响应体字符串；非 2xx 会抛错 |
//! | `fetch_json(url)` | 同上，但把响应体解析成 Lua 表 |
//! | `json_encode(v)` / `json_decode(s)` | Lua 值与 JSON 字符串互转 |
//! | `sleep(秒)` | 等一会儿再发下一个请求（抓分页站点时给对方留间隔） |
//! | `log(...)` / `print(...)` | 以 `info` 级别写进 ProxyGate 日志；**不是**输出代理的通道 |
//!
//! 返回值的每个条目是一个代理表，也可以直接写成字符串。字段与取值规则见
//! [`ParsedResult`]；一句话概括：`socks5` 一律升级成 `socks5h`（让代理去解析
//! 域名，本机 DNS 被污染时才不会把假地址交给代理），`socks4` 与认不出来的
//! 协议直接跳过，缺 `ip` 或端口不合法的条目记进 `rejected` 而不拖垮整个来源。
//!
//! 这是一个**沙箱**：不加载 `io`、`os`、`package`、`debug`，`dofile`、
//! `loadfile`、`load`、`require` 也都被摘掉了，脚本只能通过 `fetch` 接触外部
//! 世界。整段脚本受订阅源的 `timeout`（缺省用 `refresh.timeout`）限制，连
//! `while true do end` 这种死循环也会被指令钩子掐断；每次刷新都新建一个 Lua
//! 状态，脚本之间互不影响。
//!
//! 写进日志的单条消息超过 4 KiB 会被截断。配置文件本身是可信输入。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value as LuaValue, Variadic};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use url::Url;

use crate::config::{Config, SubscriberConfig, SubscriberSource};
use crate::error::{Error, Result};
use crate::model::{self, ProxyScheme};
use crate::pool::ProxyPool;
use crate::progress::{FetchEvent, Progress};
use crate::selector::SelectionOptions;

/// 一次订阅源拉取的结果。订阅源失败属于数据而非错误：
/// 一个来源坏掉不能拖停其他来源。
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    /// 订阅源名称。
    pub name: String,
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

/// 让订阅源脚本能"借"池子里的健康代理出门。
///
/// 存在的理由很实际：有些站点（比如 zdaye）按 IP 限流甚至直接封 IP，直连抓
/// 十来个请求就被 405 拦掉；换成一个健康代理的 IP 就又能拿到数据。它同时也
/// 保护了部署者自己的 IP——被站方拉黑的是代理，不是你的服务器。
///
/// 每个代理 URL 只建一次客户端并缓存：`reqwest::Client` 里装着连接池和 TLS
/// 配置，按请求新建的开销不值得。
#[derive(Debug)]
pub struct Egress {
    pool: Arc<ProxyPool>,
    options: SelectionOptions,
    timeout: Duration,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl Egress {
    /// 从池子里挑一个健康代理；池子空或全死时返回 `None`。
    fn pick(&self) -> Option<Url> {
        let selection = self.pool.select(self.options, SystemTime::now())?;
        // 日志里能看出"这次抓取是从哪个代理出去的"——排查某源为何慢/失败时
        // 这是第一个要看的线索。
        tracing::debug!(
            proxy = %selection.proxy.to_masked_string(),
            "fetching through a pooled proxy"
        );
        Some(selection.proxy.url.clone())
    }

    /// 该代理对应的客户端，按需创建并缓存。
    fn client(&self, proxy: &Url) -> Result<reqwest::Client> {
        let key = proxy.to_string();
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }

        let client = reqwest::Client::builder()
            // 既然指定了出口，就别再让环境变量里的代理插一脚。
            .no_proxy()
            .proxy(reqwest::Proxy::all(key.clone())?)
            .user_agent(crate::useragent::random())
            .timeout(self.timeout)
            .build()?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// 一个订阅源的出口策略：直连、走代理池，还是先直连失败后再走。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressPolicy {
    /// 永远直连（有些来源不该看到代理的 IP）。
    Direct,
    /// 优先走池子里的健康代理，没有可用代理时才直连。
    Pool,
    /// 先直连；失败了（被拦、超时、5xx）再借一个健康代理重试。
    ///
    /// 这是默认值：能直连的来源行为完全不变，抓不动的来源多一次机会。
    #[default]
    Fallback,
}

impl EgressPolicy {
    /// 策略的小写名称，与 YAML 中使用的值一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            EgressPolicy::Direct => "direct",
            EgressPolicy::Pool => "pool",
            EgressPolicy::Fallback => "fallback",
        }
    }
}

/// 负责拉取所有已配置的订阅源。
pub struct SubscriberSet {
    subscribers: Vec<SubscriberConfig>,
    timeout: Duration,
    client: reqwest::Client,
    /// 走代理池的出口；`None` 表示这个集合不碰池子（库的使用者直接构造时）。
    egress: Option<Arc<Egress>>,
}

impl SubscriberSet {
    /// 依据配置构建订阅源集合，并创建共享的 HTTP 客户端。
    ///
    /// 客户端带一个**普通桌面浏览器**的 User-Agent（取自内置的 UA 池）：拿
    /// `proxygate/0.5.0` 去敲门，很多站点会直接拒掉，而我们的目的只是取一份
    /// 公开的代理列表。
    pub fn new(config: &Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(crate::useragent::random())
            .timeout(config.refresh.timeout)
            .build()?;
        Ok(Self {
            subscribers: config.subscribers.clone(),
            timeout: config.refresh.timeout,
            client,
            egress: None,
        })
    }

    /// 挂上代理池：`via: pool` / `via: fallback` 的订阅源因此能借健康代理出门。
    pub fn with_egress(mut self, pool: Arc<ProxyPool>, options: SelectionOptions) -> Self {
        self.egress = Some(Arc::new(Egress {
            pool,
            options,
            timeout: self.timeout,
            clients: Mutex::new(HashMap::new()),
        }));
        self
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
    /// 的那个一起返回。
    pub async fn fetch_all_reporting(&self, progress: &dyn Progress) -> Vec<FetchOutcome> {
        self.fetch_all_streaming(progress, |_| {}).await
    }

    /// 与 [`SubscriberSet::fetch_all_reporting`] 相同，但每个订阅源一完成就
    /// 调一次 `on_finished`。
    ///
    /// 调用方用它做**增量落盘**：一轮全量刷新可能跑几十秒，中途被 Ctrl-C
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

    /// 运行单个订阅源的脚本，并把任何失败都转换为 `FetchOutcome::error`。
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
        });

        let outcome = self.fetch_one_inner(subscriber, progress).await;

        progress.fetch(FetchEvent::Finished(&outcome));
        outcome
    }

    /// 真正的拉取：跑脚本、归一化、截断。
    async fn fetch_one_inner(
        &self,
        subscriber: &SubscriberConfig,
        _progress: &dyn Progress,
    ) -> FetchOutcome {
        let started = Instant::now();
        let mut outcome = FetchOutcome {
            name: subscriber.name().to_string(),
            proxies: Vec::new(),
            rejected: Vec::new(),
            skipped: 0,
            truncated: 0,
            duration: Duration::ZERO,
            error: None,
        };

        let result = match self.run_subscriber(subscriber).await {
            Ok(result) => result,
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

        outcome.skipped = result.skipped;
        outcome.rejected = result.rejected;
        for candidate in result.candidates {
            match model::normalize(&candidate) {
                Ok(url) => outcome.proxies.push(url),
                Err(error) => outcome.rejected.push(error.to_string()),
            }
        }

        // Keep the cap last, so it applies to usable proxies rather than to
        // whatever the source happened to list first.
        outcome.truncated = apply_limit(&mut outcome.proxies, effective_limit(subscriber));

        outcome.duration = started.elapsed();
        outcome
    }

    /// 运行订阅源的脚本，把返回值变成候选代理字符串。
    async fn run_subscriber(&self, subscriber: &SubscriberConfig) -> Result<ParsedResult> {
        let code = match subscriber.source() {
            SubscriberSource::Inline(code) => code.to_string(),
            SubscriberSource::File(path) => {
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| Error::Subscriber {
                        name: subscriber.name().to_string(),
                        message: format!("cannot read {}: {e}", path.display()),
                    })?
            }
        };

        run_lua(
            subscriber.name(),
            &code,
            &subscriber.params,
            subscriber.timeout.unwrap_or(self.timeout),
            &self.client,
            self.egress.clone(),
            subscriber.via,
        )
        .await
    }
}

/// Lua 沙箱里每执行这么多条 VM 指令调一次钩子，用来检查脚本是否超时。
///
/// 只有钩子能掐断 `while true do end`：`tokio::time::timeout` 需要脚本先
/// 让出执行权，而纯计算的死循环永远不会让出。
const LUA_HOOK_INTERVAL: u32 = 10_000;

/// 写进日志的单条消息最长保留多少字节，超出部分截断。
const LUA_MAX_ARG: usize = 4 * 1024;

/// 运行一段 Lua 订阅源脚本，把它返回的代理表翻译成候选代理字符串。
///
/// 脚本可用的全局变量与函数见本模块的文档。这里是**一次性**沙箱：每次刷新
/// 都新建一个 Lua 状态，脚本之间不共享任何东西——一个来源的脚本改坏了全
/// 局变量，不会影响另一个来源。
async fn run_lua(
    name: &str,
    code: &str,
    params: &BTreeMap<String, serde_yaml::Value>,
    timeout: Duration,
    client: &reqwest::Client,
    egress: Option<Arc<Egress>>,
    via: EgressPolicy,
) -> Result<ParsedResult> {
    let fail = |message: String| Error::Subscriber {
        name: name.to_string(),
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

    // `fetch` / `fetch_json`：脚本访问外部世界的唯一方式，共用订阅源的
    // HTTP 客户端（同一份浏览器 UA、连接池与 TLS 配置）。
    //
    // 出口策略见 [`EgressPolicy`]：`pool` 先借代理、`fallback` 直连失败后再借。
    // `via_proxy` 记着"这一轮已经改用代理了"，所以一次回退之后剩下的请求不用
    // 再白试一次直连——抓一个被 WAF 拦住的站点时，这一条能省掉一半请求。
    let via_proxy = Arc::new(AtomicBool::new(false));

    let fetch_client = client.clone();
    let fetch_state = (egress.clone(), Arc::clone(&via_proxy), name.to_string());
    let fetch = lua
        .create_async_function(move |_, (url, headers): (String, Option<Table>)| {
            let client = fetch_client.clone();
            let (egress, via_proxy, label) = fetch_state.clone();
            async move {
                let headers = read_headers(headers)?;
                let egress = egress.as_deref();
                fetch_body(
                    &client, egress, &via_proxy, &label, via, &url, &headers, timeout,
                )
                .await
                .map_err(mlua::Error::runtime)
            }
        })
        .map_err(|e| fail(format!("cannot define `fetch` for Lua: {e}")))?;
    lua.globals()
        .set("fetch", fetch)
        .map_err(|e| fail(format!("cannot define `fetch` for Lua: {e}")))?;

    let fetch_json_client = client.clone();
    let fetch_json_state = (egress, Arc::clone(&via_proxy), name.to_string());
    let fetch_json = lua
        .create_async_function(move |lua, (url, headers): (String, Option<Table>)| {
            let client = fetch_json_client.clone();
            let (egress, via_proxy, label) = fetch_json_state.clone();
            async move {
                let headers = read_headers(headers)?;
                let body = fetch_body(
                    &client,
                    egress.as_deref(),
                    &via_proxy,
                    &label,
                    via,
                    &url,
                    &headers,
                    timeout,
                )
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

    // `sleep(秒)`：给"对同一个站点连着发几十个请求"的脚本一个礼貌的间隔。
    // 沙箱里没有 `os`/`io`，但抓一个分页站点确实需要它——少了它，脚本只能靠
    // 硬扛对方的限流。整段脚本的 `timeout` 仍然兜底，所以它跑不远。
    let sleep = lua
        .create_async_function(|_, seconds: f64| async move {
            if !seconds.is_finite() || seconds <= 0.0 {
                return Ok(());
            }
            // 上限就是脚本自己的 timeout（调用方会掐），这里只防止荒谬的数值。
            let seconds = seconds.min(3600.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Ok(())
        })
        .map_err(|e| fail(format!("cannot define `sleep` for Lua: {e}")))?;
    lua.globals()
        .set("sleep", sleep)
        .map_err(|e| fail(format!("cannot define `sleep` for Lua: {e}")))?;

    // `log`（以及它的别名 `print`）只写日志：脚本的**返回值**才是代理，
    // 这样 `print` 不会被误当成输出通道，也不会把杂音打到进程 stdout 上。
    let log_name = name.to_string();
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
    for alias in ["log", "print"] {
        lua.globals()
            .set(alias, log.clone())
            .map_err(|e| fail(format!("cannot define `{alias}` for Lua: {e}")))?;
    }

    // 指令钩子：纯计算的死循环只有它能掐断。
    //
    // 必须用 `set_global_hook`：`eval_async` 会把脚本放进一条新协程执行，
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
    let returned = match tokio::time::timeout(timeout, chunk.eval_async::<LuaValue>()).await {
        Err(_) => return Err(fail(expired)),
        Ok(Err(error)) => {
            return if Instant::now() >= deadline {
                Err(fail(expired))
            } else {
                Err(fail(format!("Lua error: {error}")))
            };
        }
        Ok(Ok(value)) => value,
    };

    let returned: JsonValue = lua
        .from_value(returned)
        .map_err(|e| fail(format!("cannot read what the script returned: {e}")))?;

    Ok(entries_to_candidates(&returned))
}

/// 一次请求最多试几个不同的代理。
///
/// 免费代理池里能用的比例很低（实测 1~5%），只试一个等于没试；但这些尝试都
/// 只发生在直连已经失败、或者来源明确要求走代理的时候，所以不至于白烧请求。
const EGRESS_PROXY_ATTEMPTS: usize = 3;

/// 按出口策略排出"先试哪条路"：`None` 是直连，`Some(proxy)` 是借那个代理。
///
/// 纯函数，方便单独测顺序；`egress` 为空（池子空/没挂池子）时只会返回直连。
fn egress_attempts(
    via: EgressPolicy,
    egress: Option<&Egress>,
    already_proxied: bool,
) -> Vec<Option<Url>> {
    let mut attempts: Vec<Option<Url>> = Vec::new();
    let push_proxies = |attempts: &mut Vec<Option<Url>>| {
        let Some(egress) = egress else {
            return;
        };
        for _ in 0..EGRESS_PROXY_ATTEMPTS {
            let Some(proxy) = egress.pick() else {
                return;
            };
            // `select` 的轮换本来就会换一个，这里再兜一次底：同一个代理试两遍没意义。
            if !attempts
                .iter()
                .any(|attempt| attempt.as_ref() == Some(&proxy))
            {
                attempts.push(Some(proxy));
            }
        }
    };

    match via {
        EgressPolicy::Direct => attempts.push(None),
        // 站点按 IP 限流/封 IP 时用这个：先借代理，代理都不成才直连碰运气。
        EgressPolicy::Pool => {
            push_proxies(&mut attempts);
            attempts.push(None);
        }
        // 默认：直连优先；这一轮已经因为回退改用代理了就直接走代理。
        EgressPolicy::Fallback if already_proxied => {
            push_proxies(&mut attempts);
            attempts.push(None);
        }
        EgressPolicy::Fallback => {
            attempts.push(None);
            push_proxies(&mut attempts);
        }
    }

    attempts
}

/// 从 Lua 传进来的表里读出请求头。
fn read_headers(table: Option<Table>) -> mlua::Result<Vec<(String, String)>> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::new();
    for pair in table.pairs::<String, String>() {
        let (name, value) = pair?;
        headers.push((name, value));
    }
    Ok(headers)
}

/// 按出口策略取一段响应体：直连、借池子里的健康代理，或者两者按顺序试。
///
/// 一次回退成功之后会把 `via_proxy` 置上，同一轮脚本剩下的请求就直接走代理，
/// 不再对着已知会失败的直连路径白试一遍。
#[allow(clippy::too_many_arguments)]
async fn fetch_body(
    client: &reqwest::Client,
    egress: Option<&Egress>,
    via_proxy: &AtomicBool,
    label: &str,
    via: EgressPolicy,
    url: &str,
    headers: &[(String, String)],
    timeout: Duration,
) -> std::result::Result<String, String> {
    let already_proxied = via_proxy.load(Ordering::Relaxed);
    let attempts = egress_attempts(via, egress, already_proxied);

    let mut last_error = format!("{url}: no attempt was made");
    for attempt in attempts {
        let request_client = match &attempt {
            None => client.clone(),
            Some(proxy) => match egress.expect("a proxy implies an egress").client(proxy) {
                Ok(client) => client,
                Err(error) => {
                    last_error = format!("{url}: cannot use proxy {proxy}: {error}");
                    continue;
                }
            },
        };

        let mut request = request_client.get(url).timeout(timeout);
        if !headers.is_empty() {
            let mut map = reqwest::header::HeaderMap::new();
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| format!("invalid header name `{name}`: {e}"))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|e| format!("invalid header value for `{name}`: {e}"))?;
                map.insert(name, value);
            }
            request = request.headers(map);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                return response
                    .text()
                    .await
                    .map_err(|e| format!("{url}: {}", crate::error::describe_reqwest_error(&e)));
            }
            Ok(response) => {
                let status = response.status();
                if attempt.is_some() {
                    via_proxy.store(true, Ordering::Relaxed);
                }
                last_error = format!("{url}: HTTP {status}");
            }
            Err(error) => {
                if attempt.is_some() {
                    via_proxy.store(true, Ordering::Relaxed);
                }
                last_error = format!("{url}: {}", crate::error::describe_reqwest_error(&error));
            }
        }
    }

    tracing::debug!(subscriber = %label, "fetch failed: {last_error}");
    Err(last_error)
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

/// 脚本返回值的转换结果。
///
/// 和 [`FetchOutcome`] 的分工：这里只管把脚本给的东西翻译成候选代理字符串，
/// 归一化与计数归 [`FetchOutcome`]。
#[derive(Debug, Clone, Default)]
pub struct ParsedResult {
    /// 候选代理字符串，尚未归一化。
    pub candidates: Vec<String>,
    /// 因为协议不受支持（`socks4`、写错的名称）而故意丢掉的条目。
    pub skipped: usize,
    /// 结构不对、用不了的条目，附上原因。
    pub rejected: Vec<String>,
}

/// 把一个脚本返回值翻译成候选代理字符串。
///
/// 期待一个代理表组成的数组；单个代理表、以及 `nil`（脚本什么都没返回）
/// 也接受。每个代理表认这些字段：
///
/// | 字段 | 含义 |
/// | --- | --- |
/// | `type` | 协议：`http`/`https`/`ssl`、`socks5`/`socks5h`/`socks`；其它（如 `socks4`）会被跳过 |
/// | `ip` | 主机名或 IP，`host`/`hostname`/`server`/`address`/`addr` 也算 |
/// | `port` | 端口，数字或字符串；缺省时用协议的默认端口 |
/// | `auth` | 可选，`user:password`（也可以只写 `user`） |
///
/// 数组里直接写字符串也可以，那就按代理 URL 交给归一化处理。
fn entries_to_candidates(value: &JsonValue) -> ParsedResult {
    let mut parsed = ParsedResult::default();

    let entries: &[JsonValue] = match value {
        JsonValue::Array(items) => items,
        // 空表要单独认：Lua 的空 table 没有数组部分，serde 把它序列化成空**对象**，
        // 而空对象显然不是一个代理条目——那是"这轮什么都没拿到"。
        JsonValue::Object(map) if map.is_empty() => return parsed,
        JsonValue::Object(_) => std::slice::from_ref(value),
        JsonValue::Null => return parsed,
        other => {
            parsed
                .rejected
                .push(format!("expected a list of proxies, got `{other}`"));
            return parsed;
        }
    };

    for entry in entries {
        match entry_to_candidate(entry) {
            Ok(Some(candidate)) => parsed.candidates.push(candidate),
            Ok(None) => parsed.skipped += 1,
            Err(reason) => parsed.rejected.push(reason),
        }
    }

    parsed
}

/// 一个代理表（或一条代理字符串）转成候选字符串。
///
/// `Ok(None)` 表示这个条目协议不受支持、应当计入 skipped。
fn entry_to_candidate(entry: &JsonValue) -> std::result::Result<Option<String>, String> {
    let JsonValue::Object(map) = entry else {
        return match entry {
            JsonValue::String(text) => Ok(Some(text.clone())),
            other => Err(format!("expected a proxy table or string, got `{other}`")),
        };
    };

    let Some(scheme) = scheme_from_entry(map) else {
        return Ok(None);
    };

    let host = ["ip", "host", "hostname", "server", "address", "addr"]
        .iter()
        .find_map(|key| map.get(*key).and_then(value_as_text))
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "missing `ip`".to_string())?;

    let port = match map.get("port") {
        Some(value) => value_as_text(value)
            .and_then(|text| text.trim().parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("invalid `port` for `{host}`"))?,
        None => scheme.default_port(),
    };

    // IPv6 字面量要塞进方括号，否则拼出来的 URL 会被解析成端口。
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };

    let auth = map.get("auth").and_then(value_as_text).unwrap_or_default();
    let userinfo = if auth.is_empty() {
        String::new()
    } else {
        let (user, password) = match auth.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (auth.as_str(), None),
        };
        let mut userinfo = encode_userinfo(user);
        if let Some(password) = password {
            userinfo.push(':');
            userinfo.push_str(&encode_userinfo(password));
        }
        userinfo.push('@');
        userinfo
    };

    Ok(Some(format!(
        "{}://{userinfo}{host}:{port}",
        scheme.as_str()
    )))
}

/// 从代理表里读出协议。
///
/// `http`/`https`/`ssl` 归一为 [`ProxyScheme::Http`]（列表里的 https 指
/// “这个代理能 CONNECT 到 HTTPS”，不是“对代理做 TLS”）；`socks` 一族统一
/// 成 [`ProxyScheme::Socks5h`]，由*代理*去解析域名——在 DNS 被污染的网络里，
/// 客户端自己解析会把伪造的地址交给代理。`socks4` 或无法识别的名称返回
/// `None`，调用方据此跳过该条目。
fn scheme_from_entry(map: &serde_json::Map<String, JsonValue>) -> Option<ProxyScheme> {
    let mut named: Vec<String> = Vec::new();
    for key in ["type", "scheme", "protocol", "protocols", "proxy_type"] {
        if let Some(value) = map.get(key) {
            collect_scheme_names(value, &mut named);
        }
    }

    // 什么都没说：按惯例当成 HTTP 代理。
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

/// JSON 标量转成文本：数字和字符串都算，其它不算。
fn value_as_text(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Number(number) => Some(number.to_string()),
        _ => None,
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

    /// 构造一个订阅源脚本条目，超时 5 秒、没有额外参数。
    fn lua_subscriber(code: &str) -> SubscriberConfig {
        SubscriberConfig {
            name: "lua".into(),
            lua_code: Some(code.into()),
            lua_file: None,
            timeout: Some(Duration::from_secs(5)),
            limit: None,
            enabled: true,
            via: EgressPolicy::default(),
            params: BTreeMap::new(),
        }
    }

    /// 用一个只含该订阅源的配置跑一次。
    async fn run_one(subscriber: SubscriberConfig) -> FetchOutcome {
        let config = Config {
            subscribers: vec![subscriber],
            ..Config::default()
        };
        let mut outcomes = SubscriberSet::new(&config).unwrap().fetch_all().await;
        outcomes.remove(0)
    }

    /// 跑一段脚本并返回规范渲染后的代理。
    async fn run_script(code: &str) -> Vec<String> {
        let outcome = run_one(lua_subscriber(code)).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        rendered(&outcome)
    }

    /// 把归一化后的代理渲染成规范的 `scheme://host:port` 形式，便于比较。
    fn rendered(outcome: &FetchOutcome) -> Vec<String> {
        outcome
            .proxies
            .iter()
            .map(|proxy| model::render_url(proxy, true))
            .collect()
    }

    fn json(text: &str) -> JsonValue {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn proxy_tables_become_candidate_urls() {
        let parsed = entries_to_candidates(&json(
            r#"[
                {"type": "http", "ip": "1.2.3.4", "port": 8080},
                {"type": "https", "host": "5.6.7.8", "port": "3128", "auth": "u:p"},
                {"type": "socks5", "ip": "9.9.9.9", "port": 1080},
                {"type": "socks", "ip": "9.9.9.8", "port": 1080},
                {"type": "socks4", "ip": "9.9.9.7", "port": 1080},
                {"type": "weird", "ip": "9.9.9.6", "port": 1080},
                {"type": "http", "ip": "2001:db8::1", "port": 8080},
                {"type": "http", "server": "10.0.0.1"},
                "1.1.1.1:1111"
            ]"#,
        ));

        assert_eq!(
            parsed.candidates,
            vec![
                "http://1.2.3.4:8080".to_string(),
                "http://u:p@5.6.7.8:3128".to_string(),
                "socks5h://9.9.9.9:1080".to_string(),
                // `socks` 也是 socks5h：让代理去解析域名。
                "socks5h://9.9.9.8:1080".to_string(),
                // IPv6 会被套上方括号。
                "http://[2001:db8::1]:8080".to_string(),
                // 端口缺省时用协议的默认端口。
                "http://10.0.0.1:80".to_string(),
                // 直接写字符串也收。
                "1.1.1.1:1111".to_string(),
            ]
        );
        assert_eq!(parsed.skipped, 2, "socks4 与未知协议该被跳过");
        assert!(parsed.rejected.is_empty());
    }

    #[test]
    fn entries_without_a_host_or_a_good_port_are_rejected() {
        let parsed = entries_to_candidates(&json(
            r#"[
                {"type": "http", "port": 8080},
                {"type": "http", "ip": "1.2.3.4", "port": "nope"},
                {"type": "http", "ip": "1.2.3.4", "port": 0},
                {"type": "http", "ip": "", "port": 1},
                42
            ]"#,
        ));
        assert!(parsed.candidates.is_empty());
        assert_eq!(parsed.rejected.len(), 5, "{:?}", parsed.rejected);
        assert!(parsed.rejected[0].contains("missing `ip`"));
        assert!(parsed.rejected[4].contains("proxy table or string"));
    }

    #[test]
    fn the_egress_order_follows_the_policy() {
        // 没有池子时，无论什么策略都只有直连一条路。
        for via in [
            EgressPolicy::Direct,
            EgressPolicy::Pool,
            EgressPolicy::Fallback,
        ] {
            let attempts = egress_attempts(via, None, false);
            assert_eq!(attempts, vec![None], "{via:?}");
        }
    }

    #[test]
    fn the_egress_order_prefers_the_pool_or_direct_as_configured() {
        // 池子里三条健康代理（借本地假上游指代），分别验证三种策略的顺序。
        let pool = Arc::new(ProxyPool::new());
        for port in [18081, 18082, 18083] {
            let (id, _) =
                pool.insert(crate::model::normalize(&format!("127.0.0.1:{port}")).unwrap());
            pool.apply_health_pass(
                &[(
                    id,
                    crate::pool::HealthUpdate {
                        alive: true,
                        latency: Some(Duration::from_millis(1)),
                        checked_at: SystemTime::now(),
                        probes: Vec::new(),
                    },
                )],
                &crate::pool::HealthPolicy::default(),
            );
        }
        let egress = Egress {
            pool,
            options: crate::selector::SelectionOptions::default(),
            timeout: Duration::from_secs(1),
            clients: Mutex::new(HashMap::new()),
        };

        // `direct` 只有一条直连。
        assert_eq!(
            egress_attempts(EgressPolicy::Direct, Some(&egress), false),
            vec![None]
        );

        // `pool` 先代理（最多三条）、再直连兜底。
        let attempts = egress_attempts(EgressPolicy::Pool, Some(&egress), false);
        assert_eq!(attempts.len(), 4, "{attempts:?}");
        assert!(attempts[0].is_some() && attempts[1].is_some() && attempts[2].is_some());
        assert_eq!(attempts[3], None, "{attempts:?}");
        // 每条代理都指向池子里的地址，而不是别处。
        for attempt in &attempts[..3] {
            let url = attempt.as_ref().expect("pooled attempt");
            assert!(url.host_str() == Some("127.0.0.1"), "{url}");
        }
        // 三个代理互不相同。
        let mut proxies: Vec<String> = attempts
            .iter()
            .filter_map(|attempt| attempt.as_ref().map(|url| url.to_string()))
            .collect();
        let before = proxies.len();
        proxies.dedup();
        assert_eq!(proxies.len(), before, "不该重复试同一个代理");

        // `fallback` 先直连，再借代理。
        let attempts = egress_attempts(EgressPolicy::Fallback, Some(&egress), false);
        assert_eq!(attempts[0], None);
        assert!(attempts[1].is_some());
        // 已经回退过：代理优先。
        let attempts = egress_attempts(EgressPolicy::Fallback, Some(&egress), true);
        assert!(attempts[0].is_some());
        assert_eq!(attempts.last(), Some(&None));
    }

    #[test]
    fn an_empty_result_is_empty_not_a_rejected_entry() {
        // `return {}` / 什么都没拿到时脚本返回空表，不该被算成"一条被拒绝的条目"。
        let parsed = entries_to_candidates(&json("{}"));
        assert!(parsed.candidates.is_empty());
        assert!(parsed.rejected.is_empty(), "{:?}", parsed.rejected);
    }

    #[test]
    fn a_single_proxy_table_or_nothing_is_fine() {
        // 一个代理表（没有包数组）。
        let parsed = entries_to_candidates(&json(r#"{"ip": "1.2.3.4", "port": 8080}"#));
        assert_eq!(parsed.candidates, vec!["http://1.2.3.4:8080".to_string()]);

        // 脚本什么都没返回。
        assert!(
            entries_to_candidates(&JsonValue::Null)
                .candidates
                .is_empty()
        );

        // 返回了别的东西：算一条被拒绝的条目，而不是把整个来源判死。
        let parsed = entries_to_candidates(&json("7"));
        assert!(parsed.candidates.is_empty());
        assert_eq!(parsed.rejected.len(), 1);
    }

    #[test]
    fn credentials_are_percent_encoded() {
        let parsed = entries_to_candidates(&json(
            r#"[{"ip": "1.2.3.4", "port": 8080, "auth": "u ser:p@ss"}]"#,
        ));
        assert_eq!(
            parsed.candidates,
            vec!["http://u%20ser:p%40ss@1.2.3.4:8080".to_string()]
        );
    }

    #[tokio::test]
    async fn a_script_returns_proxy_tables() {
        let rendered = run_script(
            r#"
            local result = {}
            for i = 1, 3 do
              table.insert(result, { type = "http", ip = "10.0.0." .. i, port = 8080 })
            end
            table.insert(result, { type = "socks5", ip = "1.2.3.4", port = 1080 })
            return result
            "#,
        )
        .await;

        assert_eq!(
            rendered,
            vec![
                "http://10.0.0.1:8080".to_string(),
                "http://10.0.0.2:8080".to_string(),
                "http://10.0.0.3:8080".to_string(),
                "socks5h://1.2.3.4:1080".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_script_that_returns_nothing_is_empty_not_broken() {
        let outcome = run_one(lua_subscriber("local _ = 1")).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.count(), 0);
    }

    #[tokio::test]
    async fn print_goes_to_the_log_not_to_the_pool() {
        // `print` 是 `log` 的别名：它不该被当成输出通道。
        let outcome = run_one(lua_subscriber(
            r#"
            print("11.11.11.11:1111")
            return { { type = "http", ip = "10.0.0.1", port = 8080 } }
            "#,
        ))
        .await;
        assert_eq!(rendered(&outcome), vec!["http://10.0.0.1:8080".to_string()]);
    }

    #[tokio::test]
    async fn a_script_sees_extra_config_keys_as_globals() {
        // 额外键变成全局变量：字符串、数字、以及整个表都能用。
        let mut subscriber = lua_subscriber(
            r#"
            return {
              { type = "http", ip = target_host, port = port },
              { type = "http", ip = "10.0.0.9", port = port, auth = token },
            }
            "#,
        );
        subscriber
            .params
            .insert("target_host".into(), serde_yaml::Value::from("10.1.2.3"));
        subscriber.params.insert(
            "port".into(),
            serde_yaml::Value::Number(serde_yaml::Number::from(8080)),
        );
        subscriber
            .params
            .insert("token".into(), serde_yaml::Value::from("u:p"));
        // `key:` 后面什么都不写：全局变量保持未定义，而不是拿到一个 null。
        subscriber
            .params
            .insert("unused".into(), serde_yaml::Value::Null);

        let outcome = run_one(subscriber).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(
            rendered(&outcome),
            vec![
                "http://10.1.2.3:8080".to_string(),
                "http://u:p@10.0.0.9:8080".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_lua_sandbox_has_no_file_or_process_access() {
        let outcome = run_one(lua_subscriber(
            r#"
            local missing = 0
            local names = {"io", "os", "package", "debug", "dofile", "loadfile", "load", "require"}
            for _, name in ipairs(names) do
              if _G[name] == nil then missing = missing + 1 end
            end
            return { { type = "http", ip = "10.0.0." .. missing, port = 8080 } }
            "#,
        ))
        .await;

        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(rendered(&outcome), vec!["http://10.0.0.8:8080".to_string()]);
    }

    #[tokio::test]
    async fn a_script_can_fetch_json_from_an_endpoint() {
        let app = axum::Router::new().route(
            "/list",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "data": {"proxies": ["10.9.8.7:3128", {"ip": "10.9.8.6", "port": 3128}]}
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
            local result = {{}}
            for _, item in ipairs(data.data.proxies) do
              if type(item) == "table" then
                table.insert(result, {{ type = "socks5", ip = item.ip, port = item.port }})
              else
                table.insert(result, item)
              end
            end
            return result
            "#,
        )))
        .await;
        server.abort();

        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(
            rendered(&outcome),
            vec![
                "http://10.9.8.7:3128".to_string(),
                "socks5h://10.9.8.6:3128".to_string()
            ]
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
        let outcome = run_one(lua_subscriber(
            r#"return { fetch("http://127.0.0.1:1/nope") }"#,
        ))
        .await;
        assert!(!outcome.ok());
        let error = outcome.error.unwrap();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }

    #[tokio::test]
    async fn a_runaway_lua_script_is_stopped() {
        let mut subscriber = lua_subscriber("while true do end");
        subscriber.timeout = Some(Duration::from_millis(300));

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
    async fn a_lua_limit_truncates_the_result() {
        let mut subscriber = lua_subscriber(
            r#"
            local result = {}
            for i = 1, 10 do
              table.insert(result, { type = "http", ip = "10.0.0." .. i, port = 8080 })
            end
            return result
            "#,
        );
        subscriber.limit = Some(3);

        let outcome = run_one(subscriber).await;
        assert!(outcome.ok(), "{:?}", outcome.error);
        assert_eq!(outcome.count(), 3);
        assert_eq!(outcome.truncated, 7);
    }

    #[tokio::test]
    async fn disabled_subscribers_are_skipped() {
        let mut subscriber = lua_subscriber("return { { ip = '1.2.3.4', port = 8080 } }");
        subscriber.enabled = false;

        let config = Config {
            subscribers: vec![subscriber],
            ..Config::default()
        };
        let set = SubscriberSet::new(&config).unwrap();
        assert!(set.is_empty());
        assert!(set.fetch_all().await.is_empty());
    }

    /// 记录进度事件的接收器。
    #[derive(Default)]
    struct Recorder {
        started: std::sync::Mutex<Vec<String>>,
        finished: std::sync::Mutex<Vec<String>>,
    }

    impl Progress for Recorder {
        fn fetch(&self, event: FetchEvent<'_>) {
            match event {
                FetchEvent::Started { name } => self.started.lock().unwrap().push(name.to_string()),
                FetchEvent::Finished(outcome) => self.finished.lock().unwrap().push(format!(
                    "{}={}",
                    outcome.name,
                    outcome.count()
                )),
            }
        }
    }

    #[tokio::test]
    async fn every_subscriber_is_reported_whether_it_works_or_not() {
        let config = Config {
            subscribers: vec![
                SubscriberConfig {
                    name: "good".into(),
                    ..lua_subscriber("return { { ip = '1.2.3.4', port = 8080 } }")
                },
                SubscriberConfig {
                    name: "bad".into(),
                    ..lua_subscriber("error('nope')")
                },
            ],
            ..Config::default()
        };

        let recorder = Recorder::default();
        let outcomes = SubscriberSet::new(&config)
            .unwrap()
            .fetch_all_reporting(&recorder)
            .await;
        assert_eq!(outcomes.len(), 2);

        let mut started = recorder.started.lock().unwrap().clone();
        started.sort();
        assert_eq!(started, vec!["bad".to_string(), "good".to_string()]);

        let mut finished = recorder.finished.lock().unwrap().clone();
        finished.sort();
        assert_eq!(finished, vec!["bad=0".to_string(), "good=1".to_string()]);
    }

    #[tokio::test]
    async fn limits_come_from_the_config() {
        let mut subscriber = lua_subscriber("return {}");
        assert_eq!(effective_limit(&subscriber), None);
        subscriber.limit = Some(25);
        assert_eq!(effective_limit(&subscriber), Some(25));
        // `0` 表示不限。
        subscriber.limit = Some(0);
        assert_eq!(effective_limit(&subscriber), None);
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
        assert_eq!(proxies[3].host_str(), Some("10.0.0.4"));
    }

    #[test]
    fn long_text_is_truncated_on_a_character_boundary() {
        assert_eq!(truncate_text("hello".to_string(), 16), "hello");
        assert_eq!(truncate_text("hello".to_string(), 3), "hel…");
        // 多字节字符不会被切成半个：4 落在「代」中间，只能退到 3。
        let chinese = "代理池".to_string();
        assert_eq!(truncate_text(chinese.clone(), 6), "代理…");
        assert_eq!(truncate_text(chinese, 4), "代…");
    }
}
