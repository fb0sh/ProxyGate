#!/usr/bin/env python3
"""打 /api/v1/get 的延迟基线：P50 / P95 / P99 + 服务端指标增量。

这台机器上没有 wrk/ab/hey，所以用标准库的线程 + `http.client` 打。它不追求把
服务端压到极限，只求**可复现、能对比**：优化之前跑一遍、之后跑同一份配置，两组
数字放一起看。

用法::

    # 默认：8 并发、5 秒，打 127.0.0.1:8080，跑完抓一次 /metrics 的前后差值
    python3 scripts/loadtest.py

    # 量"选择路径"本身：服务端要配 selection.verify: false
    python3 scripts/loadtest.py --label no-verify

    # 量真实发放路径（默认配置 verify: true，大头是现探）
    python3 scripts/loadtest.py --label verify --concurrency 16

    # 顺手看服务端进程的 fd / RSS（Linux）
    python3 scripts/loadtest.py --pid "$(pgrep -x proxygate | head -1)"

它只打印观测到的东西，脚本里没有任何"期望值"。
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import statistics
import sys
import threading
import time
import urllib.parse

# 每个线程用独立连接，和服务端之间不共享 socket，免得测出的是客户端排队。
RESULTS: list[tuple[int, float]] = []
LOCK = threading.Lock()

# 值得看增量的指标（前缀匹配整行）。
WATCHED = (
    "proxygate_pool_total ",
    "proxygate_pool_healthy ",
    "proxygate_check_total{",
    "proxygate_verify_total{",
    "proxygate_get_total{",
    "proxygate_get_latency_seconds_count{",
    "proxygate_get_latency_seconds_sum{",
    "proxygate_subscriber_fetch_total{",
    "proxygate_subscriber_proxies_count",
    "proxygate_state_save_total{",
)


def worker(host: str, port: int, path: str, deadline: float, timeout: float) -> None:
    local: list[tuple[int, float]] = []
    while time.monotonic() < deadline:
        started = time.perf_counter()
        try:
            connection = http.client.HTTPConnection(host, port, timeout=timeout)
            connection.request("GET", path)
            response = connection.getresponse()
            response.read()
            status = response.status
            connection.close()
        except Exception:  # noqa: BLE001 - 连接层失败也要计入，别假装没发生
            status = 0
        local.append((status, time.perf_counter() - started))
    with LOCK:
        RESULTS.extend(local)


def summarize(results: list[tuple[int, float]], wall: float, concurrency: int) -> dict:
    ok = sorted(elapsed for status, elapsed in results if status == 200)
    other = sorted({status for status, _ in results if status != 200})
    summary: dict = {
        "concurrency": concurrency,
        "wall_s": round(wall, 2),
        "requests": len(results),
        "ok": len(ok),
        "statuses": other,
        "qps": round(len(ok) / wall, 1) if wall else 0.0,
    }
    if not ok:
        return summary

    def percentile(p: float) -> float:
        index = max(0, min(len(ok) - 1, int(-(-p / 100 * len(ok) // 1)) - 1))
        return round(ok[index] * 1000, 2)

    summary |= {
        "mean_ms": round(statistics.fmean(ok) * 1000, 2),
        "p50_ms": round(statistics.median(ok) * 1000, 2),
        "p95_ms": percentile(95),
        "p99_ms": percentile(99),
        "max_ms": round(ok[-1] * 1000, 2),
    }
    return summary


def fetch_metrics(base: str) -> dict[str, float]:
    """抓一次 /metrics，返回 {序列（不含值）: 值}。"""
    parsed = urllib.parse.urlparse(base)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=10)
    connection.request("GET", "/metrics")
    body = connection.getresponse().read().decode()
    connection.close()

    series: dict[str, float] = {}
    for line in body.splitlines():
        if not line or line.startswith("#") or not line.startswith(WATCHED):
            continue
        name, _, value = line.rpartition(" ")
        try:
            series[name] = float(value)
        except ValueError:
            continue
    return series


def print_delta(before: dict[str, float], after: dict[str, float]) -> None:
    print("# metrics delta over the run:")
    for name in sorted(set(before) | set(after)):
        delta = after.get(name, 0.0) - before.get(name, 0.0)
        if name.endswith("pool_total") or name.endswith("pool_healthy") or delta:
            print(f"#   {name} {after.get(name, 0.0):g} (delta {delta:+g})")


def process_facts(pid: int) -> list[str]:
    """Linux 下的 fd 数与 RSS；读不到就返回空。"""
    facts: list[str] = []
    try:
        facts.append(f"open fds: {len(os.listdir(f'/proc/{pid}/fd'))}")
    except OSError:
        return facts
    try:
        with open(f"/proc/{pid}/status") as handle:
            facts.extend(
                line.strip()
                for line in handle
                if line.startswith(("VmRSS:", "Threads:"))
            )
    except OSError:
        pass
    return facts


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", default="http://127.0.0.1:8080", help="服务端地址")
    parser.add_argument("--path", default="/api/v1/get", help="要打的路径")
    parser.add_argument("--concurrency", type=int, default=8, help="并发连接数")
    parser.add_argument("--duration", type=float, default=5.0, help="持续秒数")
    parser.add_argument("--timeout", type=float, default=30.0, help="单请求超时")
    parser.add_argument("--label", default="", help="这一轮的标签，便于对比")
    parser.add_argument(
        "--pid",
        type=int,
        default=0,
        help="服务端进程号（读 fd/RSS）。用 `pgrep -x proxygate` 拿，别用 -f："
        "`timeout .../proxygate` 那种包装进程也会被 -f 匹配到",
    )
    parser.add_argument("--no-metrics", action="store_true", help="不抓 /metrics")
    args = parser.parse_args()

    parsed = urllib.parse.urlparse(args.base)
    host, port = parsed.hostname, parsed.port or 80

    before: dict[str, float] = {}
    if not args.no_metrics:
        try:
            before = fetch_metrics(args.base)
        except OSError as error:
            print(f"# cannot read /metrics before the run: {error}", file=sys.stderr)

    deadline = time.monotonic() + args.duration
    started = time.monotonic()
    threads = [
        threading.Thread(
            target=worker,
            args=(host, port, args.path, deadline, args.timeout),
            daemon=True,
        )
        for _ in range(args.concurrency)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    wall = time.monotonic() - started

    summary = summarize(RESULTS, wall, args.concurrency)
    if args.label:
        summary["label"] = args.label
    print(json.dumps(summary, ensure_ascii=False, sort_keys=True))

    if not args.no_metrics:
        try:
            print_delta(before, fetch_metrics(args.base))
        except OSError as error:
            print(f"# cannot read /metrics after the run: {error}", file=sys.stderr)

    if args.pid:
        for fact in process_facts(args.pid):
            print(f"# {fact}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
