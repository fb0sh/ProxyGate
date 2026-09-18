//! 持久化状态。
//!
//! `proxygate` 是 CLI，每次调用都是一个新进程：轮换状态以及订阅源和健康检查
//! 的缓存必须落到磁盘上。默认放在缓存目录（`~/.cache/proxygate`）里的两个
//! 文件：
//!
//! * `state.json` —— 只记录使用事实（`generation`、`last_used_at`），只有真正
//!   发放过的代理才有条目。健康状态从不作为事实持久化；下面的健康缓存只记住
//!   最后一次检查时间，这样连续调用 `GET /api/v1/get` 不必每次都重新探测
//!   整个池。
//! * `cache.json` —— 最近一次订阅源响应体，加上最近一次健康检查结果，各带
//!   时间戳以便过期；健康检查用过的探测目标也一并记录。
//!
//! 两个文件都以原子写入（临时文件 + rename）方式保存，目录以 0700、文件以
//! 0600 创建。缓存写入失败不是致命错误，只会记一条警告并继续服务；写入被
//! 权限拒绝时，错误信息还会附带可操作的排查提示。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::ProxyId;
use crate::pool::ProxyPool;

/// `state.json` 的文件名。
pub const STATE_FILE: &str = "state.json";
/// `cache.json` 的文件名。
pub const CACHE_FILE: &str = "cache.json";

/// 超过这个时长的条目会从 `state.json` 里丢弃，这样文件不会随着服务商不断
/// 轮换代理列表而无限增长。
pub const USAGE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// `state.json` —— README 中记录的结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    /// 当前代，发完一整轮后递增。
    #[serde(default)]
    pub generation: u64,
    /// 按代理 ID 索引的使用记录。
    #[serde(default)]
    pub proxies: HashMap<ProxyId, ProxyUsageFile>,
}

/// `state.json` 里单个代理的使用记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyUsageFile {
    /// 该代理最后一次被发放时所处的代。
    #[serde(default)]
    pub generation: u64,
    /// 最后一次被发放的时间，RFC3339 格式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

/// `cache.json` —— 带时间戳的订阅源结果与健康检查结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheFile {
    /// 最近一次成功拉取订阅源的时间，RFC3339 格式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    /// 最近一次拉取到的代理 URL，含明文凭据。
    #[serde(default)]
    pub proxies: Vec<String>,
    /// 最近一次健康检查的时间，RFC3339 格式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// 按代理 ID 索引的健康检查结果。
    #[serde(default)]
    pub health: HashMap<ProxyId, HealthFile>,
}

/// `cache.json` 里单个代理的健康检查结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthFile {
    /// 该代理是否存活。
    pub alive: bool,
    /// 最近一次健康检查的延迟，单位毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// 连续失败次数。
    #[serde(default)]
    pub failures: u32,
    /// 最近一轮的逐目标结果，这样重启后无需重新探测就能知道该代理连不上
    /// 哪个端点。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<TargetHealthFile>,
}

/// 针对单个探测目标的健康检查结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetHealthFile {
    /// 探测目标的 URL。
    pub target: String,
    /// 该目标是否可达。
    pub ok: bool,
    /// 该目标的延迟，单位毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// 读写缓存目录中的文件。
#[derive(Debug)]
pub struct StateStore {
    dir: PathBuf,
    /// 最近一次成功写入的 Unix 秒数，用于在 API 承受突发请求时限流
    /// `state.json` 的写入。
    last_write: std::sync::atomic::AtomicU64,
}

/// 一次状态恢复的结果统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreSummary {
    /// 成功恢复使用记录的代理数。
    pub restored: usize,
    /// 文件里有记录、但已不在代理池中的代理数。
    pub missing: usize,
    /// 从 `state.json` 恢复出的代。
    pub generation: u64,
}

impl StateStore {
    /// 用给定的缓存目录创建状态存储。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            last_write: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 返回缓存目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 返回 `state.json` 的完整路径。
    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    /// 返回 `cache.json` 的完整路径。
    pub fn cache_path(&self) -> PathBuf {
        self.dir.join(CACHE_FILE)
    }

