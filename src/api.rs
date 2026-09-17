//! REST API。
//!
//! 路由覆盖 CLI 需要的最小端点集，让其他语言也能直接调用：
//!
//! ```text
//! GET /                 端点索引
//! GET /api/v1/get       一个代理（纯文本，或 ?format=json）
//! GET /api/v1/getua     一个内置 User-Agent（纯文本，或 ?format=json）
//! GET /api/v1/proxies   整个代理池，凭据已脱敏
//! GET /api/v1/health    存活状态与代理池计数
//! ```
//!
//! 只有 `GET /api/v1/get?format=json` 会返回 JSON；默认情况下该端点只回一行
//! `host:port`。响应里的凭据一律脱敏（替换成 `***:***`）。网关也可以把这个
//! 路由挂到代理端口上，二者能区分彼此的请求。

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::HealthRequirement;
use crate::error::{Error, Result};
use crate::pool::ProxyPool;
use crate::selector::Strategy;
use crate::state::{self, StateStore};

/// 相邻两次重写 `state.json` 之间的最小间隔。
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

/// API 路由背后的共享状态。
#[derive(Debug)]
pub struct ApiState {
    /// 用来挑选代理的代理池。
    pub pool: Arc<ProxyPool>,
    /// 选择代理时使用的选择器策略。
    pub strategy: Strategy,
    /// 同一代理在被再次选中前需要等待的时长。
    pub reuse_after: Duration,
    /// 轮换状态的状态存储。
    pub store: Arc<StateStore>,
    /// 进程启动时刻，用于计算 `uptime_seconds`。
    pub started: Instant,
    /// 健康检查的探测目标，由 `/health` 返回，便于运维查看。
    pub health_targets: Vec<String>,
    /// 是否要求所有探测目标都通过，代理才算存活。
    pub health_require: HealthRequirement,
}

impl ApiState {
    /// 用给定的代理池、状态存储和选择参数构造共享状态。
    pub fn new(
        pool: Arc<ProxyPool>,
        store: Arc<StateStore>,
        strategy: Strategy,
        reuse_after: Duration,
        health_targets: Vec<String>,
        health_require: HealthRequirement,
    ) -> Self {
        Self {
            pool,
            strategy,
            reuse_after,
            store,
            started: Instant::now(),
            health_targets,
            health_require,
        }
    }
}

/// 构建 API 路由。
pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/v1/get", get(get_proxy))
        .route("/api/v1/getua", get(get_user_agent))
        .route("/api/v1/proxies", get(list_proxies))
        .route("/api/v1/health", get(health))
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
    /// 整体状态：`ok`、`degraded`（没有存活代理）或 `empty`（池为空）。
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
    let now = SystemTime::now();
    let Some(selection) = state.pool.select(state.strategy, state.reuse_after, now) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "proxygate: no healthy proxy available\n",
        )
            .into_response();
    };

    if let Err(error) = state
        .store
        .persist_throttled(&state.pool, now, PERSIST_INTERVAL)
    {
        tracing::warn!(error = %error, "cannot persist state");
    }

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
/// 无状态且分布均匀，与 `proxygate getua` 完全一致：不做轮换，也不记忆
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
    let stats = state.pool.stats();
    let status = if stats.total == 0 {
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
        generation: state.pool.generation(),
        strategy: state.strategy.as_str(),
        health_targets: state.health_targets.clone(),
        health_require: state.health_require.as_str(),
        proxies: HealthCounts {
            total: stats.total,
            alive: stats.alive,
            dead: stats.dead,
        },
    })
}

/// `GET /` —— 一个极简索引，让端口自己说明用途。
async fn index() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "proxygate",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": {
            "get": "/api/v1/get",
            "get_json": "/api/v1/get?format=json",
            "getua": "/api/v1/getua",
            "getua_json": "/api/v1/getua?format=json",
            "proxies": "/api/v1/proxies",
            "health": "/api/v1/health",
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
    use tower::ServiceExt;

    fn test_state() -> Arc<ApiState> {
        let pool = Arc::new(ProxyPool::new());
        let (id, _) = pool.insert(normalize("http://user:pass@1.2.3.4:3128").unwrap());
        pool.update_health(&[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(82)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);
        let store = Arc::new(StateStore::new(
            std::env::temp_dir().join("proxygate-api-test"),
        ));
        Arc::new(ApiState::new(
            pool,
            store,
            Strategy::Random,
            Duration::from_secs(1800),
            vec!["https://example.com/generate_204".to_string()],
            HealthRequirement::All,
        ))
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
        let pool = Arc::new(ProxyPool::new());
        let state = Arc::new(ApiState::new(
            pool,
            Arc::new(StateStore::new(
                std::env::temp_dir().join("proxygate-api-empty"),
            )),
            Strategy::Random,
            Duration::from_secs(1800),
            vec!["https://example.com/generate_204".to_string()],
            HealthRequirement::All,
        ));
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
    }
}
