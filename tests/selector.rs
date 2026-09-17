//! The rotation contract, tested through the public selection API.
//!
//! This is the behaviour the project exists for:
//!
//! * a round hands out every healthy proxy once;
//! * proxies used within `reuse_after` are avoided while others are available;
//! * when everything has been used, the next request starts a new round
//!   immediately instead of waiting for the window to expire;
//! * the round survives a process restart.

use std::time::{Duration, SystemTime};

use proxygate::model::{self};
use proxygate::pool::{HealthUpdate, ProxyPool, Selection};
use proxygate::selector::Strategy;
use proxygate::state::StateStore;

const REUSE: Duration = Duration::from_secs(30 * 60);

fn pool_of(hosts: &[&str]) -> ProxyPool {
    let pool = ProxyPool::new();
    let mut updates = Vec::new();
    for host in hosts {
        let (id, _) = pool.insert(model::normalize(host).expect("valid proxy url"));
        updates.push((
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(10)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        ));
    }
    pool.apply_health_pass(&updates, 3);
    pool
}

fn hosts_of(pool: &ProxyPool) -> Vec<String> {
    let mut hosts: Vec<String> = pool
        .snapshot()
        .into_iter()
        .map(|proxy| proxy.host().to_string())
        .collect();
    hosts.sort();
    hosts
}

fn pick(pool: &ProxyPool, now: SystemTime) -> Selection {
    pool.select(Strategy::Random, REUSE, now)
        .expect("a healthy proxy is available")
}

#[test]
fn a_round_hands_out_every_proxy_exactly_once() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080", "3.3.3.3:8080"]);
    let now = SystemTime::now();

    let mut handed_out: Vec<String> = (0..3)
        .map(|_| pick(&pool, now).proxy.host().to_string())
        .collect();
    handed_out.sort();
    handed_out.dedup();

    assert_eq!(
        handed_out.len(),
        3,
        "each proxy must be handed out once per round"
    );
    assert_eq!(handed_out, hosts_of(&pool));
}

#[test]
fn a_new_round_starts_as_soon_as_everything_was_used() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let now = SystemTime::now();

    pick(&pool, now);
    pick(&pool, now);
    let round_before = pool.generation();

    // Nothing left in this round, and no waiting for the reuse window.
    let fourth = pick(&pool, now);
    assert!(fourth.reset_round, "the round must roll over immediately");
    assert_eq!(fourth.round, round_before + 1);
    assert_eq!(pool.generation(), round_before + 1);

    // The new round starts handing out proxies again.
    let fifth = pick(&pool, now);
    assert!(hosts_of(&pool).contains(&fifth.proxy.host().to_string()));
}

#[test]
fn proxies_used_recently_are_avoided() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let now = SystemTime::now();
    let snapshot = pool.snapshot();
    let recent = snapshot[0].id.clone();
    let stale = snapshot[1].id.clone();

    // Both were used in an earlier round, but only one recently.
    pool.restore_usage(&recent, 1, Some(now - Duration::from_secs(60)));
    pool.restore_usage(&stale, 1, Some(now - Duration::from_secs(60 * 60)));

    for _ in 0..5 {
        let selection = pick(&pool, now);
        assert_eq!(
            selection.proxy.id, stale,
            "the recently used proxy must be skipped while an older one is available"
        );
        // Put the pool back to its starting state for the next iteration.
        pool.restore_usage(&recent, 1, Some(now - Duration::from_secs(60)));
        pool.restore_usage(&stale, 1, Some(now - Duration::from_secs(60 * 60)));
    }
}

#[test]
fn recently_used_proxies_come_back_once_everything_else_is_gone() {
    let pool = pool_of(&["1.1.1.1:8080"]);
    let now = SystemTime::now();
    let id = pool.snapshot()[0].id.clone();
    pool.restore_usage(&id, 1, Some(now - Duration::from_secs(5)));

    // Only one proxy, used 5 seconds ago: it must still be handed out.
    let selection = pick(&pool, now);
    assert_eq!(selection.proxy.id, id);
    assert!(selection.reset_round);
}

#[test]
fn dead_proxies_are_never_selected() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let dead = pool.snapshot()[0].id.clone();
    pool.record_failure(&dead, 1);

    for _ in 0..5 {
        let selection = pick(&pool, SystemTime::now());
        assert_ne!(selection.proxy.id, dead);
    }

    // A pool with nothing alive yields nothing at all.
    let last_alive = pool
        .snapshot()
        .into_iter()
        .find(|proxy| proxy.alive)
        .unwrap();
    pool.record_failure(&last_alive.id, 1);
    assert!(
        pool.select(Strategy::Random, REUSE, SystemTime::now())
            .is_none()
    );
}

#[test]
fn latency_strategy_picks_the_fastest_alive_proxy() {
    let pool = ProxyPool::new();
    let mut updates = Vec::new();
    for (host, latency) in [
        ("1.1.1.1:8080", 300u64),
        ("2.2.2.2:8080", 25),
        ("3.3.3.3:8080", 900),
    ] {
        let (id, _) = pool.insert(model::normalize(host).unwrap());
        updates.push((
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(latency)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        ));
    }
    pool.apply_health_pass(&updates, 3);

    // Fastest first, then the fastest of what is left.
    let now = SystemTime::now();
    let first = pool
        .select(Strategy::Latency, REUSE, now)
        .expect("a healthy proxy");
    assert_eq!(first.proxy.host(), "2.2.2.2");

    let second = pool
        .select(Strategy::Latency, REUSE, now)
        .expect("a healthy proxy");
    assert_eq!(second.proxy.host(), "1.1.1.1");
}

#[test]
fn the_round_continues_across_a_restart() {
    let dir = std::env::temp_dir().join(format!("proxygate-it-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = StateStore::new(&dir);
    store.ensure_dir().unwrap();

    // First "process".
    let first = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let taken = pick(&first, SystemTime::now());
    store.persist(&first, SystemTime::now()).unwrap();

    // Second "process": same config, same cache directory.
    let second = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    store.restore(&second);

    let next = pick(&second, SystemTime::now());
    assert_ne!(
        next.proxy.id, taken.proxy.id,
        "a restart must not hand out the same proxy while the round has candidates left"
    );
    assert_eq!(next.round, taken.round);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selection_reports_its_own_diagnostics() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080", "3.3.3.3:8080"]);
    let now = SystemTime::now();

    let first = pick(&pool, now);
    assert_eq!(first.healthy, 3);
    assert_eq!(first.candidates, 3);
    assert!(!first.reset_round);

    let second = pick(&pool, now);
    assert_eq!(second.healthy, 3);
    assert_eq!(
        second.candidates, 2,
        "the proxy used in this round is no longer a candidate"
    );
    assert!(!second.reset_round);
}
