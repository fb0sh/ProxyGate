//! REST API.
//!
//! Three endpoints, exactly as much as the CLI needs to be usable from other
//! languages:
//!
//! ```text
//! GET /api/v1/get       one proxy (plain text, or ?format=json)
//! GET /api/v1/proxies   the pool, with credentials masked
//! GET /api/v1/health    liveness and pool counters
//! ```

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

/// How often a selection may rewrite `state.json`.
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

/// Shared state behind the API router.
#[derive(Debug)]
pub struct ApiState {
    pub pool: Arc<ProxyPool>,
    pub strategy: Strategy,
    pub reuse_after: Duration,
    pub store: Arc<StateStore>,
    pub started: Instant,
    /// Health check targets, reported by `/health` so operators can see them.
    pub health_targets: Vec<String>,
    /// Whether every target is required for a proxy to count as alive.
    pub health_require: HealthRequirement,
}

impl ApiState {
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

/// Builds the API router.
pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/v1/get", get(get_proxy))
        .route("/api/v1/getua", get(get_user_agent))
        .route("/api/v1/proxies", get(list_proxies))
        .route("/api/v1/health", get(health))
        .with_state(state)
}

/// Serves the API until `shutdown` resolves.
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

#[derive(Debug, Deserialize)]
pub struct GetQuery {
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Serialize)]
struct GetResponse {
    proxy: String,
    latency_ms: Option<u64>,
    round: u64,
}

#[derive(Debug, Serialize)]
struct ProxyEntry {
    proxy: String,
    status: &'static str,
    latency_ms: Option<u64>,
    failures: u32,
    generation: u64,
    last_used_at: Option<String>,
    last_checked_at: Option<String>,
    /// `2/2` — how many health targets answered, out of how many were probed.
    targets: String,
    probes: Vec<ProbeEntry>,
}

#[derive(Debug, Serialize)]
struct ProbeEntry {
    target: String,
    ok: bool,
    latency_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_seconds: u64,
    generation: u64,
    strategy: &'static str,
    health_targets: Vec<String>,
    health_require: &'static str,
    proxies: HealthCounts,
}

#[derive(Debug, Serialize)]
struct HealthCounts {
    total: usize,
    alive: usize,
    dead: usize,
}

/// `GET /api/v1/get` — hands out one healthy proxy.
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

/// `GET /api/v1/getua` — one random user agent from the built-in pool.
///
/// Stateless and uniform, exactly like `proxygate getua`: no rotation, no memory
/// of previous calls.
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

/// `GET /api/v1/proxies` — the whole pool, credentials masked.
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

/// `GET /api/v1/health` — liveness plus pool counters.
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

/// `GET /` — a tiny index so the port explains itself.
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
