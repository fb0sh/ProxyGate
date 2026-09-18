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
curl -sf http://127.0.0.1:8081/api/v1/get
http://user:pass@1.2.3.4:8080
curl -x "$(curl -sf http://127.0.0.1:8081/api/v1/get)" https://example.com

# 4. 也可以直接把网关当代理用
curl -x http://127.0.0.1:8080 https://example.com

# 5. 手册就是这个端点，agent 和人都能读
curl -s http://127.0.0.1:8081/help
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
docker run --rm -p 8080:8080 -p 8081:8081 \
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

1. `--config <path>`（或环境变量 `$PROXYGATE_CONFIG`）
2. `./config.yaml`
3. `~/.config/proxygate/config.yaml`

压缩包里带了一份 [`config.example.yaml`](config.example.yaml)，直接拿来起步。服务
已经跑起来时，同一份内容也能从 API 取：

```bash
curl -s http://127.0.0.1:8081/api/v1/config > config.yaml
```

> 示例配置**不含客户端认证**：它假设网关只在本机可达。要对外开放时在配置里加
> `gateway.auth: user:password`（或用 `PROXYGATE_CONFIG` 指向另一份含凭据的配置）。

| 配置项 | 默认值 | 含义 |
| --- | --- | --- |
| `server.proxy` | `127.0.0.1:8080` | HTTP 代理网关地址 |
| `server.api` | `127.0.0.1:8081` | REST API 地址；填 `same` 可与网关共用同一端口 |
| `subscribers` | `[]` | 代理来源，见下一节 |
| `refresh.interval` | `10m` | 拉取结果复用时⻓ |
| `refresh.timeout` | `20s` | 单个 subscriber 超时 |
| `health.targets` | Google 204 + `cn.bing.com` | **通过代理**去访问的探测目标，并发探测 |
| `health.require` | `any` | 目标全部要通（`all`）还是通一个就算（`any`） |
| `health.interval` | `30s` | 探测周期，同时也是健康结果缓存有效期 |
| `health.timeout` | `5s` | 单个代理的探测超时 |
| `health.concurrency` | `100` | 同时探测的代理数量 |
| `health.max_failures` | `3` | 原本可用的代理连续失败多少次后判死 |
| `selection.strategy` | `random` | `random` 或 `latency` |
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

> **开满内置来源后池子会变大**：全部 13 条订阅源（rola-ip 的 10 页 + 另外 3 个）一次
> 冷启动抓到约 5,200 条代理，一遍健康探测要几分钟（实测 5,157 条约 3.5 分钟，66 条存活）。
> `health.interval` 默认 30s，池子这么大时探测基本是连轴转的——想让它喘口气就把
> `health.interval` 加到 `10m`，或者给来源加 `limit` 少抓一点。

> **注意 `health.targets` 的语义**：探测请求是**通过代理**发出的，所以「你本机连不上
> Google」不是问题——要连上的是代理。默认两个目标里，Google 只有代理真的能出国才会
> 应答，`cn.bing.com` 则证明这条隧道不是对所有站点都坏。默认 `require: any`，通一个
> 就算可用；想只发放两边都通的代理就设成 `all`。

### 让 API 和代理共用一个端口

`server.api` 可以写成 `same`（也接受 `proxy`，或者直接把 `server.proxy` 的地址抄一遍）：

```yaml
server:
  proxy: 127.0.0.1:8080
  api: same          # REST API 和代理网关共用 127.0.0.1:8080
```

同一个监听端口上按**请求形状**分流：

| 收到的请求 | 判定 | 去向 |
| --- | --- | --- |
| `CONNECT host:443` | 代理请求 | HTTP 网关 |
| `GET http://host/path`（绝对形式） | 代理请求 | HTTP 网关 |
| `GET /api/v1/get`（原始形式） | API 请求 | REST API |

> **认证只覆盖代理请求。** `gateway.auth` 只拦代理请求，共用端口时 API 本身仍然是
> 开放的——谁连上这个端口都能读 `/api/v1/proxies`。所以共用端口只适合监听在受信任的
> 接口（默认就是 `127.0.0.1`）。服务启动时会为此打一条警告日志。要对外提供服务，就把 API 放回独立端口，或者用防火墙限制来源。

分端口和共端口的选择没有功能差别，纯粹看部署习惯：容器里映射一个端口更省事，本地开发
分开更好排查。

### Subscriber（代理来源）

四种：`builtin`（内置源）、`http`、`file`、`exec`。

`builtin` 指向代码里维护的**内置源目录**。不想逐个挑的话，一个总开关就够：

```yaml
builtin-subscribers: enabled      # 订阅目录里的每一个源（示例配置写的就是这行）
# disabled                        # 只用手写的 subscribers
```

`GET /api/v1/providers` 列出目录里的全部条目（端点、格式、分页范围、注意事项、文档地址）。
总开关默认 `disabled`（不写就不隐式联网），而示例配置里带的是 `enabled`。

