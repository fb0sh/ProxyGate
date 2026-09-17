//! The `proxygate` binary: command dispatch and the application runtime.
//!
//! Everything that `get`, `list`, `refresh`, `check` and `serve` share lives in
//! [`App`]: the pool, the on-disk state, the subscriber set and the checker.
//! `serve` simply runs four things on one Tokio runtime — subscriber refresh,
//! health checking, the REST API and the HTTP gateway.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use proxygate::api::{self, ApiState};
use proxygate::checker::{CheckReport, HealthChecker, ProxyClients};
use proxygate::cli::{
    CheckArgs, Cli, Command, GetArgs, GetUaArgs, ListArgs, OutputFormat, ProvidersArgs,
    RefreshArgs, ServeArgs,
};
use proxygate::config::Config;
use proxygate::error::{Error, Result};
use proxygate::gateway::{Gateway, GatewayOptions};
use proxygate::model::{self, ProbeOutcome, Proxy, ProxyId};
use proxygate::pool::{HealthRestore, ProxyPool, Selection};
use proxygate::selector::Strategy;
use proxygate::state::{self, CacheFile, HealthFile, StateStore, TargetHealthFile};
use proxygate::subscriber::SubscriberSet;

/// How often a selection may rewrite `state.json`.
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli);

    match run(cli).await {
        Ok(code) => code,
        Err(error) => {
            // stdin/stdout belong to the caller (`proxygate get` prints exactly
            // one line there), so diagnostics always go to stderr.
            eprintln!("proxygate: error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn init_tracing(cli: &Cli) {
    let default_level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let Cli {
        command, config, ..
    } = cli;

    match command {
        Command::Get(args) => cmd_get(config, args).await,
        Command::List(args) => cmd_list(config, args).await,
        Command::Refresh(args) => cmd_refresh(config, args).await,
        Command::Check(args) => cmd_check(config, args).await,
        Command::Serve(args) => cmd_serve(config, args).await,
        Command::Genconfig => cmd_genconfig(),
        Command::Getua(args) => cmd_getua(args),
        Command::Skill => cmd_skill(),
        Command::Providers(args) => cmd_providers(args),
    }
}

/// `proxygate providers` — what the built-in catalog contains.
///
/// Needs no config file: the point is to discover what you *could* subscribe to.
fn cmd_providers(args: ProvidersArgs) -> Result<ExitCode> {
    let providers = proxygate::providers::ALL;

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
        "{:<width$}  {:<10}  {}\n",
        "NAME",
        "FORMAT",
        "ENDPOINT",
        width = width
    ));
    for provider in providers {
        output.push_str(&format!(
            "{:<width$}  {:<10}  {}\n",
            provider.name,
            provider.format.as_str(),
            provider.url,
            width = width
        ));
        output.push_str(&format!(
            "{:<width$}  {:<10}  notes: {}\n",
            "",
            "",
            provider.notes,
            width = width
        ));
        output.push_str(&format!(
            "{:<width$}  {:<10}  docs:  {}\n",
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

/// `proxygate skill` — this tool's own documentation for an agent to read.
///
/// Prints `SKILL.md` (embedded in the binary) to stdout. It is deliberately not
/// written to disk: the caller decides where, if anywhere, it belongs.
fn cmd_skill() -> Result<ExitCode> {
    print_stdout(proxygate::SKILL.trim_end())?;
    Ok(ExitCode::SUCCESS)
}

/// `proxygate genconfig` — the annotated example, straight to stdout.
///
/// The content is embedded in the binary, so this works from an installed
/// build with no checkout next to it. Redirect it to get a starting point:
/// `proxygate genconfig > config.yaml`.
fn cmd_genconfig() -> Result<ExitCode> {
    // `print_stdout` adds the trailing newline; the file already has one.
    print_stdout(proxygate::config::EXAMPLE_CONFIG.trim_end())?;
    Ok(ExitCode::SUCCESS)
}

/// `proxygate getua` — one random user agent, same shape as `get`.
fn cmd_getua(args: GetUaArgs) -> Result<ExitCode> {
    let user_agent = proxygate::useragent::random();

    match args.format {
        OutputFormat::Text => print_stdout(user_agent)?,
        OutputFormat::Json => {
            let value = serde_json::json!({ "user_agent": user_agent });
            print_stdout(&serde_json::to_string(&value)?)?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Application runtime
// ---------------------------------------------------------------------------

/// How a cache should be used during bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// Use it when it is younger than the configured interval.
    IfStale,
    /// Ignore it and do the work.
    Force,
    /// Use it no matter how old it is.
    Never,
}

#[derive(Debug, Clone, Copy)]
struct Bootstrap {
    refresh: Freshness,
    check: Freshness,
}

#[derive(Debug, Clone)]
struct RefreshSummary {
    subscribers: usize,
    failed: usize,
    fetched: usize,
    added: usize,
    existing: usize,
    removed: usize,
    rejected: usize,
    duration: Duration,
}

/// Everything the commands share.
struct App {
    config: Config,
    config_path: Option<PathBuf>,
    pool: Arc<ProxyPool>,
    store: Arc<StateStore>,
    clients: Arc<ProxyClients>,
    checker: HealthChecker,
    subscribers: SubscriberSet,
    /// Unix seconds of the last successful subscriber fetch (0 = never).
    fetched_at: AtomicU64,
    /// Unix seconds of the last completed health pass (0 = never).
    checked_at: AtomicU64,
    /// Serializes refresh and check so `serve` cannot run two passes at once.
    busy: tokio::sync::Mutex<()>,
}

impl App {
    /// Loads config state, the caches, and performs the requested work.
    async fn bootstrap(
        config: Config,
        config_path: Option<PathBuf>,
        options: Bootstrap,
    ) -> Result<Self> {
        let store = Arc::new(StateStore::new(config.cache_dir()));
        store.ensure_dir()?;

        let clients = Arc::new(ProxyClients::new(
            config.health.timeout,
            config.gateway.connect_timeout,
        ));
        let checker = HealthChecker::new(&config.health, clients.clone());
        let subscribers = SubscriberSet::new(&config)?;
        let pool = Arc::new(ProxyPool::new());

        // 1. Proxies from the cache.
        let cache = store.load_cache();
        let now = SystemTime::now();
        let mut cached_urls = Vec::with_capacity(cache.proxies.len());
        for raw in &cache.proxies {
            match model::normalize(raw) {
                Ok(url) => cached_urls.push(url),
                Err(error) => debug!(entry = %raw, error = %error, "ignoring invalid cached proxy"),
            }
        }
        if !cached_urls.is_empty() {
            let merged = pool.merge(cached_urls);
            debug!(
                total = pool.len(),
                added = merged.added,
                "restored proxies from cache"
            );
        }

        // 2. Usage facts: generation and per-proxy last use.
        let restored = store.restore(&pool);
        if restored.restored > 0 || restored.missing > 0 {
            debug!(
                restored = restored.restored,
                missing = restored.missing,
                generation = restored.generation,
                "restored rotation state"
            );
        }

        // 3. Cached health, so an unchanged proxy keeps its last known status.
        let checked_at = cache.checked_at.as_deref().and_then(state::parse_rfc3339);
        if !cache.health.is_empty() {
            let entries: Vec<(ProxyId, HealthRestore)> = cache
                .health
                .iter()
                .map(|(id, entry)| {
                    (
                        id.clone(),
                        HealthRestore {
                            alive: entry.alive,
                            latency: entry.latency_ms.map(Duration::from_millis),
                            failures: entry.failures,
                            checked_at,
                            probes: entry
                                .targets
                                .iter()
                                .map(|probe| ProbeOutcome {
                                    target: Arc::from(probe.target.as_str()),
                                    ok: probe.ok,
                                    latency: probe.latency_ms.map(Duration::from_millis),
                                })
                                .collect(),
                        },
                    )
                })
                .collect();
            pool.restore_health(&entries);
        }

        let app = Self {
            fetched_at: AtomicU64::new(
                cache
                    .fetched_at
                    .as_deref()
                    .and_then(state::parse_rfc3339)
                    .map(|time| state::unix_secs(time).max(0) as u64)
                    .unwrap_or(0),
            ),
            checked_at: AtomicU64::new(
                checked_at
                    .map(|time| state::unix_secs(time).max(0) as u64)
                    .unwrap_or(0),
            ),
            config,
            config_path,
            pool,
            store,
            clients,
            checker,
            subscribers,
            busy: tokio::sync::Mutex::new(()),
        };

        if options.refresh == Freshness::Force
            || (options.refresh == Freshness::IfStale
                && !cache.proxies_fresh(app.config.refresh.interval, now))
        {
            let summary = app.refresh().await?;
            info!(
                fetched = summary.fetched,
                added = summary.added,
                failed_subscribers = summary.failed,
                "subscriber refresh complete"
            );
        }

        if options.check == Freshness::Force
            || (options.check == Freshness::IfStale
                && !cache.health_fresh(app.config.health.interval, now))
        {
            let report = app.check(false).await?;
            if report.checked > 0 {
                info!(report = %report.summary(), "initial health check complete");
            }
        }

        Ok(app)
    }

    /// Fetches every subscriber and merges the result into the pool.
    async fn refresh(&self) -> Result<RefreshSummary> {
        let _guard = self.busy.lock().await;
        let started = Instant::now();

        if self.subscribers.configured() == 0 {
            debug!("no subscribers configured; nothing to refresh");
        }

        let outcomes = self.subscribers.fetch_all().await;
        let all_ok = !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.ok());

        let mut urls = Vec::new();
        let mut fetched = 0;
        let mut failed = 0;
        let mut rejected = 0;
        for outcome in &outcomes {
            if outcome.ok() {
                fetched += outcome.count();
                rejected += outcome.rejected.len();
                debug!(
                    subscriber = %outcome.name,
                    found = outcome.count(),
                    rejected = outcome.rejected.len(),
                    skipped = outcome.skipped,
                    elapsed_ms = outcome.duration.as_millis() as u64,
                    "subscriber fetched"
                );
            } else {
                failed += 1;
                warn!(
                    subscriber = %outcome.name,
                    error = outcome.error.as_deref().unwrap_or("unknown error"),
                    "subscriber failed"
                );
            }
            urls.extend(outcome.proxies.iter().cloned());
        }

        let fresh_ids: HashSet<ProxyId> = urls.iter().map(Proxy::id_of).collect();
        let merged = self.pool.merge(urls);

        // Only a completely successful refresh may evict proxies: a subscriber
        // that failed could otherwise delete all of its proxies.
        let mut removed = 0;
        if all_ok {
            removed = self.pool.retain(&fresh_ids);
            if removed > 0 {
                info!(removed, "dropped proxies that are no longer offered");
            }
        }

        self.fetched_at.store(
            state::unix_secs(SystemTime::now()).max(0) as u64,
            Ordering::SeqCst,
        );
        self.save_cache();

        Ok(RefreshSummary {
            subscribers: outcomes.len(),
            failed,
            fetched,
            added: merged.added,
            existing: merged.existing,
            removed,
            rejected,
            duration: started.elapsed(),
        })
    }

    /// Probes every (or every alive) proxy and writes the result into the pool.
    async fn check(&self, alive_only: bool) -> Result<CheckReport> {
        let _guard = self.busy.lock().await;

        let mut proxies = self.pool.snapshot();
        if alive_only {
            proxies.retain(|proxy| proxy.alive);
        }
        if proxies.is_empty() {
            debug!("nothing to check");
            return Ok(CheckReport::default());
        }

        let report = self.checker.check_and_apply(&self.pool, &proxies).await;
        self.checked_at.store(
            state::unix_secs(SystemTime::now()).max(0) as u64,
            Ordering::SeqCst,
        );
        self.save_cache();
        Ok(report)
    }

    /// Picks a proxy and records the use.
    fn select(&self, strategy: Strategy) -> Option<Selection> {
        let selection = self.pool.select(
            strategy,
            self.config.selection.reuse_after,
            SystemTime::now(),
        )?;

        tracing::debug!(
            proxy = %selection.proxy.to_masked_string(),
            round = selection.round,
            reset_round = selection.reset_round,
            candidates = selection.candidates,
            "selected proxy"
        );

        if let Err(error) = self.persist(false) {
            warn!(error = %error, "cannot persist rotation state");
        }
        Some(selection)
    }

    /// Writes the rotation state, optionally bypassing the throttle.
    fn persist(&self, force: bool) -> Result<bool> {
        let now = SystemTime::now();
        if force {
            self.store.persist(&self.pool, now)?;
            Ok(true)
        } else {
            self.store
                .persist_throttled(&self.pool, now, PERSIST_INTERVAL)
        }
    }

    /// Writes the subscriber/health cache.
    ///
    /// Persistence is an optimisation, not correctness: a read-only cache
    /// directory degrades the cache to "never fresh" instead of taking the
    /// gateway down.
    fn save_cache(&self) {
        if let Err(error) = self.try_save_cache() {
            warn!(error = %error, "cannot write the cache; continuing without it");
        }
    }

    fn try_save_cache(&self) -> Result<()> {
        let proxies = self.pool.snapshot();
        let fetched = self.fetched_at.load(Ordering::SeqCst);
        let checked = self.checked_at.load(Ordering::SeqCst);

        let cache = CacheFile {
            fetched_at: (fetched > 0)
                .then(|| state::to_rfc3339(state::from_unix_secs(fetched as i64))),
            proxies: proxies.iter().map(Proxy::to_full_string).collect(),
            checked_at: (checked > 0)
                .then(|| state::to_rfc3339(state::from_unix_secs(checked as i64))),
            health: proxies
                .iter()
                .map(|proxy| {
                    (
                        proxy.id.clone(),
                        HealthFile {
                            alive: proxy.alive,
                            latency_ms: proxy.latency_ms(),
                            failures: proxy.failures,
                            targets: proxy
                                .probes
                                .iter()
                                .map(|probe| TargetHealthFile {
                                    target: probe.target.to_string(),
                                    ok: probe.ok,
                                    latency_ms: probe.latency.map(|d| d.as_millis() as u64),
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
        };
        self.store.save_cache(&cache)
    }

    /// Explains why nothing could be selected.
    fn empty_reason(&self) -> String {
        let stats = self.pool.stats();
        if stats.total == 0 {
            if self.subscribers.configured() == 0 {
                "the pool is empty and no subscribers are configured; add one to config.yaml and run `proxygate refresh`".to_string()
            } else {
                format!(
                    "the pool is empty: {} subscriber(s) produced no usable proxy (run `proxygate refresh -v` to see why)",
                    self.subscribers.configured()
                )
            }
        } else if stats.alive == 0 {
            format!(
                "{} proxy(ies) in the pool, none passed the health check against {} (require: {}; run `proxygate check` for the per-target reasons)",
                stats.total,
                self.config.health.targets().join(" + "),
                self.config.health.require
            )
        } else {
            format!(
                "no proxy could be selected ({} alive of {} total)",
                stats.alive, stats.total
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn cmd_get(config_path: Option<PathBuf>, args: GetArgs) -> Result<ExitCode> {
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

async fn cmd_list(config_path: Option<PathBuf>, args: ListArgs) -> Result<ExitCode> {
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

async fn cmd_refresh(config_path: Option<PathBuf>, args: RefreshArgs) -> Result<ExitCode> {
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
            "pool_total": app.pool.len(),
            "duration_ms": summary.duration.as_millis() as u64,
        });
        print_stdout(&serde_json::to_string_pretty(&value)?)?;
        return Ok(ExitCode::SUCCESS);
    }

    print_stdout(&format!(
        "subscribers   {} ({} failed)\nfetched       {}\nadded         {}\nexisting      {}\nremoved       {}\nrejected      {}\npool total    {}\nduration      {:.2}s",
        summary.subscribers,
        summary.failed,
        summary.fetched,
        summary.added,
        summary.existing,
        summary.removed,
        summary.rejected,
        app.pool.len(),
        summary.duration.as_secs_f64()
    ))?;

    Ok(ExitCode::SUCCESS)
}

async fn cmd_check(config_path: Option<PathBuf>, args: CheckArgs) -> Result<ExitCode> {
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

async fn cmd_serve(config_path: Option<PathBuf>, args: ServeArgs) -> Result<ExitCode> {
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

    let app = Arc::new(
        App::bootstrap(
            config,
            path,
            Bootstrap {
                refresh: if args.no_refresh {
                    Freshness::Never
                } else {
                    Freshness::IfStale
                },
                check: Freshness::IfStale,
            },
        )
        .await?,
    );

    let proxy_listener = TcpListener::bind(&proxy_address).await.map_err(|error| {
        Error::Other(format!(
            "cannot bind the HTTP proxy on {proxy_address}: {error}"
        ))
    })?;
    let api_listener = TcpListener::bind(&api_address).await.map_err(|error| {
        Error::Other(format!(
            "cannot bind the REST API on {api_address}: {error}"
        ))
    })?;

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

    let api_state = Arc::new(ApiState::new(
        app.pool.clone(),
        app.store.clone(),
        app.config.selection.strategy,
        app.config.selection.reuse_after,
        app.config.health.targets(),
        app.config.health.require,
    ));

    info!(
        proxy = %proxy_address,
        api = %api_address,
        config = ?app.config_path,
        proxies = app.pool.len(),
        alive = app.pool.stats().alive,
        auth = credentials.is_some(),
        "proxygate is ready"
    );

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

    tasks.spawn({
        let state = api_state.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            if let Err(error) = api::serve(state, api_listener, wait_for_shutdown(shutdown)).await {
                error!(error = %error, "REST API stopped");
            }
            "api"
        }
    });

    tasks.spawn({
        let app = app.clone();
        let shutdown = shutdown_rx.clone();
        async move {
            refresh_loop(app, shutdown).await;
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

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Resolves when the shutdown flag is set (or the sender is dropped).
async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    let _ = receiver.wait_for(|stop| *stop).await;
}

/// Re-fetches the subscribers and re-checks health on the refresh interval.
async fn refresh_loop(app: Arc<App>, shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(app.config.refresh.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; bootstrap already did the work.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = ticker.tick() => {
                let added = match app.refresh().await {
                    Ok(summary) => {
                        info!(
                            fetched = summary.fetched,
                            added = summary.added,
                            removed = summary.removed,
                            failed = summary.failed,
                            "subscriber refresh complete"
                        );
                        summary.added
                    }
                    Err(error) => {
                        warn!(error = %error, "subscriber refresh failed");
                        0
                    }
                };

                // New proxies need a verdict before they can be handed out; if
                // nothing changed, the health loop owns the next pass.
                if added > 0 {
                    match app.check(false).await {
                        Ok(report) => info!(report = %report.summary(), "health check complete"),
                        Err(error) => warn!(error = %error, "health check failed"),
                    }
                }
            }
        }
    }
}

/// Re-checks health on the health interval.
async fn health_loop(app: Arc<App>, shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(app.config.health.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => return,
            _ = ticker.tick() => {
                match app.check(false).await {
                    Ok(report) => info!(report = %report.summary(), "health check complete"),
                    Err(error) => warn!(error = %error, "health check failed"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Order for `list`: alive first, then fastest, then stable by URL.
fn sort_for_display(proxies: &mut [Proxy]) {
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

/// Writes one line to stdout, reporting a broken pipe instead of panicking.
fn print_stdout(text: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")
        .map_err(|error| Error::Other(format!("cannot write to stdout: {error}")))
}
