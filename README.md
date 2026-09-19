# ProxyGate

**中文** · [English](README.en.md)

把任意代理来源，变成一个统一、随时可用的代理池。

ProxyGate 从 HTTP 接口、本地文件或任意脚本里收集代理，统一归一化成
`scheme://user:pass@host:port`，探测哪些真的能用，然后通过 REST API 或一个
透明 HTTP 代理网关把它们发出去——客户端完全看不到上游地址和上游凭据。

```text
                Subscribers
              /      |       \
           HTTP     File     Exec
             \       |       /
              \      |      /
                 Proxy URL
                    ↓
                Normalizer
                    ↓
                 Pool
               ↙      ↘
          Checker    Selector
                       ↓
          ┌────────────┼────────────┐
          ↓            ↓            ↓
     REST API      Gateway
                                HTTP Proxy
```

代理一旦进入 Pool，后续任何环节都不再关心它最初是什么格式。

> Rust API 文档（**中文**）在 <https://docs.rs/proxygate>，源码里的文档注释就是它；
> 想在本机看：`cargo doc --no-deps --open`。

## 快速开始

**这是个服务端程序，没有命令行客户端**——跑起来之后一切都走 HTTP：

```bash
# 1. 准备配置（压缩包里就有一份带注释的；服务起来后也能拿到）
cp config.example.yaml config.yaml

# 2. 启动：无参数，读 $PROXYGATE_CONFIG 或 ./config.yaml
proxygate

# 3. 拿一个（已验证的）代理并用它
curl -sf http://127.0.0.1:8080/api/v1/get
http://user:pass@1.2.3.4:8080
curl -x "$(curl -sf http://127.0.0.1:8080/api/v1/get)" https://example.com

# 4. 网关和 REST API 在同一个端口上，请求形状不同而已
curl -x http://127.0.0.1:8080 https://example.com

# 5. 手册就是这个端点，agent 和人都能读
curl -s http://127.0.0.1:8080/help
```

注意 `curl -sf`：池子空的时候 `/get` 会返回 `503` 和一段说明正文，不加 `-f`
的话那段正文会被当成代理地址塞进 `-x`。

两层认证完全独立：客户端向 ProxyGate 认证，ProxyGate 向上游认证。

## 安装

```bash
cargo build --release
install -m755 target/release/proxygate ~/.local/bin/proxygate
```

或者用 Docker（见 [`Dockerfile`](Dockerfile)）：

```bash
docker build -t proxygate .
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/home/proxygate/config.yaml:ro" \
  -v proxygate-cache:/home/proxygate/.cache/proxygate \
  proxygate
```

构建需要 Rust 1.85+。没有数据库、没有 Redis、除 Tokio 外没有别的异步运行时。

