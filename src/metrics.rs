//! 指标：让「优化」有依据，而不是靠猜。
//!
//! 这里只有一层薄封装：[`metrics`] 是门面，[`metrics_exporter_prometheus`]
//! 把注册表渲染成 Prometheus 文本，`GET /metrics` 原样返回它。
//!
//! 命名带 `proxygate_` 前缀（Prometheus 的 namespace），暴露的家族：
//!
//! | 指标 | 类型 | 标签 | 回答的问题 |
//! | --- | --- | --- | --- |
//! | `pool_total` | gauge | – | 池子里一共有多少代理（抓取时刻的值） |
//! | `pool_healthy` | gauge | – | 其中有多少是健康的（抓取时刻的值） |
//! | `check_total` | counter | `result=ok\|fail` | 健康探测一共成功/失败多少次，成功率是多少 |
//! | `verify_total` | counter | `result=ok\|fail\|fresh` | 发放前验证：通过、失败、以及因为判定足够新而跳过 |
//! | `get_total` | counter | `strategy`, `result` | `/api/v1/get` 的分发次数（`result=ok\|empty`） |
//! | `get_latency_seconds` | histogram | `strategy` | 一次发放要多久（含现探），P99 从这里看 |
//! | `subscriber_fetch_total` | counter | `result=ok\|fail` | 订阅源脚本跑成功/失败多少次 |
//! | `subscriber_proxies` | histogram | – | 一个来源一次贡献多少条可用代理 |
//! | `state_save_total` | counter | `result=ok\|error` | 轮换状态落盘次数 |
//! | `build_info` | gauge | `version` | 恒为 1，用来对齐版本 |
//!
//! 两个 gauge 在**抓取时**从代理池现取（[`set_pool`]），这样它们永远和池子
//! 一致，不需要在别处维护第二份计数。
//!
//! 没有安装 recorder 时（例如库的使用者只用了 [`crate::app::App`]），所有
//! `metrics` 宏都是空操作，进程照常跑——指标是观测手段，不是依赖。

use std::sync::OnceLock;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{
    Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};

use crate::pool::PoolStats;

// 指标名写在常量里：`metrics` 的宏要字面量，而名字被 `describe_*` 和记录点
// 共用，写错一个字母就会变成两个指标家族。
const POOL_TOTAL: &str = "proxygate_pool_total";
const POOL_HEALTHY: &str = "proxygate_pool_healthy";
const CHECK_TOTAL: &str = "proxygate_check_total";
const VERIFY_TOTAL: &str = "proxygate_verify_total";
const GET_TOTAL: &str = "proxygate_get_total";
const GET_LATENCY: &str = "proxygate_get_latency_seconds";
const SUBSCRIBER_FETCH: &str = "proxygate_subscriber_fetch_total";
const SUBSCRIBER_PROXIES: &str = "proxygate_subscriber_proxies";
const STATE_SAVE: &str = "proxygate_state_save_total";
const BUILD_INFO: &str = "proxygate_build_info";

/// `get_latency_seconds` 的桶：从 5ms 到 10s，覆盖"命中缓存判定"到"现探三次"。
/// 最后一桶是 `+Inf`，由 exporter 自己补。
const LATENCY_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `subscriber_proxies` 的桶：一个来源一次贡献 1 到 10,000+ 条。
const PROXY_COUNT_BUCKETS: [f64; 8] = [1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0];

/// 已安装的 recorder 句柄；[`install`] 只会真正安装一次。
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// 安装全局 recorder，返回渲染句柄。
///
/// 重复调用返回同一个句柄（`metrics` 的全局 recorder 只能装一次，测试里
/// 多个用例共用它是安全的）。
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            // `build_recorder` 而不是 `install_recorder`：安装要显式做，好让
            // 重复调用拿到同一个句柄而不是 panic。
            //
            // 指标名的 `proxygate_` 前缀是手写的，不走 exporter 的 namespace
            // 或 `PrefixLayer`：宏要字面量，而这两条路都得多一个依赖。
            // 直方图按 Prometheus 的桶输出（而不是默认的 summary 分位数）：
            // 分位数是进程内滑动窗口算的，桶则能用 `histogram_quantile()` 在
            // 查询侧算 P99，也能跨实例汇总。
            let recorder: PrometheusRecorder = PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Full(GET_LATENCY.to_string()), &LATENCY_BUCKETS)
                .expect("latency buckets are not empty")
                .set_buckets_for_metric(
                    Matcher::Full(SUBSCRIBER_PROXIES.to_string()),
                    &PROXY_COUNT_BUCKETS,
                )
                .expect("proxy buckets are not empty")
                .build_recorder();
            let handle = recorder.handle();
            let _ = metrics::set_global_recorder(recorder);
            describe();
            gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
            handle
        })
        .clone()
}

/// 给每个指标补上 HELP / TYPE，`/metrics` 里因此能看出单位与含义。
fn describe() {
    describe_gauge!(POOL_TOTAL, "proxies currently in the pool");
    describe_gauge!(POOL_HEALTHY, "proxies currently considered healthy");
    describe_counter!(
        CHECK_TOTAL,
        "health probes performed, labelled by result (ok|fail)"
    );
    describe_counter!(
        VERIFY_TOTAL,
        "hand-out verifications, labelled by result (ok|fail|fresh)"
    );
    describe_counter!(
        GET_TOTAL,
        "proxy hand-outs, labelled by selection strategy and result (ok|empty)"
    );
    describe_histogram!(
        GET_LATENCY,
        "wall-clock time to hand out one proxy, including verification"
    );
    describe_counter!(
        SUBSCRIBER_FETCH,
        "subscriber script runs, labelled by result (ok|fail)"
    );
    describe_histogram!(
        SUBSCRIBER_PROXIES,
        "usable proxies contributed by one subscriber run"
    );
    describe_counter!(
        STATE_SAVE,
        "rotation-state writes, labelled by result (ok|error)"
    );
}

