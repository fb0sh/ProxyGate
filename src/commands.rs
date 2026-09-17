//! CLI 命令，每个子命令对应一个函数。
//!
//! 这些是 `proxygate get`、`list`、`refresh`、`check`、`serve` 等
//! 子命令背后的库入口，另有 `providers`、`genconfig`、`getua` 与
//! `skill`。它们是公开的，因此另一个 Rust 程序无需启动二进制就能
//! 驱动 ProxyGate。
//!
//! 多数函数返回 `Result<ExitCode>`：`--json` 一类的开关只改变输出
//! 格式，从不改变退出码。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::api::{self, ApiState};
use crate::app::{App, Bootstrap, Freshness, health_loop, refresh_loop, wait_for_shutdown};
use crate::cli::{
    CheckArgs, Cli, Command, GetArgs, GetUaArgs, ListArgs, OutputFormat, ProvidersArgs,
    RefreshArgs, ServeArgs,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::gateway::{Gateway, GatewayOptions};
use crate::model::Proxy;
use crate::state;

/// `proxygate providers` — 列出内置代理来源以及如何启用它们。
///
/// `--json` 时输出机器可读的数组；否则输出一张对齐的表格，外加一段可直接
/// 粘贴进配置的示例片段。
pub fn providers(args: ProvidersArgs) -> Result<ExitCode> {
    let providers = crate::providers::ALL;

    if args.json {
        let entries: Vec<serde_json::Value> = providers
            .iter()
            .map(|provider| {
                serde_json::json!({
                    "name": provider.name,
                    "url": provider.url,
                    "format": provider.format.as_str(),
                    "homepage": provider.homepage,
                    "notes": provider.notes,
                    "pages": provider.pages_label(),
                    "page_count": provider.page_count(),
                    "limit": provider.limit,
                })
            })
            .collect();
        print_stdout(&serde_json::to_string_pretty(&entries)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    let width = providers
        .iter()
        .map(|provider| provider.name.len())
        .max()
        .unwrap_or(4)
        .max("NAME".len());

    let mut output = String::new();
    output.push_str(&format!(
        "{:<width$}  {:<8}  {:<7}  {}\n",
        "NAME",
        "FORMAT",
        "PAGES",
        "ENDPOINT",
        width = width
    ));
    for provider in providers {
        output.push_str(&format!(
            "{:<width$}  {:<8}  {:<7}  {}\n",
            provider.name,
            provider.format.as_str(),
            provider.pages_label(),
            provider.url,
            width = width
        ));
        output.push_str(&format!(
            "{:<width$}  {:<8}  {:<7}  notes: {}\n",
            "",
            "",
            "",
            provider.notes,
            width = width
        ));
        output.push_str(&format!(
            "{:<width$}  {:<8}  {:<7}  docs:  {}\n",
            "",
            "",
            "",
            provider.homepage,
            width = width
        ));
    }
    output.push_str(&format!(
        "\nEnable one by name (proxygate genconfig already enables all of them):\n\n  \
         subscribers:\n    - name: {}\n      type: builtin\n      provider: {}\n",
        providers.first().map(|p| p.name).unwrap_or("provider"),
        providers.first().map(|p| p.name).unwrap_or("provider")
    ));
    print_stdout(output.trim_end())?;

    Ok(ExitCode::SUCCESS)
}

/// `proxygate skill` — 本工具面向 agent 的文档。
///
/// 把嵌入二进制的 `SKILL.md` 打印到 STDOUT。它刻意不写入磁盘：是否需要落盘、
/// 落到哪里，都由调用方决定。
pub fn skill() -> Result<ExitCode> {
    print_stdout(crate::SKILL.trim_end())?;
    Ok(ExitCode::SUCCESS)
}

/// `proxygate genconfig` — 把带注释的示例配置直接写到 STDOUT。
///
/// 内容嵌在二进制里，因此在没有源码检出目录的已安装版本上也能使用。重定向即可
/// 得到一份起点：`proxygate genconfig > config.yaml`。
pub fn genconfig() -> Result<ExitCode> {
    // `print_stdout` adds the trailing newline; the file already has one.
    print_stdout(crate::config::EXAMPLE_CONFIG.trim_end())?;
    Ok(ExitCode::SUCCESS)
}

/// `proxygate getua` — 一个随机 User-Agent。
///
/// 输出形状与 `get` 一致。
pub fn getua(args: GetUaArgs) -> Result<ExitCode> {
    let user_agent = crate::useragent::random();

    match args.format {
        OutputFormat::Text => print_stdout(user_agent)?,
        OutputFormat::Json => {
            let value = serde_json::json!({ "user_agent": user_agent });
            print_stdout(&serde_json::to_string(&value)?)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// `proxygate get` — 挑选并打印一个可用代理。
///
/// 成功时向 STDOUT 精确写入一行：`Text` 格式下就是代理 URL
/// （`host:port`），JSON 格式下是包含代理、延迟与轮次的 JSON 对象。
/// 没有可用代理时返回 [`Error::NoProxy`]，对应退出码 `3`。
pub async fn get(config_path: Option<PathBuf>, args: GetArgs) -> Result<ExitCode> {
    let (config, path) = Config::load(config_path.as_deref())?;
    let strategy = args.strategy.unwrap_or(config.selection.strategy);

    let app = App::bootstrap(
        config,
        path,
        Bootstrap {
            refresh: if args.no_refresh {
                Freshness::Never
            } else {
                Freshness::IfStale
            },
            check: if args.no_check {
                Freshness::Never
            } else {
                Freshness::IfStale
            },
        },
    )
    .await?;

    let Some(selection) = app.select(strategy) else {
        return Err(Error::NoProxy(app.empty_reason()));
    };

    match args.format {
        OutputFormat::Text => {
            let text = if args.mask {
                selection.proxy.to_masked_string()
            } else {
                selection.proxy.to_full_string()
            };
            print_stdout(&text)?;
        }
        OutputFormat::Json => {
            let value = serde_json::json!({
                "proxy": selection.proxy.to_full_string(),
                "latency_ms": selection.proxy.latency_ms(),
                "round": selection.round,
            });
            print_stdout(&serde_json::to_string(&value)?)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// `proxygate list` — 列出代理池中的代理。
///
/// `--alive` 只显示健康代理，`--json` 输出与 REST API 同形的机器
/// 可读结果；两者都只影响输出格式，退出码始终为成功。
pub async fn list(config_path: Option<PathBuf>, args: ListArgs) -> Result<ExitCode> {
    let (config, path) = Config::load(config_path.as_deref())?;
    let app = App::bootstrap(
        config,
        path,
        Bootstrap {
            refresh: if args.no_refresh {
                Freshness::Never
            } else {
                Freshness::IfStale
            },
            check: if args.no_check {
                Freshness::Never
            } else {
                Freshness::IfStale
            },
        },
    )
    .await?;

    let mut proxies = app.pool.snapshot();
    if args.alive {
        proxies.retain(|proxy| proxy.alive);
    }
    sort_for_display(&mut proxies);

    if args.json {
        let entries: Vec<serde_json::Value> = proxies
            .iter()
            .map(|proxy| {
                serde_json::json!({
                    "proxy": proxy.render(args.show_auth),
                    "status": proxy.status(),
                    "latency_ms": proxy.latency_ms(),
                    "failures": proxy.failures,
                    "generation": proxy.generation,
                    "last_used_at": proxy.last_used_at.map(state::to_rfc3339),
                    "last_checked_at": proxy.last_checked_at.map(state::to_rfc3339),
                    // Same shape as `/api/v1/proxies`: which health targets this
                    // proxy reached, so `list --json` is enough to triage a pool.
                    "targets": proxy.targets_summary(),
                    "probes": proxy
                        .probes
                        .iter()
                        .map(|probe| serde_json::json!({
                            "target": probe.target.as_ref(),
                            "ok": probe.ok,
                            "latency_ms": probe.latency.map(|d| d.as_millis() as u64),
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        print_stdout(&serde_json::to_string_pretty(&entries)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    if proxies.is_empty() {
        eprintln!("proxygate: no proxies in the pool (run `proxygate refresh -v`)");
        return Ok(ExitCode::SUCCESS);
    }

    // `targets` is `passed/total` over the configured health targets. Under the
    // default `require: any` a `1/2` proxy is still handed out, so the column is
    // how you tell "reaches everything" from "reaches something".
    let rows: Vec<(String, &'static str, String, String)> = proxies
        .iter()
        .map(|proxy| {
            (
                proxy.render(args.show_auth),
                proxy.status(),
                proxy.targets_summary(),
                proxy
                    .latency_ms()
                    .map(|ms| format!("{ms}ms"))
                    .unwrap_or_else(|| "-".to_string()),
            )
        })
        .collect();

    let width = rows
        .iter()
        .map(|row| row.0.chars().count())
        .max()
        .unwrap_or(0)
        .max("PROXY".len());
    let targets_width = rows
        .iter()
        .map(|row| row.2.chars().count())
        .max()
        .unwrap_or(0)
        .max("TARGETS".len());

    let mut output = String::new();
    output.push_str(&format!(
        "{:<width$}  {:<7}  {:<targets_width$}  {}\n",
        "PROXY",
        "STATUS",
        "TARGETS",
        "LATENCY",
        width = width,
        targets_width = targets_width
    ));
    for (proxy, status, targets, latency) in &rows {
        output.push_str(&format!(
            "{:<width$}  {:<7}  {:<targets_width$}  {}\n",
            proxy,
            status,
            targets,
            latency,
            width = width,
            targets_width = targets_width
        ));
    }
    print_stdout(output.trim_end())?;

    Ok(ExitCode::SUCCESS)
}

/// `proxygate refresh` — 抓取全部订阅源并重建代理池。
///
/// 打印本次刷新的统计结果；单个订阅源失败只会计入统计，不会变成错误返回。
pub async fn refresh(config_path: Option<PathBuf>, args: RefreshArgs) -> Result<ExitCode> {
    let (config, path) = Config::load(config_path.as_deref())?;
    let app = App::bootstrap(
        config,
        path,
        Bootstrap {
            refresh: Freshness::Never,
            check: Freshness::Never,
        },
    )
    .await?;

    let summary = app.refresh().await?;

    if args.json {
        let value = serde_json::json!({
            "subscribers": summary.subscribers,
            "failed_subscribers": summary.failed,
            "fetched": summary.fetched,
            "added": summary.added,
            "existing": summary.existing,
            "removed": summary.removed,
            "rejected": summary.rejected,
            "truncated": summary.truncated,
            "pool_total": app.pool.len(),
            "duration_ms": summary.duration.as_millis() as u64,
        });
        print_stdout(&serde_json::to_string_pretty(&value)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    print_stdout(&format!(
        "subscribers   {} ({} failed)\nfetched       {}\nadded         {}\nexisting      {}\nremoved       {}\nrejected      {}\ntruncated     {}\npool total    {}\nduration      {:.2}s",
        summary.subscribers,
        summary.failed,
        summary.fetched,
        summary.added,
        summary.existing,
        summary.removed,
        summary.rejected,
        summary.truncated,
        app.pool.len(),
        summary.duration.as_secs_f64()
    ))?;

    Ok(ExitCode::SUCCESS)
}

/// `proxygate check` — 探测代理池中代理的健康状态。
///
/// `--concurrency` 覆盖本次运行的并发度，`--alive-only` 跳过已知
/// 失效的代理；文本模式下还会附带几条失败样例，而不是把成千上万个
/// id 全部列出。
pub async fn check(config_path: Option<PathBuf>, args: CheckArgs) -> Result<ExitCode> {
    let (mut config, path) = Config::load(config_path.as_deref())?;
    if let Some(concurrency) = args.concurrency {
        config.health.concurrency = concurrency;
    }

    let app = App::bootstrap(
        config,
        path,
        Bootstrap {
            refresh: Freshness::IfStale,
            check: Freshness::Never,
        },
    )
    .await?;

    let report = app.check(args.alive_only).await?;

    if args.json {
        let value = serde_json::json!({
            "checked": report.checked,
            "alive": report.alive,
            "dead": report.dead,
            "duration_ms": report.duration.as_millis() as u64,
            "targets": app.checker.targets(),
            "require": app.config.health.require.as_str(),
            "pool_total": app.pool.len(),
            "failures": report
                .results
                .iter()
                .filter(|result| !result.alive)
                .map(|result| serde_json::json!({
                    "id": result.id,
                    "error": result.error,
                    "targets": result
                        .targets
                        .iter()
                        .map(|target| serde_json::json!({
                            "target": target.target.as_ref(),
                            "ok": target.ok,
                            "latency_ms": target.latency_ms(),
                            "error": target.error,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        });
        print_stdout(&serde_json::to_string_pretty(&value)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    print_stdout(&format!(
        "checked   {}\nalive     {}\ndead      {}\ntargets   {}\nrequire   {}\nduration  {:.2}s",
        report.checked,
        report.alive,
        report.dead,
        app.checker.targets().join(" + "),
        app.config.health.require,
        report.duration.as_secs_f64()
    ))?;

    // A few examples are worth more than a list of ten thousand ids.
    for result in report.results.iter().filter(|result| !result.alive).take(5) {
        if let Some(proxy) = app.pool.get(&result.id) {
            eprintln!(
                "  dead {} [{}/{} targets] ({})",
                proxy.to_masked_string(),
                result.targets.iter().filter(|target| target.ok).count(),
                result.targets.len(),
                result.error.as_deref().unwrap_or("unknown error")
            );
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// `proxygate serve` — 运行 HTTP 代理网关与 REST API。
///
/// 它会一直运行，直到收到 Ctrl-C。
///
/// 命令行传入的监听地址与认证会覆盖配置。`--api same`（或 API 地址与
/// 代理地址相同）让两个角色共用一个监听端口。关闭时会等待后台任务退出，
/// 并把轮换状态强制落盘。
pub async fn serve(config_path: Option<PathBuf>, args: ServeArgs) -> Result<ExitCode> {
    let (mut config, path) = Config::load(config_path.as_deref())?;
    if let Some(listen) = &args.listen {
        config.server.proxy = listen.clone();
    }
    if let Some(api) = &args.api {
        config.server.api = api.clone();
    }
    if let Some(auth) = &args.auth {
        config.gateway.auth = Some(auth.clone());
    }
    // Re-validate: the flags above are user input too.
    config.normalize()?;

    let credentials = config.gateway_credentials()?;
    let proxy_address = config.server.proxy.clone();
    let api_address = config.server.api.clone();

    // 只读本地状态：抓取订阅源与探测代理放到后台，端口立刻开始接受请求。
    // 未就绪期间 REST API 返回 503，并推动后台立刻重试。
    let bootstrap = Bootstrap {
        refresh: if args.no_refresh {
            Freshness::Never
        } else {
            Freshness::IfStale
        },
        check: Freshness::IfStale,
    };
    let app = Arc::new(App::new(config, path)?);

    let proxy_listener = TcpListener::bind(&proxy_address).await.map_err(|error| {
        Error::Other(format!(
            "cannot bind the HTTP proxy on {proxy_address}: {error}"
        ))
    })?;
    // `api: same` (or an api address equal to the proxy one) puts both roles on
    // one listener: no client sends a proxy request and an API request at the
    // same time, so the two are distinguishable.
    let shared_port = app.config.server.shares_port();
    let api_listener = if shared_port {
        None
    } else {
        Some(TcpListener::bind(&api_address).await.map_err(|error| {
            Error::Other(format!(
                "cannot bind the REST API on {api_address}: {error}"
            ))
        })?)
    };

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

    let api_state = Arc::new(ApiState::new(app.clone()));

    // On a shared port the API rides along on the gateway listener.
    let gateway = if shared_port {
        Arc::new((*gateway).clone().with_api(api::router(api_state.clone())))
    } else {
        gateway
    };

    let api_label = if shared_port {
        proxy_address.clone()
    } else {
        api_address.clone()
    };
    info!(
        proxy = %proxy_address,
        api = %api_label,
        shared_port,
        config = ?app.config_path,
        proxies = app.pool.len(),
        alive = app.pool.stats().alive,
        auth = credentials.is_some(),
        ready = app.readiness().is_ready(),
        "proxygate is listening"
    );
    if !app.readiness().is_ready() {
        info!("the pool is still being initialized; the REST API answers 503 until it is ready");
    }
    if shared_port {
        info!(
            "the REST API shares the proxy port; `gateway.auth` covers proxy requests only, \
             so the API itself stays open — keep it on a trusted interface"
        );
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks: JoinSet<&'static str> = JoinSet::new();

    tasks.spawn({
        let gateway = gateway.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            if let Err(error) = gateway
                .serve(proxy_listener, wait_for_shutdown(shutdown))
                .await
            {
                error!(error = %error, "HTTP proxy gateway stopped");
            }
            "gateway"
        }
    });

    if let Some(api_listener) = api_listener {
        tasks.spawn({
            let state = api_state.clone();
            let shutdown = shutdown_rx.clone();
            async move {
                if let Err(error) =
                    api::serve(state, api_listener, wait_for_shutdown(shutdown)).await
                {
                    error!(error = %error, "REST API stopped");
                }
                "api"
            }
        });
    }

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

    Ok(ExitCode::SUCCESS)
}

/// `list` 的排序规则：先按存活，再按延迟，最后按 URL 稳定排序。
pub(crate) fn sort_for_display(proxies: &mut [Proxy]) {
    proxies.sort_by(|a, b| {
        b.alive
            .cmp(&a.alive)
            .then_with(|| {
                a.latency
                    .unwrap_or(Duration::MAX)
                    .cmp(&b.latency.unwrap_or(Duration::MAX))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// 往 STDOUT 写入一行；管道断开时返回错误而不是 panic。
pub(crate) fn print_stdout(text: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")
        .map_err(|error| Error::Other(format!("cannot write to stdout: {error}")))
}

/// 分发从命令行解析出的命令。
pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let Cli {
        command, config, ..
    } = cli;

    match command {
        Command::Get(args) => get(config, args).await,
        Command::List(args) => list(config, args).await,
        Command::Refresh(args) => refresh(config, args).await,
        Command::Check(args) => check(config, args).await,
        Command::Serve(args) => serve(config, args).await,
        Command::Genconfig => genconfig(),
        Command::Getua(args) => getua(args),
        Command::Skill => skill(),
        Command::Providers(args) => providers(args),
    }
}