不想自己编译就直接下载：打 `v*` 标签会触发 [Releases](https://github.com/fb0sh/ProxyGate/releases)
构建，三个平台的可执行文件都在那里：

| 平台 | 文件 |
| --- | --- |
| Linux amd64 | `proxygate-<版本>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS arm64 | `proxygate-<版本>-aarch64-apple-darwin.tar.gz` |
| Windows amd64 | `proxygate-<版本>-x86_64-pc-windows-msvc.zip` |

每个压缩包里是二进制加上两份 README、`SKILL.md`、`LICENSE` 和 `config.example.yaml`。
普通 push 也会在 [`build.yml`](.github/workflows/build.yml) 的运行结果里挂同样的包。
Windows 上缓存目录是 `%LOCALAPPDATA%\proxygate`（可用 `state.dir` 或
`$PROXYGATE_CACHE_DIR` 覆盖）。

## 配置

配置文件查找顺序：

1. `$PROXYGATE_CONFIG`
2. `./config.yaml`
3. `~/.config/proxygate/config.yaml`

压缩包里带了一份 [`config.example.yaml`](config.example.yaml)，也可以让二进制直接
吐一份（不需要服务在跑，这就是它不做成 HTTP 端点的原因）：

```bash
proxygate --example-config > config.yaml
```

> 示例配置**不含客户端认证**：它假设网关只在本机可达。要对外开放时在配置里加
> `gateway.auth: user:password`（或用 `PROXYGATE_CONFIG` 指向另一份含凭据的配置）。

| 配置项 | 默认值 | 含义 |
| --- | --- | --- |
| `server.listen` | `127.0.0.1:8080` | 唯一的监听地址：HTTP 代理网关与 REST API 共用它 |
| `subscribers` | `[]` | 代理来源（Lua 脚本），见下一节 |
| `refresh.interval` | `10m` | 拉取结果复用时⻓ |
| `refresh.timeout` | `20s` | 单个 subscriber 超时 |
| `health.targets` | Google 204 + `cn.bing.com` | **通过代理**去访问的探测目标，并发探测 |
| `health.require` | `any` | 目标全部要通（`all`）还是通一个就算（`any`） |
| `health.interval` | `5m` | **可用**代理的重探周期，同时也是健康结果缓存有效期（与 `refresh.interval` 无关：那个是多久重新跑一次订阅源脚本） |
| `health.timeout` | `3s` | 单个代理的探测超时 |
| `health.concurrency` | `300` | 同时探测的代理数量——首查速度的关键 |
| `health.max_failures` | `3` | 原本可用的代理连续失败多少次后判死 |
| `health.backoff_base` | `5s` | 失败后第一次重试的等待时间，之后每次翻倍 |
| `health.backoff_max` | `30m` | 失败退避的上限；也是"死代理复活多久被发现"的上限 |
| `selection.strategy` | `random` | `random`、`latency` 或 `score` |
| `selection.sample_size` | `32` | `score` 每次发放看多少个候选（`0` = 全部） |
| `selection.reuse_after` | `30m` | 优先避开这段时间内用过的代理 |
| `selection.verify` | `true` | 发放前现探选中的代理（判定够新则跳过） |
| `selection.max_age` | `60s` | 判定比这新就直接用；`0s` 表示每次都探 |
| `selection.verify_timeout` | `3s` | 现探单个候选的超时（比 `health.timeout` 短） |
| `selection.verify_attempts` | `3` | 现探失败后最多再试几个候选 |
| `gateway.retries` | `2` | 首个上游失败后的额外重试次数 |
| `gateway.connect_timeout` | `10s` | 连接上游并完成 CONNECT 握手的超时 |
| `gateway.auth` | – | 要求客户端提供的 `user:password` |
| `state.dir` | `~/.cache/proxygate` | `state.json` / `cache.json` 所在目录 |

时⻓支持 `30s`、`10m`、`2h`、`1d`、`250ms`、`1h30m` 或纯秒数。

> **首查慢是因为健康探测，不是因为脚本**：`rola-ip` 一个源就有 4,400 多条（10 页在
> 脚本里循环）。示例配置给它加了 `limit: 1000`：免费代理的存活率极低（实测一份 1,020
> 条的池子只有 13 条通过），与其花 5 倍时间去探 5,000 条、换来多得有限的可用代理，不如
> 把这 5 倍时间留给刷新。
>
> `health.concurrency` 就是决定首查要跑多久的那个旋钮：同一份 1,020 条的池子、同一组
> 真实目标，实测 300 并发 14s、100 并发 30s（差距取决于死代理多快失败，不只是并发倍数）。

> **注意 `health.targets` 的语义**：探测请求是**通过代理**发出的，所以「你本机连不上
> Google」不是问题——要连上的是代理。默认两个目标里，Google 只有代理真的能出国才会
> 应答，`cn.bing.com` 则证明这条隧道不是对所有站点都坏。默认 `require: any`，通一个
> 就算可用；想只发放两边都通的代理就设成 `all`。

### 一个端口

`server.listen` 只有一个地址，HTTP 代理网关和 REST API 共享它。两者不冲突，
因为请求形状不同：

| 收到的请求 | 判定 | 去向 |
| --- | --- | --- |
| `CONNECT host:443` | 代理请求 | HTTP 网关 |
| `GET http://host/path`（绝对形式） | 代理请求 | HTTP 网关 |
| `GET /api/v1/get`（原始形式） | API 请求 | REST API |

> **认证只覆盖代理请求。** `gateway.auth` 只拦代理请求，API 是与它同端口的，
> 因此始终开放——谁连上这个端口都能读 `/api/v1/proxies`。所以默认只监听
> `127.0.0.1`；要对外开放就把 `listen` 改成 `0.0.0.0:8080` 并自己加一层防火墙，
> 或者让 ProxyGate 只监听内网地址。

### Subscriber（代理来源）

每个订阅源就是**一段 Lua 脚本**：它自己决定去哪里取、怎么翻页、怎么拼装，最后
**返回一组代理表**（`type` / `ip` / `port` / `auth`）。配置里只写脚本：

```yaml
subscribers:
  - name: rola-ip
    timeout: 60s
    lua_code: |
      local page, pages = 1, 1
      local result = {}
      repeat
        local body = fetch_json("https://rola-ip.co/proxy-api/api/v1/proxies?page=" .. page .. "&pageSize=500")
        pages = (body.pagination and body.pagination.totalPages) or 1
        for _, item in ipairs(body.data or {}) do
          local scheme = nil
          for _, protocol in ipairs(item.protocols or {}) do
            local name = string.lower(protocol)
            if name == "http" or name == "https" then
              scheme = "http"
              break
            elseif name == "socks5" then
              scheme = "socks5h"
            end
          end
          if scheme then
            table.insert(result, {
              type = scheme,
              ip = item.ip,
              port = item.port,
              auth = item.auth or ""
            })
          end
        end
        page = page + 1
      until page > pages
      return result

  # 脚本也可以写在文件里，端点用额外键传进去
  - name: my_scraper
    timeout: 30s
    target_url: https://api.example.com/data.json   # 不是 ProxyGate 的键 -> 脚本全局变量
    token: "..."                                    # 同上
    limit: 500                                      # 最多保留多少个可用代理（0 = 不限）
    lua_file: ./scripts/my_scraper.lua
```

`limit` 是给「一个源返回上万条」准备的：健康探测要把池子里每个代理都探一遍，
上万条在默认并发下就是十几分钟一轮。

### 用 Lua 写 subscriber

脚本里能用的东西：

| 名称 | 说明 |
| --- | --- |
| `fetch(url)` | 发一次 GET，返回响应体字符串；非 2xx 抛错 |
| `fetch_json(url)` | 同上，但把响应体解码成 Lua 表 |
| `json_encode(v)` / `json_decode(s)` | Lua 值与 JSON 字符串互转 |
| `log(...)` / `print(...)` | 以 `info` 级别写进 ProxyGate 日志。**它们不是输出通道**，代理靠 `return` |

`return` 的每个条目是一个代理表，字段与取值规则：

| 字段 | 说明 |
| --- | --- |
| `type` | `http` / `https` / `ssl` → `http`；`socks5` / `socks5h` / `socks` → `socks5h`；`socks4` 或无法识别的名称会被**跳过** |
| `ip` | 主机名或 IP，`host` / `hostname` / `server` / `address` / `addr` 也可以；IPv6 会自动套方括号 |
| `port` | 端口，数字或字符串；不写就用协议的默认端口 |
| `auth` | 可选，`user:password`（也可以只写 `user`），会做百分号编码 |

`https` 归一成 `http`：列表里的 https 指「这个代理能 CONNECT 到 HTTPS」，不是
「对代理做 TLS」。`socks5` 归一成 `socks5h` 是刻意的：让代理去解析域名，本机 DNS
被污染时，`socks5://`（本地解析）会把假 IP 交给代理，探测和实际使用都会失败。
条目也可以直接写成字符串（`"socks5h://user:pass@1.2.3.4:1080"`、`"1.2.3.4:8080"`）；
既不是表也不是字符串、缺 `ip`、`port` 不是合法端口的条目会记进 `rejected`，不会
把整个来源判死。

脚本跑在**沙箱**里：不加载 `io`、`os`、`package`、`debug`，`dofile`、`loadfile`、
`load`、`require` 也被摘掉了，唯一的出口是 `fetch`。`timeout`（缺省 `refresh.timeout`）
限制整段脚本的墙钟时间，`while true do end` 也会被指令钩子掐断；每次刷新都新建一个
Lua 状态，脚本之间互不影响。`lua_code` 和 `lua_file` 二选一，两个都给或都不给都是
配置错误。

之所以把这件事交给脚本：分页、签名、字段拼装本来就是脚本的活，写进配置比写进
ProxyGate 更合适——加一个源不再需要发一个版本。

接受的 URL 写法：

```text
http://1.2.3.4:8080          socks5://1.2.3.4:1080
user:pass@1.2.3.4:3128       socks5h://user:pass@[2001:db8::1]:1080
1.2.3.4:8080                 # 协议和端口都会补默认值
```

示例配置里的第一个源是 [proxy.scdn.io](https://proxy.scdn.io/api_docs.php)：它返回 JSON
包装、里面是裸 `host:port`，内置 `json` 格式可以直接读。因为返回体不带协议，这类条目一律
按 HTTP 代理处理（所以示例里请求 `protocol=http`）；想用它家的 `socks4`/`socks5` 端点，
改成 `type: lua` 再 `print("socks5h://" .. item)` 就行——`rola-ip` 那段脚本就是这么写的。

关于这类免费池要有心理准备，下面两点正是健康探测存在的意义：**大部分条目是死的**，并且
相当一部分「支持 HTTPS」的其实在中间人劫持 TLS、拿自己的证书签发。ProxyGate 会拒绝这类
代理（`invalid peer certificate`）——会重新签发流量的代理不是你要的代理，而且客户端只要
校验证书也用不了它。

## 指标

`GET /metrics` 是 Prometheus 文本，和别的端点共用同一个端口：

| 指标 | 类型 | 标签 | 说明 |
| --- | --- | --- | --- |
| `pool_total` / `pool_healthy` | gauge | – | 抓取时刻的池子规模与健康数 |
| `check_total` | counter | `result=ok\|fail` | 健康探测成功/失败，**免费池子的成功率一眼可见** |
| `verify_total` | counter | `result=ok\|fail\|fresh` | 发放前验证：通过、失败、判定够新跳过 |
| `get_total` | counter | `strategy`、`result` | `/api/v1/get` 的分发次数 |
| `get_latency_seconds` | histogram | `strategy` | 一次发放的服务端耗时（含现探） |
| `subscriber_fetch_total` / `subscriber_proxies` | counter / histogram | `result` | 订阅源脚本跑得怎么样、一次贡献多少条 |
| `state_save_total` | counter | `result` | 轮换状态落盘次数 |
| `build_info` | gauge | `version` | 恒为 1，用来对齐版本 |

名字都带 `proxygate_` 前缀。判断"慢在哪"的关键是比较**客户端延迟**和
`get_latency_seconds`：两者差很多说明请求在排队，而不是在算。本机的实测基线、
复现命令和结论都在 [`BENCHMARKS.md`](BENCHMARKS.md)。

## 日志与进度

服务端没有 stdout 契约要保护，进度与结果都在 **stderr 日志**里（`RUST_LOG`
控制级别，默认 `info`）：

```console
$ proxygate
2026-09-18T10:51:02Z  INFO proxygate is listening listen=127.0.0.1:8080 config=Some("./config.yaml") proxies=0 alive=0 auth=false ready=false
2026-09-18T10:51:02Z  INFO API documentation help=http://127.0.0.1:8080/help
2026-09-18T10:51:02Z  INFO running subscriber script subscriber=rola-ip
2026-09-18T10:51:02Z  INFO running subscriber script subscriber=scdn
2026-09-18T10:51:04Z  INFO subscriber fetched subscriber=scdn found=20 rejected=0 skipped=0 truncated=0 elapsed_ms=1574
2026-09-18T10:51:37Z  INFO subscriber fetched subscriber=rola-ip found=4479 rejected=0 skipped=0 truncated=0 elapsed_ms=35700
2026-09-18T10:51:38Z  INFO hand-out verification passed proxy=http://***:***@1.2.3.4:8080 elapsed_ms=312
2026-09-18T10:55:10Z  INFO proxygate is ready proxies=4499 alive=61
```

* 每个订阅源两行：开始跑脚本，以及拿到多少、跳过/拒绝/截断、耗时。
* 健康探测每 5 秒报一次进度与存活数。
* 发放验证每次都会记一行（通过或失败+原因），所以"为什么这个代理没发出来"
  在日志里查得到。

## REST API

| 端点 | 返回 |
| --- | --- |
| `GET /api/v1/get` | 一个代理 URL，`text/plain` |
| `GET /api/v1/get?format=json` | `{"proxy": "...", "latency_ms": 83, "round": 3}` |
| `GET /api/v1/getua` | 一个 User-Agent，`text/plain` |
| `GET /api/v1/getua?format=json` | `{"user_agent": "Mozilla/5.0 ..."}` |
| `GET /api/v1/proxies` | 整个池的 JSON，凭据已脱敏 |
| `POST /api/v1/refresh` | `202`：让后台立刻抓一轮订阅源 |
| `POST /api/v1/check` | `202`：让后台立刻重探一遍代理池 |
| `GET /help` | 面向 agent 与人的手册（就是 `SKILL.md` 原文，`text/markdown`） |
| `GET /api/v1/health` | `{"status": "initializing"\|"ok"\|"degraded"\|"empty", "ready": true, "proxies": {...}}` |
| `GET /metrics` | Prometheus 文本：池子规模、探测成功率、发放延迟 |
| `GET /` | 以上端点的索引 |

```console
$ curl http://127.0.0.1:8080/api/v1/get
http://user:pass@1.2.3.4:8080

$ curl -s http://127.0.0.1:8080/api/v1/health
{"status":"ok","version":"0.5.0","uptime_seconds":42,"generation":3,
 "strategy":"random","health_targets":["https://www.google.com/generate_204",
 "https://cn.bing.com/"],"health_require":"any",
 "ready":true,"initializing":false,"initialization_attempts":1,
 "initialization_error":null,
 "proxies":{"total":2,"alive":2,"dead":0}}
# 冷启动还没做完时 status 是 "initializing"，ready 是 false。
```

API 与代理网关同端口，所以上面这些路径和 `curl -x` 用的是同一个地址。

`/get` 有两种 `503`，靠响应体区分：

| 情况 | 响应体 | 含义 |
| --- | --- | --- |
| 冷启动还没做完 | `proxygate: still initializing the proxy pool; retry in 5 seconds` | 还没准备好，`Retry-After: 5`；挂载端口后第一次抓取与探测还在跑 |
| 池里没有可用代理 | `proxygate: no healthy proxy available` | 已经初始化过了，只是当下确实没有能用（含义同退出码 `3`） |

收到第一种时重试即可：每次请求都会顺手推动后台立刻再试一次初始化，不用自己轮询。
`/health` 里有 `ready`、`initializing`、`initialization_attempts`、
`initialization_error` 四个字段，运维看这四个就够。

`serve` **不会**在启动时等抓取完成：它读完本地缓存就把端口挂上（几毫秒），抓取和探测
在后台跑，所以 `systemd`/`k8s` 的探针能立刻拿到 `503` 而不是连接被拒。缓存新鲜时初始
化是瞬间完成的，第一次请求就直接拿到代理。

`/proxies` 永远不会暴露凭据（替换成 `***:***`），但会带上每个代理的逐目标探测结果。

## 网关

服务进程在同一个 Tokio runtime 上只跑四件事：subscriber 刷新、健康探测、
REST API、HTTP 网关。

- **CONNECT**（HTTPS）会被变成一条字节隧道。上游是在客户端看到 `200` **之前**就选好、
  连上并完成握手的，所以坏上游可以被透明重试；一条隧道全程固定一个上游。
- **纯 HTTP** 转发时保留绝对形式请求目标，DNS 和建连交给上游。
- 客户端凭据（`gateway.auth`）由 ProxyGate 消费，绝不转发；上游凭据由 ProxyGate 补上，绝不
  暴露。

重试规则保守且安全：

| 请求 | 何时重试 |
| --- | --- |
| `CONNECT` | 任何失败（建连、握手、上游返回非 2xx） |
| `GET`/`HEAD` | 建连和超时错误 |
| 其他方法 | 从不——body 不能被发两次 |

每次失败都会给上游记一次失败；连续失败到 `health.max_failures` 次后，该上游退出轮换，
直到某次探测或请求成功才回来。

支持的上游：`http://`、`socks5://`（DNS 本地解析）、`socks5h://`（DNS 交给代理解析）。
`https://` 上游 **目前不支持**——它在加载列表时就被拒绝，而不是进池之后在 CONNECT 阶段
才失败。

### 谁负责探测

健康探测要连每一个代理（全开内置来源时几千个，一遍几分钟），所以它分成两条路：

| 谁 | 探测行为 |
| --- | --- |
| 后台探测循环 | 按 `health.interval` 周期重探整个池，并给 REST API 与网关提供判定 |
| `POST /api/v1/refresh` | 抓完之后**只探新抓到的**那些代理（老代理的判定还在有效期里） |
| `POST /api/v1/check` | 立刻重探整个池 |
| 发放验证 | `GET /api/v1/get` 选中的那个代理，如果判定比 `selection.max_age` 旧就先探一次（`selection.verify`，默认开） |

所以典型体验是：**发放时验证保证交出去的这个刚刚通过**（判定够新时毫秒级返回），
后台循环负责让整个池子的判定不过期。如果你把 `selection.verify` 关掉，就只剩后台
循环——那时判定可能几分钟旧，发出去死代理的概率会明显上升。

`refresh` 是**边抓边落盘**的：每个订阅源一完成就立刻合并进池并写 `cache.json`，
不等最慢的那个（`rola-ip` 要 35 秒）。中途 Ctrl-C 或断电，已经拿到的代理仍
然在磁盘上；因为整轮没有跑完，`fetched_at` 不会被更新，所以下次还会重新抓一遍补全。

## 选择与轮换规则

用户真正能感觉到的规则只有这几条：

```text
只从健康代理里选
      ↓
跳过本轮已经发放过的
      ↓
优先选 reuse_after（默认 30 分钟）内没用过的
      ↓
发放并记下这次使用
```

当所有健康代理都发放过之后，轮次**立即**加一——不会干等那 30 分钟：

```text
池里 A、B、C
GET /api/v1/get  →  A
GET /api/v1/get  →  B
GET /api/v1/get  →  C
GET /api/v1/get  →  进入下一轮，A/B/C 重新可用
```

轮次号和每个代理的最后使用时间会写进 `state.json`，所以跨进程、跨重启都接着轮。后加入的
代理（比如刷新时新发现的）在当前轮里算未用过，会优先被发放——新代理先被用上，而不是在池
里落灰。

## 健康探测

每个代理都会去访问全部 `health.targets`，同一代理的多个目标**并发**探测（所以多一个目标
不会让一轮探测时间翻倍），并发上限是 `health.concurrency`。探测期间**不持有任何池子锁**：
健康事实直接写进条目的原子量，`GET /api/v1/get` 照旧无锁读快照。

后台上不是"每轮全量重探"，而是**每个代理有自己的下一次探测时间**：可用的按
`health.interval`（默认 5m）保鲜，失败的按 `health.backoff_base` 起指数退避
（5s → 10s → 20s → 40s → … → `health.backoff_max`，默认 30m）。免费池子里 99% 的条目
是死的——实测一份 1,019 条的池子只有 9 条可用——按可用代理的节奏去重探它们纯属浪费
带宽和 fd。

算一下探测量（1,010 条死代理 + 9 条活代理）：

| 时间窗 | 旧版（每 5m 全量） | 现在 | 变化 |
| --- | --- | --- | --- |
| 头 5 分钟 | 1,019 次 | ~5,059 次 | **+4 倍**（10s/30s/70s/150s 各重试一轮） |
| 头 1 小时 | 12,228 次 | ~8,188 次 | **-33%** |
| 退避饱和之后（>1 小时） | 12,228 次/小时 | ~2,128 次/小时 | **-83%** |

前几分钟更勤快是**故意的**：新抓来的死代理值得多确认几次（万一是瞬时故障），
而确认"它就是死的"之后就不该再按分钟去骚扰它。`POST /api/v1/check` 不看退避，
立刻全量重探一遍。

`health.require` 决定结论：

| `require` | 判定为可用的条件 | 适用 |
| --- | --- | --- |
| `any`（默认） | 至少一个目标应答 | 池子不至于空；`TARGETS` 列告诉你它通哪边 |
| `all` | 每个目标都应答 | 只发放「你要的都能通」的代理 |

每个代理都会记住逐目标结果（`/api/v1/proxies` 的 `probes` 字段、
`/api/v1/proxies`、`/api/v1/health` 都有完整明细），并写入 `cache.json` 供重启复用，所以
「半通」的代理是可见的，而不是只能看到它不在池里。

单次探测的结论是权威的：

- 探测成功 → 代理可用，失败计数清零；
- **从未成功过的代理，第一次失败就判死**；
- 原本可用的代理，能容忍 `health.max_failures` 次连续探测失败，避免一次抖动就淘汰好
  上游；
- 网关另外单独统计自己请求的失败次数，达到同样阈值就把上游踢出轮换；之后任意一次成功
  探测或请求都能让它回来。

`alive`、`latency`、`failures` 从不被当作永久事实，但结果**会**按 `health.interval`
缓存：判定本身由后台循环按 `health.interval` 刷新，发放时再按
`selection.max_age` 决定要不要现探一次；subscriber 结果按 `refresh.interval` 复用。
换句话说，连续调 `/api/v1/get` 不会每次都去重探一万个代理，但也不会把几天前的判定
当事实发给你。

## 状态文件

两个文件都在缓存目录（`state.dir`，否则 `$PROXYGATE_CACHE_DIR`，否则
`~/.cache/proxygate`），目录权限 `0700`、文件 `0600`。

`state.json` 只存使用事实，不存任何「重新探测就能得到」的东西：

```json
{
  "generation": 13,
  "proxies": {
    "4f1c9a2e5b7d8031": { "generation": 13, "last_used_at": "2026-09-17T10:30:00Z" }
  }
}
```

只有发放过的代理才会有条目，所以文件大小跟「用过的代理数」成正比，而不是池子大小。

`cache.json` 存最近一次拉取结果和健康结果，各带时间戳：

```json
{
  "fetched_at": "2026-09-17T10:29:58Z",
  "proxies": ["http://user:pass@1.2.3.4:8080"],
  "checked_at": "2026-09-17T10:30:12Z",
  "health": {
    "4f1c9a2e5b7d8031": {
      "alive": true, "latency_ms": 82, "failures": 0,
      "targets": [
        { "target": "https://www.google.com/generate_204", "ok": false },
        { "target": "https://cn.bing.com/", "ok": true, "latency_ms": 82 }
      ]
    }
  }
}
```

`cache.json` 里的代理 URL **含明文凭据**（要重建池子就必须有），这也是目录权限收紧的原
因。两个文件删掉即可从零开始；`POST /api/v1/refresh` 会重建池子。

配置或缓存目录写不进去时，网关会**降级继续服务**（只打警告、不再持久化轮换状态），不会
因为一个只读卷就起不来。

## 安全注意

  脚本执行的配置。
- **REST API 自身没有认证**，默认只监听 `127.0.0.1`——把它暴露出去，等于把你的可用代理
  公开给所有能访问该端口的人。
- 把网关绑到公网前先设 `gateway.auth`。凭据用常量时间比较，
  `Proxy-Authorization` 在转发前会被剥掉。
- 代理凭据在 `/api/v1/proxies`、所有日志与错误信息里都脱敏；只有 `/api/v1/get`（它就
  是干这个的）和私有目录下的 `cache.json` 里会出现明文。

## 尚未包含

有意省略，为了让核心足够小：没有数据库/Redis、没有插件框架、没有限流和按客户端的配额、
不支持 `https://` 上游代理、不提供 SOCKS5 **服务端**（客户端说 HTTP 代理协议）、没有按
地区或供应商挑上游。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test              # 单元 + 集成（内置假上游，不依赖网络）
python3 scripts/loadtest.py        # 打 /api/v1/get，打印 P50/P95/P99 与指标差值
cargo doc --no-deps --open   # 中文文档注释，docs.rs 上就是这个
cargo build --release
```

集成测试自己在进程内起手写的 HTTP / SOCKS5 上游和假目标服务器，所以不需要网络、也不依赖
外部程序：

| 文件 | 覆盖内容 |
| --- | --- |
| `tests/pool.rs` | 去重、使用状态持久化、健康阈值、state 往返 |
| `tests/selector.rs` | 轮换契约、reuse 窗口、重启后继续轮换 |
| `tests/lua_subscriber.rs` | Lua 脚本：返回值、参数全局变量、翻页、凭据、沙箱、超时、失败隔离 |
| `tests/gateway.rs` | CONNECT、纯 HTTP、认证、重试、SOCKS5 及 SOCKS5 认证 |

## 目录结构

这是一个**库 crate + 一个只负责启动服务的二进制**：`src/lib.rs` 装全部逻辑，
`src/main.rs` 只有几十行（认 `--version`/`--help`、初始化日志、调 `server::run`）。
没有子命令、没有客户端命令——对外的一切都在 HTTP 上。想在自己的 Rust 程序里用
`ProxyGate`，加依赖后 `use proxygate::...` 即可。

```text
src/
  lib.rs         库入口：模块声明、crate 文档、内置 SKILL.md
  main.rs        薄壳：--version/--help、初始化日志、启动服务
  server.rs      服务端：绑端口、起网关/API/后台循环、处理 Ctrl-C
  app.rs         共享运行时：池 + 状态存储 + HTTP client + 健康检查器 + 发放验证
  progress.rs    抓取/探测/发放验证的进度事件
  config.rs      config.yaml 模型、默认值、校验
  model.rs       Proxy、稳定 ID、URL 归一化、小工具编解码
  subscriber.rs  订阅源脚本的执行、返回值转换、Lua 沙箱
  pool.rs        池子：ArcSwap 不可变快照（无锁读）、合并、健康写入、轮换选择
  checker.rs     健康探测 + 共享上游 client 缓存
  selector.rs    候选过滤与 random/latency/score 策略（含采样打分）
  gateway.rs     HTTP 代理网关：CONNECT 隧道、转发、认证
  api.rs         axum REST API
  useragent.rs   内置 User-Agent 池（100 个桌面 UA）
  state.rs       state.json / cache.json、RFC 3339 时间戳
  error.rs       整个 crate 共用的错误类型
assets/          编进二进制的数据（User-Agent 池）
scripts/         开发用脚本（loadtest.py：/get 的延迟与指标基线）
tests/           集成测试（进程内假上游）
SKILL.md         面向 agent 与人的手册（`GET /help` 输出它）
BENCHMARKS.md    性能基线与复现命令
```

拆成库之后集成测试能直接驱动真实网关（`tests/common/mod.rs` 放那些假上游），不用起子
进程。除此之外还有几处与设计说明不同，都是实际做的时候发现问题才改的：

- 包名小写 `proxygate`，让二进制名和文档里的命令一致。
- `state.json` 与设计一致，但额外有 `cache.json` 存 subscriber 和健康结果。没有它，
  一万个代理时每次 `get` 都要重探全池。
- `health.targets` 是列表、并发探测，`health.require` 选择 `any`（默认）/`all`；设计
  说明里只有一个 `target`。默认目标是 Google 加 `cn.bing.com`。
- **从未成功过的代理一次失败即判死**，`health.max_failures` 只对原本可用的代理生效。
- `https://` 上游在加载列表时就被拒绝，而不是接受之后在 CONNECT 阶段失败。
- subscriber 只剩一种：Lua 脚本。设计说明写了 http/file/exec 三种，但每个来源的差别
  本来就在「怎么取、怎么拼」上——那正是脚本擅长的事，写进配置比给每个源加一个 Rust 分支
  好维护，也不必为了一个新来源发版。脚本跑在沙箱里，唯一的出口是 `fetch`。
- 脚本的协议字段会被归一化（上表），其中 `socks5` → `socks5h` 是刻意的：本机 DNS 被污染
  时，本地解析会把假地址交给代理。
- 每个源都能写 `limit`，因为一次拉上万条代理会让健康探测循环跑不完（`0` = 不限）。
- `server.listen` 只有一个地址，REST API 和代理网关共用它（按请求形状分流）。分成两个
  端口时 API 的认证问题并不存在，但同一个进程对外只开一个口更省事，容器里也少一个映射。
- `serve` 先挂端口、再在后台初始化；未就绪时 REST API 返回 `503` + `Retry-After`，
  而不是假装池子是空的。
- 文档注释一律写成中文，docs.rs 展示的就是它。`SKILL.md` 保持英文，因为它面向
  agent，而且它就是 `GET /help` 的正文。

## 许可

[MIT](LICENSE)