/// 刷新代理池的两个 gauge。在 `GET /metrics` 时调用，取的是当下的事实。
pub fn set_pool(stats: &PoolStats) {
    gauge!(POOL_TOTAL).set(stats.total as f64);
    gauge!(POOL_HEALTHY).set(stats.alive as f64);
}

/// 一轮健康探测的结果。
pub fn record_check(alive: usize, dead: usize) {
    if alive > 0 {
        counter!(CHECK_TOTAL, "result" => "ok").increment(alive as u64);
    }
    if dead > 0 {
        counter!(CHECK_TOTAL, "result" => "fail").increment(dead as u64);
    }
}

/// 一次发放前验证的结果。
///
/// `fresh` 表示判定还在 `selection.max_age` 之内、这次没有真的去探测。
pub fn record_verify(result: &'static str) {
    counter!(VERIFY_TOTAL, "result" => result).increment(1);
}

/// 一次 `/api/v1/get` 的结果与耗时。
pub fn record_get(strategy: &'static str, ok: bool, elapsed: std::time::Duration) {
    let result = if ok { "ok" } else { "empty" };
    counter!(GET_TOTAL, "strategy" => strategy, "result" => result).increment(1);
    histogram!(GET_LATENCY, "strategy" => strategy).record(elapsed.as_secs_f64());
}

/// 一次订阅源脚本的运行结果。
pub fn record_subscriber(ok: bool, proxies: usize) {
    let result = if ok { "ok" } else { "fail" };
    counter!(SUBSCRIBER_FETCH, "result" => result).increment(1);
    if ok {
        histogram!(SUBSCRIBER_PROXIES).record(proxies as f64);
    }
}

/// 一次轮换状态的落盘结果。
pub fn record_state_save(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    counter!(STATE_SAVE, "result" => result).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 渲染结果里是否存在这个序列，且它带一个数值（Prometheus 的每一行都是
    /// `名字{标签} 值`）。
    fn has_series(rendered: &str, series: &str) -> bool {
        rendered.lines().any(|line| {
            line.starts_with(series)
                && line
                    .rsplit(' ')
                    .next()
                    .map(|value| value.parse::<f64>().is_ok())
                    .unwrap_or(false)
        })
    }

    #[test]
    fn installing_twice_is_harmless_and_the_registry_is_renderable() {
        let first = install();
        let second = install();

        record_check(3, 1);
        record_verify("fresh");
        record_get("random", true, std::time::Duration::from_millis(250));
        record_get("latency", false, std::time::Duration::from_millis(10));
        record_subscriber(true, 17);
        record_subscriber(false, 0);
        record_state_save(true);

        let rendered = second.render();
        // 两个句柄指向同一个注册表，所以第一个也渲染得出同样的内容。
        assert!(first.render().contains("proxygate_build_info"));

        // 只断言"家族与标签都在、值是一个数"：同一个进程里其他用例也会往
        // 同一个注册表计数，写死数值会让测试互相干扰。
        for needle in [
            "# HELP proxygate_pool_total",
            "# TYPE proxygate_pool_total gauge",
            "# HELP proxygate_get_latency_seconds",
        ] {
            assert!(
                rendered.contains(needle),
                "missing `{needle}` in:\n{rendered}"
            );
        }

        for series in [
            "proxygate_build_info{version=",
            "proxygate_check_total{result=\"ok\"}",
            "proxygate_check_total{result=\"fail\"}",
            "proxygate_verify_total{result=\"fresh\"}",
            "proxygate_get_total{strategy=\"random\",result=\"ok\"}",
            "proxygate_get_total{strategy=\"latency\",result=\"empty\"}",
            "proxygate_get_latency_seconds_bucket{strategy=\"random\",le=\"0.5\"}",
            "proxygate_get_latency_seconds_count{strategy=\"random\"}",
            "proxygate_subscriber_fetch_total{result=\"ok\"}",
            "proxygate_subscriber_fetch_total{result=\"fail\"}",
            "proxygate_subscriber_proxies_count",
            "proxygate_state_save_total{result=\"ok\"}",
        ] {
            assert!(
                has_series(&rendered, series),
                "missing `{series}` in:\n{rendered}"
            );
        }
    }

    #[test]
    fn the_pool_gauges_track_the_last_scrape() {
        let handle = install();
        set_pool(&PoolStats {
            total: 12,
            alive: 5,
            ..PoolStats::default()
        });
        let rendered = handle.render();
        assert!(rendered.contains("proxygate_pool_total 12"), "{rendered}");
        assert!(rendered.contains("proxygate_pool_healthy 5"), "{rendered}");

        set_pool(&PoolStats {
            total: 1,
            alive: 0,
            ..PoolStats::default()
        });
        let rendered = handle.render();
        assert!(rendered.contains("proxygate_pool_total 1"), "{rendered}");
        assert!(rendered.contains("proxygate_pool_healthy 0"), "{rendered}");
    }
}
