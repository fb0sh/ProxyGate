//! REST API：整个程序的对外面。
//!
//! 没有命令行客户端，所以这里覆盖全部操作：
//!
//! ```text
//! GET  /help                  面向人与 agent 的手册（就是 SKILL.md）
//! GET  /                      端点索引
//! GET  /api/v1/get            一个代理（纯文本，或 ?format=json）
//! GET  /api/v1/getua          一个内置 User-Agent（纯文本，或 ?format=json）
//! GET  /api/v1/proxies        整个代理池，凭据已脱敏
//! GET  /api/v1/health         存活状态、就绪状态与代理池计数
//! GET  /api/v1/providers      内置代理来源清单
//! POST /api/v1/refresh        让后台立刻抓取一轮订阅源
//! POST /api/v1/check          让后台立刻探测一轮代理池
//! ```
//!
//! 只有 `?format=json` 或 `/help` 之外的写操作才会返回 JSON；`/api/v1/get`
//! 默认只回一行 `host:port`。响应里的凭据一律脱敏（替换成 `***:***`）。
//! 网关也可以把这个路由挂到代理端口上，二者按请求形状区分彼此。

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::{App, Readiness};
use crate::error::{Error, Result};
use crate::state;

/// 未就绪时 `/api/v1/get` 在 `Retry-After` 里给出的建议重试秒数。
pub const INITIALIZE_RETRY_SECONDS: u64 = 5;

/// API 路由背后的共享状态。
///
/// 它只持有 [`App`] 与启动时刻：处理器需要的池子、选择参数、状态存储与就绪
/// 状态都从 `app` 上读，不复制一份，免得和别处（后台循环、发放验证）漂开。
#[derive(Debug)]
pub struct ApiState {
    /// 运行时上下文：池子、选择参数、状态存储、就绪状态与发放验证都在里面。
    pub app: Arc<App>,
    /// 进程启动时刻，用于计算 `uptime_seconds`。
    pub started: Instant,
}

impl ApiState {
    /// 用运行时上下文构造共享状态。
    ///
    /// 代理池、状态存储与选择参数都取自 [`App`]；额外记住 `started`，用于
    /// `/api/v1/health` 里的 `uptime_seconds`。
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            started: Instant::now(),
        }
    }

    /// 冷启动状态：未就绪时 `/api/v1/get` 返回 `503` 而不是空代理。
    fn readiness(&self) -> &Arc<Readiness> {
        self.app.readiness()
    }
}

/// 构建 API 路由。
pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/help", get(help))
        .route("/api/v1/get", get(get_proxy))
        .route("/api/v1/getua", get(get_user_agent))
        .route("/api/v1/proxies", get(list_proxies))
        .route("/api/v1/health", get(health))
        .route("/api/v1/providers", get(list_providers))
        .route("/api/v1/refresh", post(trigger_refresh))
        .route("/api/v1/check", post(trigger_check))
        .with_state(state)
}

/// 提供 API 服务，直到 `shutdown` 完成。
pub async fn serve<S>(
    state: Arc<ApiState>,
    listener: tokio::net::TcpListener,
    shutdown: S,
) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    tracing::info!(address = ?listener.local_addr().ok(), "REST API listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| Error::Other(format!("api server failed: {error}")))
}

/// `/api/v1/get` 与 `/api/v1/getua` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct GetQuery {
    /// 可选的响应格式；取值为 `json` 时返回 JSON，否则返回纯文本。
    #[serde(default)]
    format: Option<String>,
}

/// `/api/v1/get?format=json` 的响应体。
#[derive(Debug, Serialize)]
struct GetResponse {
    /// 完整的代理 URL，含凭据。
    proxy: String,
    /// 最近一次健康检查测得的延迟，单位毫秒。
    latency_ms: Option<u64>,
    /// 该代理被发放时所处的轮次。
    round: u64,
}

/// `/api/v1/proxies` 中的一个代理条目。
#[derive(Debug, Serialize)]
struct ProxyEntry {
    /// 脱敏后的代理 URL。
    proxy: String,
    /// 状态，例如 `alive`、`dead`。
    status: &'static str,
    /// 最近一次健康检查的延迟，单位毫秒。
    latency_ms: Option<u64>,
    /// 连续失败次数。
    failures: u32,
    /// 该代理最后一次被发放时所处的代。
    generation: u64,
    /// 最后一次被发放的时间，RFC3339 格式。
    last_used_at: Option<String>,
    /// 最后一次健康检查的时间，RFC3339 格式。
    last_checked_at: Option<String>,
    /// `2/2` —— 探测目标中有多少应答，以及一共探测了多少个。
    targets: String,
    /// 逐个探测目标的结果。
    probes: Vec<ProbeEntry>,
}

