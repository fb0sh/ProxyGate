//! 服务端：把网关、REST API 与后台循环跑起来。
//!
//! 这就是整个程序：没有子命令、没有参数。配置来自 `config.yaml`
//! （`$PROXYGATE_CONFIG` 或默认查找路径），其余一切都通过 HTTP 操作——
//! 包括文档本身：`GET /help` 返回的就是本 crate 里的 `SKILL.md`。
//!
//! 启停顺序是有意的：先只读本地缓存把端口挂上（毫秒级），抓取与首次探测
//! 放到后台；在那之前 REST API 返回 `503` 并推动后台立刻重试，而不是假装
//! 池子是空的。

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::api::{self, ApiState};
use crate::app::{
    App, Bootstrap, Freshness, LogProgress, health_loop, refresh_loop, wait_for_shutdown,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::gateway::{Gateway, GatewayOptions};

/// 加载配置、绑定端口、启动后台循环，直到收到 Ctrl-C。
///
/// `config_path` 为 `None` 时按
/// [`Config::load`](crate::config::Config::load) 的顺序查找配置文件。
pub async fn run(config_path: Option<std::path::PathBuf>) -> Result<()> {
    let (config, path) = Config::load(config_path.as_deref())?;
    let credentials = config.gateway_credentials()?;
    let listen = config.server.listen.clone();

    let bootstrap = Bootstrap {
        refresh: Freshness::IfStale,
        check: Freshness::IfStale,
    };
    let app = Arc::new(App::new_with_progress(config, path, Arc::new(LogProgress))?);

    // 网关与 REST API 共用这一个端口。
    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|error| Error::Other(format!("cannot bind {listen}: {error}")))?;

    let gateway = Arc::new(Gateway::new(
        app.pool.clone(),
        app.clients.clone(),
        GatewayOptions {
            strategy: app.config.selection.strategy,
            reuse_after: app.config.selection.reuse_after,
            retries: app.config.gateway.retries,
            connect_timeout: app.config.gateway.connect_timeout,
            max_failures: app.config.health.max_failures,
            credentials: credentials.clone(),
        },
    ));

    // API 搭在网关的监听上：两类请求按形状分流。
    let api_state = Arc::new(ApiState::new(app.clone()));
    let gateway = Arc::new((*gateway).clone().with_api(api::router(api_state.clone())));

    info!(
        listen = %listen,
        config = ?app.config_path,
        proxies = app.pool.len(),
        alive = app.pool.stats().alive,
        auth = credentials.is_some(),
        ready = app.readiness().is_ready(),
        "proxygate is listening"
    );
    info!(help = %format!("http://{listen}/help"), "API documentation");
    if !app.readiness().is_ready() {
        info!("the pool is still being initialized; the REST API answers 503 until it is ready");
    }
    info!(
        "the REST API shares the proxy port; `gateway.auth` covers proxy requests only, \
         so the API itself stays open — keep it on a trusted interface"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks: JoinSet<&'static str> = JoinSet::new();

    tasks.spawn({
        let gateway = gateway.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            if let Err(error) = gateway.serve(listener, wait_for_shutdown(shutdown)).await {
                error!(error = %error, "HTTP proxy gateway stopped");
            }
            "gateway"
        }
    });

    tasks.spawn({
        let app = app.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            refresh_loop(app, shutdown, bootstrap).await;
            "refresh"
        }
    });

    tasks.spawn({
        let app = app.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            health_loop(app, shutdown).await;
            "health"
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("shutdown requested"),
        finished = tasks.join_next() => {
            match finished {
                Some(Ok(name)) => warn!(task = name, "task stopped; shutting down"),
                Some(Err(error)) => warn!(error = %error, "task panicked; shutting down"),
                None => {}
            }
        }
    }

    let _ = shutdown_tx.send(true);
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        warn!("timed out waiting for background tasks to stop");
    }

    if let Err(error) = app.persist(true) {
        warn!(error = %error, "cannot persist rotation state on shutdown");
    }

    Ok(())
}
