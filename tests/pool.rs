//! Pool behaviour, exercised through the public API.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use proxygate::model::{self, Proxy};
use proxygate::pool::{HealthRestore, HealthUpdate, ProxyPool};
use proxygate::state::StateStore;

fn url(raw: &str) -> url::Url {
    model::normalize(raw).expect("valid proxy url")
}

fn proxies(hosts: &[&str]) -> ProxyPool {
    let pool = ProxyPool::new();
    pool.merge(hosts.iter().map(|host| url(host)));
    pool
}

fn mark_alive(pool: &ProxyPool, latency_ms: u64) {
    let updates: Vec<_> = pool
        .snapshot()
        .into_iter()
        .map(|proxy| {
            (
                proxy.id,
                HealthUpdate {
                    alive: true,
                    latency: Some(Duration::from_millis(latency_ms)),
                    checked_at: SystemTime::now(),
                    probes: Vec::new(),
                },
            )
        })
        .collect();
    pool.apply_health_pass(&updates, 3);
}

#[test]
fn merge_dedupes_equivalent_spellings() {
    let pool = ProxyPool::new();
    let stats = pool.merge(vec![
        url("1.2.3.4:8080"),
        url("http://1.2.3.4:8080"),
        url("HTTP://1.2.3.4:8080/"),
        url("socks5://5.6.7.8:1080"),
    ]);

    assert_eq!(stats.added, 2, "the three http spellings are one proxy");
    assert_eq!(stats.existing, 2);
    assert_eq!(pool.len(), 2);
}

#[test]
fn credentials_are_part_of_the_identity() {
    let pool = proxies(&[
        "user-a:pass@1.2.3.4:8080",
        "user-b:pass@1.2.3.4:8080",
        "1.2.3.4:8080",
    ]);
    assert_eq!(
        pool.len(),
        3,
        "same endpoint, different credentials -> different proxies"
    );

    let masked: Vec<String> = pool
        .snapshot()
        .iter()
        .map(Proxy::to_masked_string)
        .collect();
    assert!(masked.iter().all(|rendered| !rendered.contains("pass")));
}

#[test]
fn merging_never_resets_health_or_usage() {
    let pool = proxies(&["1.2.3.4:8080"]);
    let id = pool.snapshot()[0].id.clone();
    mark_alive(&pool, 42);
    pool.restore_usage(&id, 7, Some(SystemTime::now()));

    // A refresh merges the same URL again.
    pool.merge(vec![url("1.2.3.4:8080")]);

    let proxy = pool.get(&id).expect("proxy still present");
    assert!(proxy.alive);
    assert_eq!(proxy.latency_ms(), Some(42));
    assert_eq!(proxy.generation, 7);
    assert!(proxy.last_used_at.is_some());
}

#[test]
fn retain_removes_only_absent_proxies() {
    let pool = proxies(&["1.2.3.4:8080", "5.6.7.8:8080", "9.9.9.9:8080"]);
    let keep: HashSet<String> = pool
        .snapshot()
        .iter()
        .filter(|proxy| proxy.host() != "5.6.7.8")
        .map(|proxy| proxy.id.clone())
        .collect();

    let removed = pool.retain(&keep);
    assert_eq!(removed, 1);
    assert_eq!(pool.len(), 2);
    assert!(
        pool.snapshot()
            .iter()
            .all(|proxy| proxy.host() != "5.6.7.8")
    );
}

#[test]
fn health_pass_counts_failures_up_to_the_threshold() {
    let pool = proxies(&["1.2.3.4:8080"]);
    let id = pool.snapshot()[0].id.clone();
    mark_alive(&pool, 10);

    let failure = |id: &str| {
        vec![(
            id.to_string(),
            HealthUpdate {
                alive: false,
                latency: None,
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]
    };

    pool.apply_health_pass(&failure(&id), 3);
    assert!(pool.get(&id).unwrap().alive, "1 of 3 failures is not fatal");
    pool.apply_health_pass(&failure(&id), 3);
    assert!(pool.get(&id).unwrap().alive, "2 of 3 failures is not fatal");
    pool.apply_health_pass(&failure(&id), 3);
    let proxy = pool.get(&id).unwrap();
    assert!(!proxy.alive);
    assert_eq!(proxy.failures, 3);

    pool.record_success(&id, Some(Duration::from_millis(5)), SystemTime::now());
    let proxy = pool.get(&id).unwrap();
    assert!(proxy.alive);
    assert_eq!(proxy.failures, 0);
}

#[test]
fn cached_health_is_restored_verbatim() {
    let pool = proxies(&["1.2.3.4:8080", "5.6.7.8:8080"]);
    let snapshot = pool.snapshot();

    pool.restore_health(&[
        (
            snapshot[0].id.clone(),
            HealthRestore {
                alive: true,
                latency: Some(Duration::from_millis(12)),
                failures: 0,
                checked_at: Some(SystemTime::now()),
                probes: Vec::new(),
            },
        ),
        (
            snapshot[1].id.clone(),
            HealthRestore {
                alive: false,
                latency: None,
                failures: 2,
                checked_at: Some(SystemTime::now()),
                probes: Vec::new(),
            },
        ),
    ]);

    let stats = pool.stats();
    assert_eq!(stats.alive, 1);
    assert_eq!(stats.dead, 1);
    assert_eq!(pool.get(&snapshot[1].id).unwrap().failures, 2);
}

#[test]
fn usage_round_trips_through_state_json() {
    let dir = std::env::temp_dir().join(format!("proxygate-it-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = StateStore::new(&dir);
    store.ensure_dir().unwrap();

    let pool = proxies(&["1.2.3.4:8080", "5.6.7.8:8080"]);
    let used = pool.snapshot()[0].id.clone();
    let now = SystemTime::now();
    pool.set_generation(13);
    pool.mark_used_in_round(&used, 13, now);
    store.persist(&pool, now).unwrap();

    // A fresh process: same proxies, same usage, same round.
    let reloaded = proxies(&["1.2.3.4:8080", "5.6.7.8:8080"]);
    let summary = store.restore(&reloaded);
    assert_eq!(summary.generation, 13);
    // Only the proxy that was handed out carries state; the unused one is
    // simply absent from the file, which is why nothing is reported as missing.
    assert_eq!(summary.restored, 1);
    assert_eq!(summary.missing, 0);

    let proxy = reloaded.get(&used).unwrap();
    assert_eq!(proxy.generation, 13);
    assert_eq!(
        proxy.last_used_at.map(proxygate::state::to_rfc3339),
        Some(proxygate::state::to_rfc3339(now)),
        "second precision round trip"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_pool_has_no_candidates() {
    let pool = ProxyPool::new();
    assert!(pool.is_empty());
    assert!(
        pool.select(
            proxygate::selector::Strategy::Random,
            Duration::from_secs(60),
            SystemTime::now()
        )
        .is_none()
    );
    assert_eq!(pool.stats(), proxygate::pool::PoolStats::default());
}

#[test]
fn snapshot_order_is_deterministic() {
    let pool = proxies(&["9.9.9.9:8080", "1.2.3.4:8080", "5.6.7.8:8080"]);
    let first: Vec<String> = pool.snapshot().into_iter().map(|proxy| proxy.id).collect();
    let second: Vec<String> = pool.snapshot().into_iter().map(|proxy| proxy.id).collect();
    assert_eq!(
        first, second,
        "snapshots must be stable so output does not shuffle"
    );

    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted);
}