/// 针对单个探测目标的探测结果。
#[derive(Debug, Serialize)]
struct ProbeEntry {
    /// 探测目标的 URL。
    target: String,
    /// 该目标是否可达。
    ok: bool,
    /// 该目标的延迟，单位毫秒。
    latency_ms: Option<u64>,
}

/// `/api/v1/health` 的响应体。
#[derive(Debug, Serialize)]
struct HealthResponse {
    /// 整体状态：`initializing`（冷启动还没做完）、`ok`、`degraded`
    /// （没有存活代理）或 `empty`（池为空）。
    status: &'static str,
    /// crate 版本号。
    version: &'static str,
    /// 已运行秒数。
    uptime_seconds: u64,
    /// 当前代。
    generation: u64,
    /// 当前选择器策略名。
    strategy: &'static str,
    /// 健康检查使用的探测目标。
    health_targets: Vec<String>,
    /// 健康检查的通过要求。
    health_require: &'static str,
    /// 冷启动是否已经完成；未完成时 `/api/v1/get` 返回 `503`。
    ready: bool,
    /// 此刻是否正在初始化（抓取订阅源或探测代理）。
    initializing: bool,
    /// 已经开始的初始化尝试次数。
    initialization_attempts: u64,
    /// 最近一次初始化的失败原因；从未失败或已成功时为 `null`。
    initialization_error: Option<String>,
    /// 代理池计数。
    proxies: HealthCounts,
}

/// 代理池的分类计数。
#[derive(Debug, Serialize)]
struct HealthCounts {
    /// 代理总数。
    total: usize,
    /// 存活代理数。
    alive: usize,
    /// 已失效代理数。
    dead: usize,
}

/// `GET /api/v1/get` —— 发放一个健康代理。
async fn get_proxy(State(state): State<Arc<ApiState>>, Query(query): Query<GetQuery>) -> Response {
    // 冷启动（第一次抓取 + 探测）还没做完时，池子里的内容不代表最终结果，
    // 与其回答"没有可用代理"，不如明确说"还没准备好"。同时请后台立刻再试
    // 一次，所以下一次请求通常就能拿到代理。
    if !state.readiness().is_ready() {
        state.readiness().request_init();
        let retry_after = INITIALIZE_RETRY_SECONDS.to_string();
        let error = state.readiness().error();
        let retry_hint = format!(
            "proxygate: still initializing the proxy pool; retry in {INITIALIZE_RETRY_SECONDS} seconds\n"
        );

        if query.format.as_deref() == Some("json") {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::RETRY_AFTER, retry_after.as_str()),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                Json(serde_json::json!({
                    "error": "initializing",
                    "message": "the proxy pool is still being initialized",
                    "retry_after_seconds": INITIALIZE_RETRY_SECONDS,
                    "attempts": state.readiness().attempts(),
                    "detail": error,
                })),
            )
                .into_response();
        }

        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::RETRY_AFTER, retry_after.as_str()),
                (header::CACHE_CONTROL, "no-store"),
            ],
            retry_hint,
        )
            .into_response();
    }

    // 与命令行时代同一条路径：判定够新就直接发，旧了先现探一次
    // （`selection.verify`），探不通就换下一个候选。
    let Some(selection) = state
        .app
        .select_for_handout(state.app.config.selection.strategy)
        .await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (
                    header::RETRY_AFTER,
                    INITIALIZE_RETRY_SECONDS.to_string().as_str(),
                ),
            ],
            "proxygate: no verified proxy available\n",
        )
            .into_response();
    };

    tracing::debug!(
        proxy = %selection.proxy.to_masked_string(),
        round = selection.round,
        reset_round = selection.reset_round,
        "api handed out a proxy"
    );

    if query.format.as_deref() == Some("json") {
        return Json(GetResponse {
            proxy: selection.proxy.to_full_string(),
            latency_ms: selection.proxy.latency_ms(),
            round: selection.round,
        })
        .into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("{}\n", selection.proxy.to_full_string()),
    )
        .into_response()
}

/// `GET /api/v1/getua` —— 从内置池里随机取一个 User-Agent。
///
/// 无状态且分布均匀：不做轮换，也不记忆
/// 之前的调用。
async fn get_user_agent(Query(query): Query<GetQuery>) -> Response {
    let user_agent = crate::useragent::random();

    if query.format.as_deref() == Some("json") {
        return Json(serde_json::json!({ "user_agent": user_agent })).into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("{user_agent}\n"),
    )
        .into_response()
}