    /// 确保缓存目录存在。
    ///
    /// 在 Unix 上目录以 `0700` 创建：`cache.json` 里含有带凭据的代理 URL，
    /// 不能被其他用户读取。
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| write_error(&self.dir, &error, "create"))?;
        restrict_permissions(&self.dir, 0o700);
        Ok(())
    }

    /// 读取 `state.json`，文件缺失或不可读时回退为空状态。
    pub fn load_state(&self) -> StateFile {
        load_json(&self.state_path()).unwrap_or_default()
    }

    /// 写入 `state.json`。
    pub fn save_state(&self, state: &StateFile) -> Result<()> {
        save_json(&self.state_path(), state)
    }

    /// 读取 `cache.json`，文件缺失或不可读时回退为空缓存。
    pub fn load_cache(&self) -> CacheFile {
        load_json(&self.cache_path()).unwrap_or_default()
    }

    /// 写入 `cache.json`。
    pub fn save_cache(&self, cache: &CacheFile) -> Result<()> {
        save_json(&self.cache_path(), cache)
    }

    /// 把磁盘上的代和逐代理使用记录复制回代理池。
    pub fn restore(&self, pool: &ProxyPool) -> RestoreSummary {
        let state = self.load_state();
        let mut summary = RestoreSummary {
            generation: state.generation,
            ..RestoreSummary::default()
        };
        if state.generation > 0 {
            pool.set_generation(state.generation);
        }
        for (id, usage) in state.proxies {
            let last_used_at = usage.last_used_at.as_deref().and_then(parse_rfc3339);
            if pool.restore_usage(&id, usage.generation, last_used_at) {
                summary.restored += 1;
            } else {
                summary.missing += 1;
            }
        }
        summary
    }

    /// 把代理池的使用事实写回 `state.json`，并丢弃不携带信息的条目：已经
    /// 消失的代理、从未被发放过的代理（它们的默认值就是默认值），以及超过
    /// [`USAGE_RETENTION`] 没人使用的条目。
    ///
    /// 当服务商的列表里有成千上万个代理时，让文件大小与用过的代理数成正比、
    /// 而不是与代理池大小成正比就很重要。
    pub fn persist(&self, pool: &ProxyPool, now: SystemTime) -> Result<StateFile> {
        // `usage()` only ever lists proxies that are currently in the pool, so
        // entries for departed proxies disappear by construction.
        let usage = pool.usage();
        let mut proxies: HashMap<ProxyId, ProxyUsageFile> = HashMap::new();
        for (id, generation, last_used_at) in usage {
            if generation == 0 && last_used_at.is_none() {
                continue;
            }
            if last_used_at
                .map(|last| now.duration_since(last).unwrap_or(Duration::ZERO) > USAGE_RETENTION)
                .unwrap_or(false)
            {
                continue;
            }
            proxies.insert(
                id,
                ProxyUsageFile {
                    generation,
                    last_used_at: last_used_at.map(to_rfc3339),
                },
            );
        }

        let state = StateFile {
            generation: pool.generation(),
            proxies,
        };
        self.save_state(&state)?;
        Ok(state)
    }

    /// 最多每 `min_interval` 持久化一次，这样突发的 `get` 调用不会每个请求
    /// 都重写 `state.json`。
    ///
    /// 真正写入状态时返回 `true`。
    pub fn persist_throttled(
        &self,
        pool: &ProxyPool,
        now: SystemTime,
        min_interval: Duration,
    ) -> Result<bool> {
        use std::sync::atomic::Ordering;

        let seconds = unix_secs(now).max(0) as u64;
        let interval = min_interval.as_secs().max(1);
        let last = self.last_write.load(Ordering::Relaxed);
        if last != 0 && seconds.saturating_sub(last) < interval {
            return Ok(false);
        }
        if self
            .last_write
            .compare_exchange(last, seconds, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Another task is already writing.
            return Ok(false);
        }
        self.persist(pool, now)?;
        Ok(true)
    }
}

impl CacheFile {
    /// 订阅源响应体仍在 `max_age` 内时返回 `true`。
    pub fn proxies_fresh(&self, max_age: Duration, now: SystemTime) -> bool {
        is_fresh(self.fetched_at.as_deref(), max_age, now) && !self.proxies.is_empty()
    }

    /// 健康检查结果仍在 `max_age` 内时返回 `true`。
    pub fn health_fresh(&self, max_age: Duration, now: SystemTime) -> bool {
        is_fresh(self.checked_at.as_deref(), max_age, now)
    }
}

/// 两个缓存共用的新鲜度判断。
pub fn is_fresh(timestamp: Option<&str>, max_age: Duration, now: SystemTime) -> bool {
    let Some(timestamp) = timestamp.and_then(parse_rfc3339) else {
        return false;
    };
    now.duration_since(timestamp)
        .map(|age| age <= max_age)
        .unwrap_or(true)
}

/// 读取并解析 JSON 文件；文件缺失、不可读或损坏时返回 `None`。
fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Option<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read state file");
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring corrupt state file");
            None
        }
    }
}

