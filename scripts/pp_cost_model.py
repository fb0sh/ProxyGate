#!/usr/bin/env python3
"""jhao104/proxy_pool 的 `/get` 成本模型：量它 O(N) 的那一段。

对比对象是 <https://github.com/jhao104/proxy_pool>（v2.4.0）。它的代理存在 Redis
的一个 hash 里（`HKEYS`/`HVALS use_proxy`，见 `db/redisClient.py`），而 `/get`
是这么取的：

    /get                HKEYS use_proxy -> random.choice -> HGET use_proxy <key>
    /get?type=https     HVALS use_proxy -> json.loads 每条 -> 过滤 https -> choice

两条路都把**整个池子**从 Redis 拉到 Python 进程里：HKEYS 拉 N 个 key，HVALS 拉
N 份 JSON。Redis 自己很快，花钱的是响应字节过一遍 loopback、redis-py（没装
hiredis 时是纯 Python 解析器）把 RESP 切成 N 个 `str`，以及 https 分支上那 N 次
`json.loads`。

这个脚本只量这些**单元成本**，不假装在跑他们那套栈（这里没有 Redis，也没有
gunicorn）：拿到常数以后，乘上 N 就是一次 `/get` 的下限。用 `split` 那版是给
「装了 hiredis、解析回到 C 里」留的乐观下界。

    python3 scripts/pp_cost_model.py            # 默认 5000 / 20000 / 50000
"""

from __future__ import annotations

import json
import random
import socket
import statistics
import sys
import threading
import time

# 一条真实条目的形状：key 是 ip:port，value 是 Proxy.to_json 那种小 JSON。
KEY = "110.169.137.4:8080"
VALUE = json.dumps(
    {
        "proxy": KEY,
        "https": True,
        "fail_count": 0,
        "source": "zdaye",
        "check_count": 3,
        "last_status": True,
        "last_time": "2026-09-19 04:25:44",
        "region": "CN",
    },
    separators=(",", ":"),
)


def resp_array(items: list[str]) -> bytes:
    """HKEYS/HVALS 的 RESP2 响应：`*N` 之后每个 bulk string。"""
    out = [f"*{len(items)}\r\n".encode()]
    for item in items:
        raw = item.encode()
        out.append(b"$%d\r\n" % len(raw))
        out.append(raw)
        out.append(b"\r\n")
    return b"".join(out)


def parse_resp_array(payload: bytes) -> list[str]:
    """按 redis-py 纯 Python 解析器的做法，把 bulk string 数组读成 list[str]。"""
    items: list[str] = []
    pos = payload.index(b"\r\n") + 2  # 跳过 *N
    while pos < len(payload):
        end = payload.index(b"\r\n", pos)
        length = int(payload[pos + 1 : end])
        start = end + 2
        items.append(payload[start : start + length].decode())
        pos = start + length + 2
    return items


def parse_resp_array_fast(payload: bytes) -> int:
    """C 侧解析的下界参考：让 C 切出同样多的对象（hiredis 的乐观上界）。"""
    return len(payload.split(b"\r\n"))


def measure(fn, repeat: int = 7) -> tuple[float, float]:
    """返回 (中位数 ms, 最好一次 ms)。"""
    samples = []
    for _ in range(repeat):
        started = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - started) * 1000)
    return statistics.median(samples), min(samples)


def socket_roundtrip(payload: bytes, repeat: int = 7) -> float:
    """把同样多的字节推过 loopback 一次（含服务端写出），取中位数 ms。"""
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(4)
    address = listener.getsockname()
    stop = threading.Event()

    def serve() -> None:
        while not stop.is_set():
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            try:
                connection.sendall(payload)
            finally:
                connection.close()

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    timings = []
    for _ in range(repeat):
        started = time.perf_counter()
        client = socket.create_connection(address)
        received = bytearray()
        while len(received) < len(payload):
            chunk = client.recv(1 << 20)
            if not chunk:
                break
            received += chunk
        client.close()
        if len(received) != len(payload):
            raise RuntimeError(f"short read: {len(received)} != {len(payload)}")
        timings.append((time.perf_counter() - started) * 1000)
    stop.set()
    listener.close()
    thread.join(timeout=1)
    return statistics.median(timings)


def main() -> None:
    print(f"# python {sys.version.split()[0]}，loopback，key {len(KEY)} B / value {len(VALUE)} B")
    for count in (5000, 20000, 50000):
        keys_payload = resp_array([KEY] * count)
        values_payload = resp_array([VALUE] * count)
        parsed = parse_resp_array(values_payload)

        keys_wire = socket_roundtrip(keys_payload)
        values_wire = socket_roundtrip(values_payload)
        keys_parse, _ = measure(lambda: parse_resp_array(keys_payload))
        values_parse, _ = measure(lambda: parse_resp_array(values_payload))
        keys_fast, _ = measure(lambda: parse_resp_array_fast(keys_payload))
        values_fast, _ = measure(lambda: parse_resp_array_fast(values_payload))
        values_json, _ = measure(lambda: [json.loads(item) for item in parsed])
        choice, _ = measure(lambda: random.choice(parsed), repeat=200)

        print(f"\n## N = {count}")
        print(
            f"   /get              {len(keys_payload) / 1024:8.1f} KiB  "
            f"wire {keys_wire:5.2f} + parse {keys_parse:6.2f}"
            f"（C 侧下界 {keys_fast:.2f}）"
            f" = {keys_wire + keys_parse:6.2f} ms"
        )
        print(
            f"   /get?type=https   {len(values_payload) / 1024:8.1f} KiB  "
            f"wire {values_wire:5.2f} + parse {values_parse:6.2f}"
            f"（C 侧下界 {values_fast:.2f}）"
            f" + json {values_json:6.2f}"
            f" = {values_wire + values_parse + values_json:6.2f} ms"
        )
        print(f"   random.choice {choice:.4f} ms；HGET 是单个 bulk string，忽略不计")


if __name__ == "__main__":
    main()