/// `GET /api/v1/proxies` —— 返回整个代理池，凭据已脱敏。
async fn list_proxies(State(state): State<Arc<ApiState>>) -> Json<Vec<ProxyEntry>> {
    let mut proxies: Vec<ProxyEntry> = state
        .app
        .pool
        .snapshot()
        .into_iter()
        .map(|proxy| ProxyEntry {
            proxy: proxy.to_masked_string(),
            status: proxy.status(),
            latency_ms: proxy.latency_ms(),
            failures: proxy.failures,
            generation: proxy.generation,
            last_used_at: proxy.last_used_at.map(state::to_rfc3339),
            last_checked_at: proxy.last_checked_at.map(state::to_rfc3339),
            targets: proxy.targets_summary(),
            probes: proxy
                .probes
                .iter()
                .map(|probe| ProbeEntry {
                    target: probe.target.to_string(),
                    ok: probe.ok,
                    latency_ms: probe.latency.map(|latency| latency.as_millis() as u64),
                })
                .collect(),
        })
        .collect();

    proxies.sort_by(|a, b| {
        b.status
            .cmp(a.status)
            .then_with(|| {
                a.latency_ms
                    .unwrap_or(u64::MAX)
                    .cmp(&b.latency_ms.unwrap_or(u64::MAX))
            })
            .then_with(|| a.proxy.cmp(&b.proxy))
    });

    Json(proxies)
}

/// `GET /api/v1/health` —— 存活状态与代理池计数。
async fn health(State(state): State<Arc<ApiState>>) -> Json<HealthResponse> {
    let stats = state.app.pool.stats();
    // 未就绪优先：`empty` 会被误读成"配置里没有来源"，而这时其实是在初始化。
    let status = if !state.readiness().is_ready() {
        "initializing"
    } else if stats.total == 0 {
        "empty"
    } else if stats.alive == 0 {
        "degraded"
    } else {
        "ok"
    };

    Json(HealthResponse {
        status,
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.started.elapsed().as_secs(),
        generation: state.app.pool.generation(),
        strategy: state.app.config.selection.strategy.as_str(),
        health_targets: state.app.config.health.targets(),
        health_require: state.app.config.health.require.as_str(),
        ready: state.readiness().is_ready(),
        initializing: state.readiness().is_initializing(),
        initialization_attempts: state.readiness().attempts(),
        initialization_error: state.readiness().error(),
        proxies: HealthCounts {
            total: stats.total,
            alive: stats.alive,
            dead: stats.dead,
        },
    })
}

/// `GET /help` —— 手册本身。
///
/// 返回编译进二进制的 `SKILL.md`：agent 可以整份读下去，人类 `curl` 下来
/// 就是一份 markdown 文件。用 `text/markdown` 而不是 HTML，是为了不加一个
/// markdown 渲染依赖；浏览器里也能当纯文本读。
async fn help() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        crate::SKILL,
    )
        .into_response()
}

/// `/api/v1/providers` 的一个条目。
#[derive(Debug, Serialize)]
struct ProviderEntry {
    /// 目录里的 id，配置里用 `provider:` 引用它。
    name: &'static str,
    /// 端点（分页来源带 `{page}` 占位符）。
    url: &'static str,
    /// 响应内容的解析格式。
    format: &'static str,
    /// 分页范围，如 `1-10`；不分页时是 `-`。
    pages: String,
    /// 页数。
    page_count: u32,
    /// 每个来源最多保留多少条（`null` 表示不限）。
    limit: Option<usize>,
    /// 文档或落地页。
    homepage: &'static str,
    /// 使用注意事项。
    notes: &'static str,
}

/// `GET /api/v1/providers` —— 内置代理来源清单。
async fn list_providers() -> Json<Vec<ProviderEntry>> {
    let providers = crate::providers::ALL
        .iter()
        .map(|provider| ProviderEntry {
            name: provider.name,
            url: provider.url,
            format: provider.format.as_str(),
            pages: provider.pages_label(),
            page_count: provider.page_count(),
            limit: provider.limit,
            homepage: provider.homepage,
            notes: provider.notes,
        })
        .collect();
    Json(providers)
}

/// 触发类端点的响应体。
#[derive(Debug, Serialize)]
struct TriggerResponse {
    /// `accepted`：后台已经收到请求。
    status: &'static str,
    /// 人话说明。
    message: &'static str,
    /// 该去轮询哪个端点看结果。
    poll: &'static str,
}