/// 以原子写入方式保存 JSON 文件：先写临时文件，再 rename 覆盖目标文件。
fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| write_error(parent, &error, "create"))?;
        restrict_permissions(parent, 0o700);
    }
    let mut body = serde_json::to_vec_pretty(value)?;
    // A trailing newline keeps `cat state.json` pleasant in a terminal.
    body.push(b'\n');
    // Write-then-rename so a crash can never leave a half-written file behind.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &body).map_err(|error| write_error(&tmp, &error, "write"))?;
    restrict_permissions(&tmp, 0o600);
    std::fs::rename(&tmp, path).map_err(|error| write_error(path, &error, "replace"))?;
    Ok(())
}

/// 为缓存文件写入失败构造带可操作提示的错误信息。
///
/// 缓存目录只读是最典型的失败场景（容器卷属于其他用户、服务账号没有
/// `$HOME`），所以要说明该怎么办，而不是重复裸的 `os error 13`。
fn write_error(path: &Path, error: &std::io::Error, verb: &str) -> Error {
    let hint = if error.kind() == std::io::ErrorKind::PermissionDenied {
        " (the cache directory must be writable; set `state.dir` in the config or $PROXYGATE_CACHE_DIR)"
    } else {
        ""
    };
    Error::Other(format!("cannot {verb} {}: {error}{hint}", path.display()))
}

/// 尽力收紧权限；失败也不值得中断一次写入。
#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// 非 Unix 平台没有权限位，此函数不做任何事。
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) {}

/// 自 Unix 纪元起的秒数（1970 年之前为负）。
pub fn unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    }
}

/// 把自 Unix 纪元起的秒数还原为 `SystemTime`。
pub fn from_unix_secs(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// UTC 的 RFC3339 时间戳，精确到秒：`2026-09-17T10:30:00Z`。
pub fn to_rfc3339(time: SystemTime) -> String {
    let secs = unix_secs(time);
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 解析本模块写入的（以及大多数其他来源的）RFC3339 时间戳。
pub fn parse_rfc3339(input: &str) -> Option<SystemTime> {
    let bytes = input.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(bytes.get(range)?)
            .ok()?
            .parse::<i64>()
            .ok()
    };

    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    // Optional fractional seconds and timezone offset.
    let mut rest = &input[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let end = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        rest = &stripped[end..];
    }
    let offset_secs = match rest.as_bytes().first() {
        Some(b'Z') | Some(b'z') | None => 0,
        Some(b'+') | Some(b'-') => {
            let sign = if rest.as_bytes()[0] == b'-' { -1 } else { 1 };
            let body = &rest[1..];
            let (hours, minutes) = body.split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };

    let days = days_from_civil(year, month as u32, day as u32);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(from_unix_secs(secs))
}

/// 由年月日求天数（Howard Hinnant 算法），对任意年份有效。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u64;
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) as u64 + 2) / 5 + day as u64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era as i64 - 719_468
}