也可以只挑一个源，或者单独调参——`builtin` 本质上就是一次 HTTP 拉取，所以支持与 `http`
相同的覆盖项，外加一个 `limit`：

```yaml
subscribers:
  - name: scdn-cn
    type: builtin
    provider: scdn
    url: https://proxy.scdn.io/api/get_proxy.php?protocol=http&count=20&country_code=CN
    format: json      # 默认取目录里的格式
    timeout: 20s      # 目录可以给某个源更长的超时
    limit: 200        # 最多保留多少个可用代理（0 = 不限）
```

`limit` 是给「一个源返回上万条」准备的：健康探测要把池子里每个代理都探一遍，16,000 条在
默认并发下就是十二分钟一轮。目录里对那个大源设了 1000 的默认上限，按返回顺序取（大列表
基本是按速度从快到慢排的），配置里可以自己改。

另外三种是通用的，其余情况用最后一个逃生口：

```yaml
subscribers:
  - name: provider-a
    type: http
    url: https://example.com/proxies.txt
    format: plaintext        # plaintext（默认）| json | clash
    timeout: 20s
    headers:
      Authorization: Bearer <token>

  - name: local
    type: file
    path: ./proxies.txt
    format: plaintext

  - name: weird-provider
    type: exec
    command: [python3, ./subscribers/example.py, --url, https://example.com/weird-api]
    env:
      API_TOKEN: "..."
```

subscriber 的唯一职责是产出代理 URL：`exec` 在 stdout 上一行一个。内置解析器覆盖纯文本
列表、JSON 接口（数组、裸 `host:port` 字符串、带 `ip`/`host`/`server` + `port` + 凭据的
对象，以及任意层数的包装，例如 `{"code":200,"data":{"proxies":["1.2.3.4:8080"]}}`）和
Clash / Clash.Meta 的 `proxies:` 列表。其他格式都交给脚本，见
[`subscribers/README.md`](subscribers/README.md) 和
[`subscribers/example.py`](subscribers/example.py)。

subscriber 返回体里的协议字段也认：`protocol`（字符串）、`protocols`（数组）、
`"socks4+socks5"` 这种拼接串都能识别。命名规则是：

| 返回体里写的 | 归一化为 | 原因 |
| --- | --- | --- |
| `http` / `https` / `ssl` | `http://` | 列表里的 https 指「这个代理能 CONNECT 到 HTTPS」，不是「对代理做 TLS」 |
| `socks5` / `socks5h` / `socks` | `socks5h://` | 让代理去解析域名：本机 DNS 被污染时，`socks5://`（本地解析）会把假 IP 交给代理，探测和实际使用都会失败 |
| `socks4` / 其它 | 丢弃 | 无法隧穿 |

同一行既写 `http` 又写 `socks5` 时优先 `http`。

接受的 URL 写法：

```text
http://1.2.3.4:8080          socks5://1.2.3.4:1080
user:pass@1.2.3.4:3128       socks5h://user:pass@[2001:db8::1]:1080
1.2.3.4:8080                 # 协议和端口都会补默认值
```