/// `POST /api/v1/refresh` —— 让后台立刻抓取一轮订阅源。
///
/// 真正干活的是 `serve` 的刷新循环（`App::request_refresh` 把它叫醒），
/// 所以这里立刻返回 `202`，抓取进度与结果看日志，池子变化看
/// `/api/v1/health` 与 `/api/v1/proxies`。全量抓取可能要几分钟，让 HTTP
/// 请求干等那么久不是个好接口。
async fn trigger_refresh(State(state): State<Arc<ApiState>>) -> Response {
    state.app.request_refresh();
    (
        StatusCode::ACCEPTED,
        Json(TriggerResponse {
            status: "accepted",
            message: "a subscriber refresh will start now; watch the logs and /api/v1/health",
            poll: "/api/v1/health",
        }),
    )
        .into_response()
}

/// `POST /api/v1/check` —— 让后台立刻探测一轮代理池。
async fn trigger_check(State(state): State<Arc<ApiState>>) -> Response {
    state.app.request_check();
    (
        StatusCode::ACCEPTED,
        Json(TriggerResponse {
            status: "accepted",
            message: "a health check will start now; watch the logs and /api/v1/health",
            poll: "/api/v1/health",
        }),
    )
        .into_response()
}

/// `GET /` —— 一个极简索引，让端口自己说明用途。
async fn index() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "proxygate",
        "version": env!("CARGO_PKG_VERSION"),
        "help": "/help",
        "endpoints": {
            "help": "/help",
            "get": "/api/v1/get",
            "get_json": "/api/v1/get?format=json",
            "getua": "/api/v1/getua",
            "getua_json": "/api/v1/getua?format=json",
            "proxies": "/api/v1/proxies",
            "health": "/api/v1/health",
            "providers": "/api/v1/providers",
            "refresh": "POST /api/v1/refresh",
            "check": "POST /api/v1/check",
        },
        "http_proxy": "point your client at the gateway port (default 127.0.0.1:8080)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::normalize;
    use crate::pool::HealthUpdate;
    use axum::body::Body;
    use axum::http::Request;
    use std::time::{Duration, SystemTime};

    use crate::config::{Config, HealthRequirement};
    use tower::ServiceExt;

    /// 测试用的运行时上下文：缓存目录指向临时目录，避免碰到真实的
    /// `~/.cache`（沙箱里通常不可写）。
    fn test_app() -> Arc<App> {
        let mut config = Config::default();
        config.state.dir = Some(std::env::temp_dir().join("proxygate-api-test"));
        config.health.targets = Some(vec!["https://example.com/generate_204".to_string()]);
        config.health.require = HealthRequirement::All;
        Arc::new(App::new(config, None).expect("test app"))
    }

    /// 已就绪、池里有一个已判活代理的共享状态。
    fn test_state() -> Arc<ApiState> {
        let app = test_app();
        let (id, _) = app
            .pool
            .insert(normalize("http://user:pass@1.2.3.4:3128").unwrap());
        app.pool.update_health(&[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(82)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);
        app.readiness.mark_ready();
        Arc::new(ApiState::new(app))
    }

    /// 冷启动还没做完的共享状态。
    fn initializing_state() -> Arc<ApiState> {
        Arc::new(ApiState::new(test_app()))
    }

    /// 一个干净、独立的运行时上下文：缓存目录每次新建，避免上一次运行留下的
    /// `cache.json` 干扰（尤其是"池子里只该有测试塞进去的代理"这类断言）。
    ///
    /// `verify: false` 用来对比「发放验证」开关的作用；`max_age = 0` 让判定
    /// 永远算旧，于是必然现探。
    fn isolated_app(verify: bool) -> Arc<App> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let dir = std::env::temp_dir().join(format!(
            "proxygate-api-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let mut config = Config::default();
        config.state.dir = Some(dir);
        config.health.target = Some("http://example.test/".to_string());
        config.health.targets = None;
        config.health.timeout = Duration::from_millis(500);
        config.health.concurrency = 1;
        config.selection.verify = verify;
        config.selection.max_age = Duration::from_secs(0);
        Arc::new(App::new(config, None).expect("app"))
    }

    /// 只接受连接然后立刻关掉的"上游代理"：现探必然失败。
    async fn dead_proxy() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        address
    }

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn get_returns_plain_text_by_default() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_string(response).await.trim(),
            "http://user:pass@1.2.3.4:3128"
        );
    }

    #[tokio::test]
    async fn get_supports_json() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get?format=json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["proxy"], "http://user:pass@1.2.3.4:3128");
        assert_eq!(value["latency_ms"], 82);
    }

    #[tokio::test]
    async fn getua_returns_a_built_in_agent() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/getua")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_string(response).await;
        let agent = body.trim();
        assert!(
            crate::useragent::all().contains(&agent),
            "not from the built-in pool: {agent}"
        );

        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/getua?format=json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        let agent = value["user_agent"].as_str().expect("user_agent field");
        assert!(crate::useragent::all().contains(&agent));
    }

    #[tokio::test]
    async fn proxies_masks_credentials() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/proxies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_string(response).await;
        assert!(
            !body.contains("user:pass"),
            "credentials must not leak: {body}"
        );
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value[0]["proxy"], "http://***:***@1.2.3.4:3128");
        assert_eq!(value[0]["status"], "alive");
    }

    #[tokio::test]
    async fn health_reports_pool_counters() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_string(response).await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["proxies"]["alive"], 1);
        assert_eq!(value["strategy"], "random");
    }

    #[tokio::test]
    async fn empty_pool_answers_503() {
        let state = test_state_ready_with_empty_pool();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_string(response).await;
        assert!(
            !body.contains("initializing"),
            "an empty pool is not the same as a cold start: {body}"
        );
    }

    /// 已就绪但池子是空的：`/get` 应当说"没有可用代理"，而不是"还在初始化"。
    fn test_state_ready_with_empty_pool() -> Arc<ApiState> {
        let app = test_app();
        app.readiness.mark_ready();
        Arc::new(ApiState::new(app))
    }

    #[tokio::test]
    async fn a_cold_process_answers_503_with_retry_after() {
        let state = initializing_state();
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .expect("Retry-After"),
            INITIALIZE_RETRY_SECONDS.to_string().as_str()
        );
        let body = body_string(response).await;
        assert!(body.contains("initializing"), "{body}");

        // 同一个进程里，健康检查也会如实说明自己还没就绪。
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(value["ready"], false);
        assert_eq!(value["initializing"], false);
        assert_eq!(value["initialization_attempts"], 0);
        assert_eq!(
            value["status"], "initializing",
            "an empty pool during a cold start is not the same as an empty config"
        );
    }

    #[tokio::test]
    async fn a_cold_process_explains_itself_in_json() {
        let response = router(initializing_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get?format=json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(value["error"], "initializing");
        assert_eq!(value["retry_after_seconds"], INITIALIZE_RETRY_SECONDS);
    }

    #[tokio::test]
    async fn get_verifies_the_proxy_before_handing_it_out() {
        let address = dead_proxy().await;
        let app = isolated_app(true);
        // 缓存里它是"活的"，但现探会失败——这正是发放验证要拦住的场景。
        let (id, _) = app
            .pool
            .insert(normalize(&format!("http://{address}")).expect("proxy url"));
        app.pool.update_health(&[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(5)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);
        app.readiness.mark_ready();

        let response = router(Arc::new(ApiState::new(app)))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_string(response).await;
        assert!(body.contains("no verified proxy"), "{body}");
    }

    #[tokio::test]
    async fn verification_can_be_turned_off() {
        let address = dead_proxy().await;
        let app = isolated_app(false);
        let (id, _) = app
            .pool
            .insert(normalize(&format!("http://{address}")).expect("proxy url"));
        app.pool.update_health(&[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(5)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);
        app.readiness.mark_ready();

        let response = router(Arc::new(ApiState::new(app)))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/get")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // `selection.verify: false` 时行为和以前一样：判定说什么就发什么。
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            body_string(response).await.contains(&address.to_string()),
            "expected the cached proxy to be handed out"
        );
    }

    #[tokio::test]
    async fn help_serves_the_manual() {
        let response = router(initializing_state())
            .oneshot(Request::builder().uri("/help").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/markdown; charset=utf-8")
        );

        let body = body_string(response).await;
        assert!(body.starts_with("---\n"), "frontmatter missing");
        assert!(body.contains("/api/v1/get"), "endpoints missing");
    }

    #[tokio::test]
    async fn the_provider_catalog_is_downloadable() {
        let response = router(initializing_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let providers: serde_json::Value =
            serde_json::from_str(&body_string(response).await).unwrap();
        let rola = providers
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "rola-ip")
            .expect("rola-ip is in the catalog");
        assert_eq!(rola["pages"], "1-10");
        assert_eq!(rola["page_count"], 10);
    }

    #[tokio::test]
    async fn refresh_and_check_can_be_triggered() {
        for path in ["/api/v1/refresh", "/api/v1/check"] {
            let response = router(initializing_state())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::ACCEPTED, "{path}");
            let value: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(value["status"], "accepted", "{path}");
            assert_eq!(value["poll"], "/api/v1/health", "{path}");
        }
    }

    #[tokio::test]
    async fn a_ready_pool_reports_ready_in_health() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(value["ready"], true);
        assert!(value["initialization_error"].is_null());
    }
}
