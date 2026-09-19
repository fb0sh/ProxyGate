//! The rotation contract, tested through the public selection API.
//!
//! This is the behaviour the project exists for:
//!
//! * a round hands out every healthy proxy once;
//! * proxies used within `reuse_after` are avoided while others are available;
//! * when everything has been used, the next request starts a new round
//!   immediately instead of waiting for the window to expire;
//! * the round survives a process restart.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime};

use proxygate::model::{self};
use proxygate::pool::{HealthUpdate, ProxyPool, Selection};
use proxygate::selector::{SelectionOptions, Strategy};
use proxygate::state::StateStore;

const REUSE: Duration = Duration::from_secs(30 * 60);

/// 判死阈值 1 的策略：一次失败就算死，用来测"判死之后立刻不再分发"。
fn strict_policy() -> proxygate::pool::HealthPolicy {
    proxygate::pool::HealthPolicy {
        max_failures: 1,
        ..policy()
    }
}

/// 测试用的健康策略：判死阈值 3，成功 5 分钟后再探，失败退避 5s 起。
fn policy() -> proxygate::pool::HealthPolicy {
    proxygate::pool::HealthPolicy {
        max_failures: 3,
        ok_delay: Duration::from_secs(300),
        backoff_base: Duration::from_secs(5),
        backoff_max: Duration::from_secs(1800),
    }
}

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
    pool.apply_health_pass(&updates, &policy());
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
    pool.select(
        SelectionOptions {
            strategy: Strategy::Random,
            reuse_after: REUSE,
            ..Default::default()
        },
        now,
    )
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
    pool.record_failure(&dead, &strict_policy(), SystemTime::now());

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
    pool.record_failure(&last_alive.id, &strict_policy(), SystemTime::now());
    assert!(
        pool.select(
            SelectionOptions {
                strategy: Strategy::Random,
                reuse_after: REUSE,
                ..Default::default()
            },
            SystemTime::now()
        )
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
    pool.apply_health_pass(&updates, &policy());

    // Fastest first, then the fastest of what is left.
    let now = SystemTime::now();
    let first = pool
        .select(
            SelectionOptions {
                strategy: Strategy::Latency,
                reuse_after: REUSE,
                ..Default::default()
            },
            now,
        )
        .expect("a healthy proxy");
    assert_eq!(first.proxy.host(), "2.2.2.2");

    let second = pool
        .select(
            SelectionOptions {
                strategy: Strategy::Latency,
                reuse_after: REUSE,
                ..Default::default()
            },
            now,
        )
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

/// 并发分发：20 个健康代理、20 个线程同时 `select`，一轮里不许重复。
///
/// 这是 CAS 认领的回归测试——旧版靠写锁保证，新版靠条目上的
/// `compare_exchange`；把它弄丢的话，这里会看到同一个代理被发两次。
#[test]
fn concurrent_hand_outs_never_repeat_within_a_round() {
    let hosts: Vec<String> = (1..=20).map(|i| format!("10.0.0.{i}:8080")).collect();
    let borrowed: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let pool = Arc::new(pool_of(&borrowed));
    let now = SystemTime::now();

    let barrier = Arc::new(Barrier::new(hosts.len()));
    let handles: Vec<_> = (0..hosts.len())
        .map(|_| {
            let pool = pool.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                pick(&pool, now).proxy.host().to_string()
            })
        })
        .collect();

    let mut handed_out: Vec<String> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();
    let total = handed_out.len();
    handed_out.sort();
    handed_out.dedup();
    assert_eq!(
        handed_out.len(),
        total,
        "一轮内每个健康代理只能被分发一次：{handed_out:?}"
    );
}

/// 判死之后立刻就不再分发它——不需要重建快照。
///
/// 快照只记"有哪些代理"，死活读的是条目里的原子量；这个测试保证后者没被
/// 漏掉（否则池子里死掉的代理会一直被发出去，直到下一次重建）。
#[test]
fn a_proxy_marked_dead_disappears_from_selection_immediately() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let dead = pool.snapshot()[0].id.clone();
    pool.record_failure(&dead, &strict_policy(), SystemTime::now());

    for _ in 0..5 {
        let selection = pick(&pool, SystemTime::now());
        assert_ne!(selection.proxy.id, dead);
        assert_eq!(selection.healthy, 1, "健康数应当立刻反映这次判死");
    }
}

/// 新合并进来的代理立刻可以被分发。
#[test]
fn merged_proxies_are_visible_to_selection() {
    let pool = pool_of(&["1.1.1.1:8080"]);
    pick(&pool, SystemTime::now());

    let (id, is_new) = pool.insert(model::normalize("9.9.9.9:8080").unwrap());
    assert!(is_new);
    pool.apply_health_pass(
        &[(
            id,
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(5)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )],
        &policy(),
    );

    let selection = pick(&pool, SystemTime::now());
    assert_eq!(selection.proxy.host(), "9.9.9.9");
}

/// 使用记录与轮换在重建快照之后依然连续。
#[test]
fn rotation_survives_a_snapshot_rebuild() {
    let pool = pool_of(&["1.1.1.1:8080", "2.2.2.2:8080"]);
    let now = SystemTime::now();
    let first = pick(&pool, now);

    // 结构变化重建快照，已经发过的那个不该被忘掉。
    pool.insert(model::normalize("3.3.3.3:8080").unwrap());

    let second = pick(&pool, now);
    assert_ne!(
        first.proxy.id, second.proxy.id,
        "重建快照不该重置本轮的使用记录"
    );
}