内置目录里的第一个源是 [proxy.scdn.io](https://proxy.scdn.io/api_docs.php)：它返回 JSON
包装、里面是裸 `host:port`，内置 `json` 格式可以直接读。因为返回体不带协议，这类条目一律
按 HTTP 代理处理（所以目录里请求 `protocol=http`）；想用它家的 `socks4`/`socks5` 端点得用
`exec` 包一层补上 `socks5://` 前缀。

关于这类免费池要有心理准备，下面两点正是健康探测存在的意义：**大部分条目是死的**，并且
相当一部分「支持 HTTPS」的其实在中间人劫持 TLS、拿自己的证书签发。ProxyGate 会拒绝这类
代理（`invalid peer certificate`）——会重新签发流量的代理不是你要的代理，而且客户端只要
校验证书也用不了它。

## 日志与进度

服务端没有 stdout 契约要保护，进度与结果都在 **stderr 日志**里（`RUST_LOG`
控制级别，默认 `info`）：

```console
$ proxygate
2026-09-18T10:51:02Z  INFO proxygate is listening proxy=127.0.0.1:8080 api=127.0.0.1:8081 ...
2026-09-18T10:51:02Z  INFO API documentation help=http://127.0.0.1:8081/help
2026-09-18T10:51:02Z  INFO fetching subscriber subscriber=scdn kind=builtin format=json
2026-09-18T10:51:05Z  INFO subscriber fetched subscriber=scdn found=20 rejected=0 skipped=0 elapsed_ms=2423
2026-09-18T10:54:28Z  INFO still downloading subscriber=freeproxy-gh kilobytes=1300 elapsed_ms=130000
2026-09-18T10:54:50Z  INFO proxygate is ready proxies=5157 alive=66
2026-09-18T10:55:01Z  INFO hand-out verification passed proxy=http://***:***@1.2.3.4:8080 elapsed_ms=312
```

* 每个订阅源一行：名字、拿到多少、跳过/拒绝/截断、耗时；慢下载每 10 秒报一次
  字节数（那个 2.5 MB 的 GitHub 列表不会看起来像卡死）。
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
| `GET /api/v1/providers` | 内置来源目录（端点、格式、分页范围、注意事项） |
| `GET /api/v1/config` | 带注释的示例配置，`text/yaml` |
| `POST /api/v1/refresh` | `202`：让后台立刻抓一轮订阅源 |
| `POST /api/v1/check` | `202`：让后台立刻重探一遍代理池 |
| `GET /help` | 面向 agent 与人的手册（就是 `SKILL.md` 原文，`text/markdown`） |
| `GET /api/v1/health` | `{"status": "initializing"\|"ok"\|"degraded"\|"empty", "ready": true, "proxies": {...}}` |
| `GET /` | 以上端点的索引 |

```console
$ curl http://127.0.0.1:8081/api/v1/get
http://user:pass@1.2.3.4:8080

$ curl -s http://127.0.0.1:8081/api/v1/health
{"status":"ok","version":"0.3.0","uptime_seconds":42,"generation":3,
 "strategy":"random","health_targets":["https://www.google.com/generate_204",
 "https://cn.bing.com/"],"health_require":"any",
 "ready":true,"initializing":false,"initialization_attempts":1,
 "initialization_error":null,
 "proxies":{"total":2,"alive":2,"dead":0}}
# 冷启动还没做完时 status 是 "initializing"，ready 是 false。
```

`server.api: same` 时把上面的 `8081` 换成 `8080` 即可，路径不变。

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
不等最慢的那个（`freeproxy-gh` 要四分钟）。中途 Ctrl-C 或断电，已经拿到的代理仍
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
不会让一轮探测时间翻倍）。`health.require` 决定结论：

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

- **`exec` subscriber 会执行任意命令。** `config.yaml` 属于可信输入，不要加载你不会当
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
cargo doc --no-deps --open   # 中文文档注释，docs.rs 上就是这个
cargo build --release
```

集成测试自己在进程内起手写的 HTTP / SOCKS5 上游和假目标服务器，所以不需要网络、也不依赖
外部程序：

| 文件 | 覆盖内容 |
| --- | --- |
| `tests/pool.rs` | 去重、使用状态持久化、健康阈值、state 往返 |
| `tests/selector.rs` | 轮换契约、reuse 窗口、重启后继续轮换 |
| `tests/subscriber.rs` | file/http/exec、三种格式、失败隔离 |
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
  subscriber.rs  builtin/http/file/exec subscriber 与内置解析器
  providers.rs   内置代理源目录（`GET /api/v1/providers` 输出它）
  pool.rs        池子：合并、健康写入、选择（轮次）
  checker.rs     健康探测 + 共享上游 client 缓存
  selector.rs    候选过滤与 random/latency 策略
  gateway.rs     HTTP 代理网关：CONNECT 隧道、转发、认证
  api.rs         axum REST API
  useragent.rs   内置 User-Agent 池（100 个桌面 UA）
  state.rs       state.json / cache.json、RFC 3339 时间戳
  error.rs       整个 crate 共用的错误类型
assets/          编进二进制的数据（User-Agent 池）
subscribers/     exec subscriber 契约 + example.py
tests/           集成测试（进程内假上游）
SKILL.md         面向 agent 与人的手册（`GET /help` 输出它）
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
- 多了第四种 subscriber `builtin` 和一个 `builtin-subscribers` 总开关：设计说明只写了
  http/file/exec，但「内置源目录」让 URL、格式和限流说明集中维护，配置里只写名字。它做的
  仍然只是一次 HTTP 拉取。
- 返回体里的协议字段会被归一化（上表），其中 `socks5` → `socks5h` 是刻意的：本机 DNS 被
  污染时，本地解析会把假地址交给代理。
- 每个源可以有默认 `limit`，因为一次拉上万条代理会让健康探测循环跑不完。
- `server.api` 支持 `same`，让 REST API 和代理网关共用一个端口（按请求形状分流）。
- 内置来源支持**分页**：URL 里写 `{page}`、目录里声明页数，`normalize` 会把它展开成
  每页一条订阅源，各自计数、各自失败。rola-ip 因此从 500 条变成全部 10 页 4,724 条。
- `serve` 先挂端口、再在后台初始化；未就绪时 REST API 返回 `503` + `Retry-After`，
  而不是假装池子是空的。
- 文档注释一律写成中文，docs.rs 展示的就是它。`SKILL.md` 保持英文，因为它面向
  agent，而且它就是 `GET /help` 的正文。

## 许可

[MIT](LICENSE)
