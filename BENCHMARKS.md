# 性能基线

优化之前先量。这份文件记录**怎么量**和**量到了什么**，所有数字都是本机实测，
不是估算——要质疑某个数就照着下面的命令重跑一遍。

结论先写在这里：**`/api/v1/get` 的吞吐上限由代理池的写锁决定，而每请求的持锁
时间随池子大小线性增长。** 5,000 条时上限约 600 QPS（只用掉 1 个核），1,000 条时
约 2,300 QPS。健康探测的失败率是 99.1%，也就是说每轮全量重探 99% 的请求是白花的。

## 怎么量

```bash
# 1. 起一份带池子的服务（见下面的"测量用的配置"）
cargo build --release
PROXYGATE_CONFIG=/path/to/config.yaml ./target/release/proxygate &

# 2. 打 /api/v1/get，同时抓 /metrics 前后差值
python3 scripts/loadtest.py --concurrency 8 --duration 5 --label "..." \
    --pid "$(pgrep -x proxygate | head -1)"
```

`scripts/loadtest.py` 用标准库的线程 + `http.client`；这台机器上没有 wrk/ab/hey，
而它只需要**可复现**，不需要把服务端压到极限。它打印客户端看到的 P50/P95/P99，
以及这一轮里 `proxygate_*` 指标的增量；`--pid` 再补上 fd 数与 RSS。

判断瓶颈看两个地方，**别只看客户端延迟**：

* `proxygate_get_latency_seconds_sum / _count` = 服务端眼里的每次发放耗时；
* `/proc/<pid>/stat` 的 utime+stime 增量 = 服务端真正烧掉的 CPU。

两者差得多，就说明请求在排队（等锁），而不是在算。

## 测量用的配置

`verify: false` 用来把"选择路径"单独拿出来量——否则每次 `/get` 都会现探一个
候选（`selection.max_age`），网络时间会把选择开销完全盖住。

```yaml
server:
  listen: 127.0.0.1:18920
subscribers:
  - name: bulk
    timeout: 20s
    lua_code: |
      -- N 条互不相同的本地代理：端口相同、用户名不同，池子按 URL 去重
      local result = {}
      for i = 1, N do
        table.insert(result, { type = "http", ip = "127.0.0.1", port = 18083, auth = "user" .. i })
      end
      return result
health:
  target: http://127.0.0.1:18082/     # 本地假目标，探测本身很快
  timeout: 2s
  concurrency: 300
selection:
  verify: false
  reuse_after: 30m
state:
  dir: /tmp/pg-e2e/cache-bench
```

## 实测

| 场景 | 并发 | QPS | 客户端 P50 | 客户端 P99 | 服务端每次发放 | 服务端 CPU |
| --- | --- | --- | --- | --- | --- | --- |
| 1,000 条池子，选择路径 | 8 | 2,305 | 3.4ms | 8.7ms | 2.8ms | – |
| 5,000 条池子，选择路径 | 8 | 634 | 12.5ms | 15.7ms | 12.5ms | **1.08 核 / 12 核** |
| 1,019 条池子，`verify: true`（判定新鲜） | 4 | 2,360 | 1.6ms | 3.7ms | 1.2ms | – |
| 1,019 条池子，冷启动到 ready | – | – | – | – | – | 15.0s |

启动那次（线上真目标，示例配置的 rola-ip + scdn）：

| 指标 | 实测 |
| --- | --- |
| 订阅源脚本 | `scdn` 0.9s / 20 条；`rola-ip` 3.3s / 1,000 条（截断 3,499） |
| 池子 | 1,019 条，**9 条存活**（0.88%） |
| `check_total` | `ok=9`、`fail=1010` → 探测成功率 **0.88%** |
| 冷启动到 ready | **15.0s** |
| 进程 | 13 线程、RSS 92MB、fd 22 |

## 这些数字说明了什么

**1. 写锁是 `/get` 的吞吐天花板。** 5,000 条时服务端每次发放 12.5ms，但整轮只
烧了 1.08 个核：12.5ms × 634/s ≈ 7.9 "请求秒/秒"，而 CPU 只有 1.08 核。也就是说
**约 1.7ms 在真正干活（克隆整个池子 + 筛选 + 挑选），其余 10.8ms 在排队等写锁**。
上限 ≈ 1 / 1.7ms ≈ 590 QPS，实测 634 QPS，对得上。池子涨到 10,000 条，持锁时间
再翻一倍，上限就掉到 300 QPS——而且加大并发没有用，只会让 P99 更长（32 并发时
1,000 条池子的 P99 从 8.7ms 涨到 39ms，QPS 反而没涨）。

原因在代码里很直白：`ProxyPool::select()` 持**写锁**，并且为了跑选择器先把整个
池子 `values().cloned()` 复制一遍（`src/pool.rs`）。读多写少的场景用了最重的锁。

**2. 健康探测有 99.1% 是白花的。** 1,019 条里只有 9 条能用，但每轮
`health.interval`（现在 5m）都会把 1,019 条全部重探一遍，而且每条探两个目标：
约 2,000 次请求，其中 99.1% 注定失败。每 5 分钟一次全量重探对死代理毫无意义——
它们不会自己活过来；真正需要频繁重探的只有"已知可用"的那一小撮。

**3. 落盘已经是节流的，但写发生在请求路径上。** `PERSIST_INTERVAL = 2s` 加上
tmp+rename 原子写（`src/state.rs`），一轮 5s 的压力测试里只写了 3 次（指标
`state_save_total` 可以核对）。但那次写是**同步**的 `std::fs::write`，跑在
`/get` 的异步任务里，池子大时会直接堵住 reactor 线程。

## 下一步（按证据排序）

| 优先级 | 改动 | 依据 |
| --- | --- | --- |
| P0 | `ArcSwap<PoolSnapshot>` 快照 + `Proxy` 运行事实用原子量 | 第 1 条：写锁把 `/get` 限制在 ~600 QPS（5,000 条），且只用 1 个核 |
| P0 | 每代理退避重探（`next_check_at`），只对可用代理保持高频 | 第 2 条：99.1% 的探测注定失败 |
| P1 | 采样打分（K=32）替代全量筛选 | 第 1 条里那 1.7ms 的代价值得再砍 |
| P1 | 落盘挪到 `spawn_blocking` / 专用写盘任务 | 第 3 条：同步 I/O 在请求路径上 |
| 不做 | `/get` 结果缓存 | 和「每次 `/get` 都轮换 + 都是刚验证过的」契约冲突 |