/// 由天数求年月日（Howard Hinnant 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::normalize;
    use crate::pool::{HealthUpdate, ProxyPool};

    fn temp_store(name: &str) -> StateStore {
        let dir =
            std::env::temp_dir().join(format!("proxygate-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        StateStore::new(dir)
    }

    #[test]
    fn formats_and_parses_rfc3339() {
        assert_eq!(to_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            to_rfc3339(from_unix_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            to_rfc3339(parse_rfc3339("2026-09-17T10:30:00Z").unwrap()),
            "2026-09-17T10:30:00Z"
        );
        // Leap day and a pre-epoch timestamp.
        assert_eq!(
            to_rfc3339(from_unix_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
        assert_eq!(to_rfc3339(from_unix_secs(-1)), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn parses_offsets_and_fractions() {
        assert_eq!(
            parse_rfc3339("2026-09-17T10:30:00.123Z").unwrap(),
            parse_rfc3339("2026-09-17T10:30:00Z").unwrap()
        );
        assert_eq!(
            parse_rfc3339("2026-09-17T12:30:00+02:00").unwrap(),
            parse_rfc3339("2026-09-17T10:30:00Z").unwrap()
        );
        assert!(parse_rfc3339("nonsense").is_none());
        assert!(parse_rfc3339("2026-13-40T99:99:99Z").is_none());
    }

    #[test]
    fn state_round_trips_through_disk() {
        let store = temp_store("state");

        let pool = ProxyPool::new();
        let (id, _) = pool.insert(normalize("1.1.1.1:8080").unwrap());
        pool.insert(normalize("2.2.2.2:8080").unwrap());
        pool.set_generation(13);
        let now = SystemTime::now();
        pool.mark_used_in_round(&id, 13, now);

        store.persist(&pool, now).unwrap();

        let restored_pool = ProxyPool::new();
        restored_pool.insert(normalize("1.1.1.1:8080").unwrap());
        restored_pool.insert(normalize("3.3.3.3:8080").unwrap());
        let summary = store.restore(&restored_pool);

        assert_eq!(summary.generation, 13);
        assert_eq!(summary.restored, 1);
        // `2.2.2.2` was never handed out, so it was not written at all, and
        // `3.3.3.3` simply does not exist in the file.
        assert_eq!(summary.missing, 0);
        assert_eq!(restored_pool.generation(), 13);
        let proxy = restored_pool.get(&id).unwrap();
        assert_eq!(proxy.generation, 13);
        assert!(proxy.last_used_at.is_some());

        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn persist_prunes_stale_and_absent_entries() {
        let store = temp_store("prune");
        let pool = ProxyPool::new();
        let (present, _) = pool.insert(normalize("1.1.1.1:8080").unwrap());
        let (stale, _) = pool.insert(normalize("2.2.2.2:8080").unwrap());
        let (gone, _) = pool.insert(normalize("3.3.3.3:8080").unwrap());

        let now = SystemTime::now();
        pool.mark_used_in_round(&present, 2, now);
        pool.mark_used_in_round(&stale, 2, now - Duration::from_secs(48 * 3600));

        // `gone` disappears from the pool before persisting.
        pool.retain(&std::collections::HashSet::from([
            present.clone(),
            stale.clone(),
        ]));
        let state = store.persist(&pool, now).unwrap();

        assert!(state.proxies.contains_key(&present));
        assert!(
            !state.proxies.contains_key(&stale),
            "stale usage must be pruned"
        );
        assert!(
            !state.proxies.contains_key(&gone),
            "absent usage must be pruned"
        );
        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn persist_skips_proxies_that_were_never_used() {
        let store = temp_store("unused");
        let pool = ProxyPool::new();
        let (used, _) = pool.insert(normalize("1.1.1.1:8080").unwrap());
        pool.insert(normalize("2.2.2.2:8080").unwrap());
        pool.insert(normalize("3.3.3.3:8080").unwrap());
        let now = SystemTime::now();
        pool.mark_used_in_round(&used, pool.generation(), now);

        let state = store.persist(&pool, now).unwrap();
        assert_eq!(
            state.proxies.len(),
            1,
            "unused proxies are the default state and need no entry"
        );
        assert!(state.proxies.contains_key(&used));

        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn cache_round_trips_and_expires() {
        let store = temp_store("cache");
        let pool = ProxyPool::new();
        let (id, _) = pool.insert(normalize("1.1.1.1:8080").unwrap());
        pool.update_health(&[(
            id.clone(),
            HealthUpdate {
                alive: true,
                latency: Some(Duration::from_millis(42)),
                checked_at: SystemTime::now(),
                probes: Vec::new(),
            },
        )]);

        let now = SystemTime::now();
        let cache = CacheFile {
            fetched_at: Some(to_rfc3339(now)),
            proxies: vec!["http://1.1.1.1:8080".into()],
            checked_at: Some(to_rfc3339(now)),
            health: HashMap::from([(
                id.clone(),
                HealthFile {
                    alive: true,
                    latency_ms: Some(42),
                    failures: 0,
                    targets: vec![TargetHealthFile {
                        target: "https://example.test/".into(),
                        ok: true,
                        latency_ms: Some(42),
                    }],
                },
            )]),
        };
        store.save_cache(&cache).unwrap();

        let loaded = store.load_cache();
        assert_eq!(loaded.proxies, vec!["http://1.1.1.1:8080".to_string()]);
        assert!(loaded.proxies_fresh(Duration::from_secs(60), now));
        assert!(!loaded.proxies_fresh(Duration::from_secs(60), now + Duration::from_secs(61)));
        assert!(loaded.health_fresh(Duration::from_secs(30), now));
        assert_eq!(loaded.health.get(&id).unwrap().latency_ms, Some(42));

        let _ = std::fs::remove_dir_all(store.dir());
    }

    #[test]
    fn missing_files_are_not_errors() {
        let store = temp_store("missing");
        assert_eq!(store.load_state().generation, 0);
        assert!(store.load_cache().proxies.is_empty());
        let _ = std::fs::remove_dir_all(store.dir());
    }
}
