# ProxyGate

[中文文档](README.md) · **English**

Turn any proxy source into a uniform, always-ready proxy pool.

ProxyGate collects proxies from HTTP endpoints, local files or arbitrary scripts,
normalizes whatever it finds into `scheme://user:pass@host:port`, checks which
ones actually work, and then hands them out — through a REST API, or as a
transparent HTTP proxy gateway that hides the upstream (and its credentials)
from your clients.

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

From the moment a proxy enters the pool, nothing cares where it came from.

> The Rust API docs (**Chinese**) live at <https://docs.rs/proxygate> — the doc
> comments in the source are what is published. Locally:
> `cargo doc --no-deps --open`.

## Quick start

**This is a server. There is no command-line client** — once it runs, everything
happens over HTTP:

```bash
# 1. A config (the archives ship one; the server can also hand you its own).
cp config.example.yaml config.yaml

# 2. Start it: no arguments, reads $PROXYGATE_CONFIG or ./config.yaml.
proxygate

# 3. Get a (verified) proxy and use it.
curl -sf http://127.0.0.1:8080/api/v1/get
http://user:pass@1.2.3.4:8080
curl -x "$(curl -sf http://127.0.0.1:8080/api/v1/get)" https://example.com

# 4. Or just point your client at the gateway port.
curl -x http://127.0.0.1:8080 https://example.com

# 5. The manual is an endpoint, readable by agents and humans alike.
curl -s http://127.0.0.1:8080/help
```

Mind the `-f` in `curl -sf`: when the pool has nothing to give, `/get` answers
`503` with an explanatory body, and without `-f` that body would be pasted into
`-x` as if it were a proxy address.

## Install

```bash
cargo build --release
install -m755 target/release/proxygate ~/.local/bin/proxygate   # starts the server
```

Or with Docker (see [`Dockerfile`](Dockerfile)):

```bash
docker build -t proxygate .
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/home/proxygate/config.yaml:ro" \
  -v proxygate-cache:/home/proxygate/.cache/proxygate \
  proxygate
```

Requirements: Rust 1.85+ to build. No database, no Redis, no async runtime
beyond Tokio.

Or download a build: pushing a `v*` tag triggers the
[Releases](https://github.com/fb0sh/ProxyGate/releases) workflow, which attaches
a ready-to-run archive for each platform:

| Platform | File |
| --- | --- |
| Linux amd64 | `proxygate-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS arm64 | `proxygate-<version>-aarch64-apple-darwin.tar.gz` |
| Windows amd64 | `proxygate-<version>-x86_64-pc-windows-msvc.zip` |

Each archive holds the binary plus both READMEs, `SKILL.md`, `LICENSE` and
`config.example.yaml`. Ordinary pushes attach the same archives to the
[`build.yml`](.github/workflows/build.yml) run instead. On Windows the cache
directory is `%LOCALAPPDATA%\proxygate` (override with `state.dir` or
`$PROXYGATE_CACHE_DIR`).

## Configuration

ProxyGate looks for a config file in this order:

1. `--config <path>` (or `$PROXYGATE_CONFIG`)
2. `./config.yaml`
3. `~/.config/proxygate/config.yaml`

With no config file it starts with an empty pool, which is useful for testing
but not much else. Start from [`config.example.yaml`](config.example.yaml).

> The generated config carries **no client authentication**: it assumes the
> gateway stays reachable only from this machine. Set `gateway.auth` when you
> expose it — keeping credentials in a file you do not commit.

| Key                   | Default                                   | Meaning                                              |
| --------------------- | ----------------------------------------- | ---------------------------------------------------- |
| `server.listen`       | `127.0.0.1:8080`                          | the only listen address: the gateway and the REST API share it |
| `subscribers`         | `[]`                                      | Where proxies come from (see below)                   |
| `refresh.interval`    | `10m`                                     | How often the subscriber scripts run and pull in new proxies |
| `refresh.timeout`     | `20s`                                     | Per-subscriber timeout                                |
| `health.targets`      | Google 204 + `cn.bing.com`                | URLs fetched *through* each proxy, probed concurrently |
| `health.require`      | `any`                                     | `any` target may answer, or `all` of them must          |
| `health.interval`     | `5m`                                      | How often a *working* proxy is re-probed, and the verdict cache lifetime |
| `health.timeout`      | `3s`                                      | Per-proxy probe timeout                               |
| `health.concurrency`  | `300`                                     | Proxies probed in parallel — what decides how long the first check takes |
| `health.max_failures` | `3`                                       | Consecutive failures tolerated for a working proxy    |
| `health.backoff_base` | `5s`                                      | First retry delay for a failing proxy; doubles each time |
| `health.backoff_max`  | `30m`                                     | Ceiling for that backoff — also the worst case for noticing a revival |
| `selection.strategy`  | `random`                                  | `random`, `latency` or `score`                        |
| `selection.sample_size` | `32`                                    | How many candidates `score` looks at (0 = all)        |
| `selection.reuse_after` | `30m`                                   | Prefer proxies unused in this window                  |
| `selection.verify`    | `true`                                    | Probe the chosen proxy before handing it out          |
| `selection.max_age`   | `60s`                                     | Use its verdict if newer than this; `0s` = always probe |
| `selection.verify_timeout` | `3s`                                 | Per-candidate timeout while verifying                 |
| `selection.verify_attempts` | `3`                                 | How many candidates to try before giving up           |
| `gateway.retries`     | `2`                                       | Extra upstream attempts after the first failure       |
| `gateway.connect_timeout` | `10s`                                 | Upstream connect + CONNECT handshake timeout       |
| `gateway.auth`        | –                                         | `user:password` required from clients                 |
| `state.dir`           | `~/.cache/proxygate`                      | Where `state.json` and `cache.json` live              |

Durations accept `30s`, `10m`, `2h`, `1d`, `250ms`, `1h30m`, or plain seconds.

Note that the probe runs *through the proxy*, so a target you cannot reach
directly is not a problem — it is the proxy that has to get there. That is the
whole point of the default pair: `google.com/generate_204` only answers if the
proxy really has international connectivity, and `cn.bing.com` proves the tunnel
is not broken for everything else. By default `require: any` accepts a proxy
that reaches either one; the `TARGETS` column tells you which.

> **The slow part is the health check, not the scripts.** `rola-ip` alone
> returns 4,400+ proxies (its ten pages are one loop inside the script), which is
> why the example caps it with `limit: 1000`: among free proxies the survivors
> are scarce (one measured pool of 1,020 had 13 working), so probing 5,000 of them
> buys five times the waiting for a limited gain in usable proxies. The five times
> are better spent on refreshing.
>
> `health.concurrency` is the knob that decides how long that first check takes:
> on the same 1,020-proxy pool against the real targets, 300 in flight finished
> in 14s and 100 in 30s (the gap depends on how quickly dead proxies fail, not
> only on the parallelism).

### One port

`server.listen` is a single address; the HTTP proxy gateway and the REST API
share it. They never collide, because the request shapes differ:

| Request received                  | Verdict       | Destination |
| --------------------------------- | ------------- | ----------- |
| `CONNECT host:443`                | proxy request | HTTP gateway |
| `GET http://host/path` (absolute) | proxy request | HTTP gateway |
| `GET /api/v1/get` (origin-form)   | API request   | REST API     |

> **Auth covers proxy requests only.** `gateway.auth` rejects proxy requests,
> but the API on the same port stays open — anyone who can reach the port can
> read `/api/v1/proxies`. That is why the default only listens on `127.0.0.1`;
> to expose it, set `listen: 0.0.0.0:8080` and put your own access control in
> front of it.

### Subscribers

A subscriber is a Lua script. It fetches whatever it wants and **returns a list
of proxy tables** — one per proxy, with `type`, `ip`, `port` and an optional
`auth`:

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

  # A script can live in its own file, with the endpoint passed in as a global.
  - name: my_scraper
    timeout: 30s
    target_url: https://api.example.com/data.json   # not a ProxyGate key -> script global
    token: "..."
    limit: 500                                      # keep at most this many (0 = no cap)
    lua_file: ./scripts/my_scraper.lua
```

`limit` exists because the health checker probes every proxy it is given: tens of
thousands of them is a very long pass at the default concurrency.

#### Writing a subscriber

What the script can use:

| Name | Meaning |
| ---- | ------- |
| `fetch(url)` | one GET, returns the body as a string; raises on a non-2xx status |
| `fetch_json(url)` | same, but decodes the body into a Lua table |
| `json_encode(v)` / `json_decode(s)` | Lua value <-> JSON string |
| `log(...)` / `print(...)` | writes to ProxyGate's log at `info`; **not** a way to emit proxies |

Every key ProxyGate does not recognise (`name`/`script_name`, `lua_code`,
`lua_file`, `timeout`, `limit`, `enabled`) becomes a global in the script — that
is how a script gets its parameters. Each returned table is read like this:

| Field | Meaning |
| ----- | ------- |
| `type` | `http`/`https`/`ssl` become `http`; `socks5`/`socks5h`/`socks` become `socks5h`; `socks4` or an unknown name is **skipped** |
| `ip` | hostname or IP; `host`, `hostname`, `server`, `address` and `addr` work too, and IPv6 gets bracketed |
| `port` | number or string; omitted uses the scheme's default port |
| `auth` | optional `user:password` (or just `user`), percent-encoded |

In a list, "https" means the proxy can CONNECT to HTTPS, not TLS-to-proxy, so it
becomes `http`. `socks5` becoming `socks5h` is deliberate: the proxy resolves
names, and with poisoned local DNS a locally resolved address would be handed
over as-is. An entry can also be a plain string (`"1.2.3.4:8080"`); entries that
are neither, lack an `ip`, or carry an unusable `port` are counted in `rejected`
without killing the source.

The script runs in a **sandbox**: `io`, `os`, `package` and `debug` are not
loaded, and `dofile`, `loadfile`, `load` and `require` are removed, so `fetch` is
the only way out. `timeout` (or `refresh.timeout`) bounds the whole script,
including `while true do end`. Every refresh builds a fresh Lua state, so scripts
cannot see each other. `lua_code` and `lua_file` are mutually exclusive.

This is what replaced the built-in source catalog, its `builtin` kind and the
`http`/`file`/`exec` kinds before it: paging, signing and field reshaping were
always a script's job, and keeping them in the config means adding a source no
longer needs a ProxyGate release.

The script's `type` is understood in the shapes real lists use: a string, or an
array such as `["http", "socks5"]`. The mapping:

| The script says | Becomes | Why |
| ---------------- | ------- | --- |
| `http` / `https` / `ssl` | `http://` | in a list, "https" means the proxy can CONNECT to HTTPS, not TLS-to-proxy |
| `socks5` / `socks5h` / `socks` | `socks5h://` | the proxy resolves names: with poisoned local DNS, `socks5://` hands the proxy a bogus address and both the probe and real use fail |
| `socks4`, anything else | dropped | cannot be tunneled |

An entry offering both picks `http`.

A string entry goes through the same normalizer as everything else, so these all
work:

```text
http://1.2.3.4:8080          socks5://1.2.3.4:1080
user:pass@1.2.3.4:3128       socks5h://user:pass@[2001:db8::1]:1080
1.2.3.4:8080                 # scheme and port get sensible defaults
```

The first source in the example config is
[proxy.scdn.io](https://proxy.scdn.io/api_docs.php): it answers with a JSON
envelope holding bare `host:port` entries, so the script asks for
`protocol=http` and turns each string into an HTTP proxy.

Two things to expect from free lists like that one, both of which the health
check is designed to surface: most entries are simply dead, and a fair share of
the "HTTPS-capable" ones intercept TLS and present a certificate signed by
themselves. ProxyGate rejects those (`invalid peer certificate`) — a proxy that
re-signs traffic is not a proxy you want, and a client that verifies
certificates could not use it anyway.

## Metrics

`GET /metrics` serves Prometheus text on the same port as everything else. Names carry
a `proxygate_` prefix:

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `pool_total` / `pool_healthy` | gauge | – | pool size and healthy count, read at scrape time |
| `check_total` | counter | `result=ok\|fail` | health probes — the probe success ratio of a free pool, in one number |
| `verify_total` | counter | `result=ok\|fail\|fresh` | pre-hand-out verification: passed, failed, or skipped because the verdict was fresh |
| `get_total` | counter | `strategy`, `result` | hand-outs |
| `get_latency_seconds` | histogram | `strategy` | server-side time to hand out one proxy, including the probe |
| `subscriber_fetch_total` / `subscriber_proxies` | counter / histogram | `result` | how the subscriber scripts went |
| `state_save_total` | counter | `result` | rotation-state writes |
| `build_info` | gauge | `version` | always 1; useful for aligning versions |

Compare your client latency with `get_latency_seconds`: a large gap means requests are
queueing rather than computing. The measured baseline for this machine, with the exact
commands to reproduce it and what it implies, is in [`BENCHMARKS.md`](BENCHMARKS.md).

## Logs and progress

There is no stdout contract to protect, so progress and results go to the
**stderr log** (`RUST_LOG`, default `info`):

```console
$ proxygate
INFO proxygate is listening listen=127.0.0.1:8080 config=Some("./config.yaml") proxies=0 alive=0 auth=false ready=false
INFO API documentation help=http://127.0.0.1:8080/help
INFO running subscriber script subscriber=rola-ip
INFO subscriber fetched subscriber=scdn found=20 rejected=0 skipped=0 elapsed_ms=2423
INFO proxygate is ready proxies=5157 alive=66
INFO hand-out verification passed proxy=http://***:***@1.2.3.4:8080 elapsed_ms=312
```

One line per subscriber with its counts and elapsed time, a byte count every ten
seconds for slow downloads, probe progress every five seconds, and one line per
hand-out verification — so "why was this proxy not handed out" is answerable
from the log.

## REST API

| Endpoint                        | Returns                                                     |
| ------------------------------- | ----------------------------------------------------------- |
| `GET /api/v1/get`               | one proxy URL as `text/plain`                                |
| `GET /api/v1/get?format=json`   | `{"proxy": "...", "latency_ms": 83, "round": 3}`             |
| `GET /api/v1/getua`             | one random user agent as `text/plain`                         |
| `GET /api/v1/getua?format=json` | `{"user_agent": "Mozilla/5.0 ..."}`                          |
| `GET /api/v1/proxies`           | the pool as JSON, credentials masked                         |
| `POST /api/v1/refresh`          | `202` — fetch every subscriber now                           |
| `POST /api/v1/check`            | `202` — probe the whole pool now                             |
| `GET /help`                     | this project's manual (`SKILL.md`), as `text/markdown`       |
| `GET /api/v1/health`            | `{"status": "ok", "proxies": {"total": 2, "alive": 2, ...}}` |
| `GET /metrics`                  | Prometheus text: pool size, probe success ratio, hand-out latency |
| `GET /`                         | a small index of the above                                   |

```console
$ curl http://127.0.0.1:8080/api/v1/get
http://user:pass@1.2.3.4:8080

$ curl -s http://127.0.0.1:8080/api/v1/health
{"status":"ok","version":"0.1.0","uptime_seconds":42,"generation":3,
 "strategy":"random","health_targets":["https://cn.bing.com/"],
 "health_require":"any","ready":true,"initializing":false,
 "initialization_attempts":1,"initialization_error":null,
 "proxies":{"total":2,"alive":2,"dead":0}}
# While the first pass runs, `status` is "initializing" and `ready` is false.
```

The API and the gateway share a port, so the examples above and `curl -x` use
the same address. The paths
are unchanged.

`/get` has two distinct `503`s, told apart by the body:

| Situation | Body | Meaning |
| --- | --- | --- |
| The first pass is still running | `proxygate: still initializing the proxy pool; retry in 5 seconds` | not ready yet; carries `Retry-After: 5` |
| The pool has nothing healthy | `proxygate: no healthy proxy available` | initialization finished, there is just nothing usable (same as exit code `3`) |

Just retry the first one: every request also nudges the background task to try
again, so there is no need to poll. `/health` carries `ready`, `initializing`,
`initialization_attempts` and `initialization_error` for monitoring.

`serve` does **not** wait for the first fetch: it restores the local cache, binds
the port in milliseconds, and does the fetching and probing in the background, so
a `systemd`/k8s probe gets a `503` instead of a refused connection. With a fresh
cache initialization is instant and the very first request is served.

Credentials are never exposed by `/proxies` (they are replaced with `***:***`).

## Gateway

The server runs four things on one Tokio runtime: subscriber refresh,
health checking, the REST API and the HTTP proxy gateway.

* **CONNECT** (HTTPS) becomes a byte tunnel. The upstream is chosen, dialled and
  handshaked *before* the client sees `200`, so a broken upstream is retried
  transparently. One tunnel is pinned to one upstream for its whole lifetime.
* **Plain HTTP** is forwarded with the absolute request target preserved, so the
  upstream does the DNS and the connecting.
* Client credentials (`gateway.auth`) are consumed by ProxyGate and never forwarded;
  upstream credentials are added by ProxyGate and never exposed.

Retries follow the boring, safe rule:

| Request     | Retried on                                             |
| ----------- | ------------------------------------------------------ |
| `CONNECT`   | any failure (connect, handshake, non-2xx from upstream) |
| `GET`/`HEAD`| connect and timeout errors                              |
| anything else | never — a body must not be sent twice                 |

Every failed attempt bumps the upstream's failure counter; after
`health.max_failures` in a row, that upstream drops out of rotation until a
probe or a request succeeds.

Supported upstreams: `http://`, `socks5://` (DNS resolved locally) and
`socks5h://` (DNS resolved by the proxy). `https://` upstreams are **not**
supported yet — they are rejected when the list is loaded rather than
silently entering the pool as proxies that cannot serve CONNECT.

### Who runs the health check

Probing connects to every proxy (thousands with all built-ins enabled, minutes
per pass), so it is split in two:

| Who | Probing behaviour |
| --- | --- |
| the background loop | re-probes the whole pool on `health.interval` and feeds both the REST API and the gateway |
| `POST /api/v1/refresh` | only the proxies it just fetched (the others still have a valid verdict) |
| `POST /api/v1/check` | the whole pool, right now |
| hand-out verification | the one proxy `/api/v1/get` picked, when its verdict is older than `selection.max_age` (`selection.verify`, on by default) |

So the experience is: a handed-out proxy is one that just passed, and the
background loop keeps the pool's verdicts from going stale. With
`selection.verify: false` only the loop remains — verdicts can then be minutes
old and dead proxies do get handed out.

`refresh` also persists **as it goes**: every subscriber is merged into the pool
and written to `cache.json` the moment it finishes instead of waiting for the
slowest one (`rola-ip` takes 35 seconds). A Ctrl-C or a power cut keeps
whatever was already fetched; because the pass never completed, `fetched_at` is
not updated and the next run fetches again to fill in the rest.

## How selection works

The rule users actually feel:

```text
healthy proxies
      ↓
skip the ones already handed out in this round
      ↓
prefer the ones unused within selection.reuse_after  (default 30m)
      ↓
hand one out and remember it
```

When every healthy proxy has been handed out, the round increments
**immediately** — there is no waiting for the 30 minutes to expire:

```text
A, B, C in the pool
GET /api/v1/get  →  A
GET /api/v1/get  →  B
GET /api/v1/get  →  C
GET /api/v1/get  →  round 2 starts, A/B/C are all fair game again
```

`state.json` persists the round number and each proxy's last use, so the
rotation continues across requests and restarts. A proxy that is added
later (a refresh found a new one) is unused in the current round and therefore
handed out first — new proxies get exercised instead of gathering dust.

## Health checking

For every proxy the checker asks three questions: can a request be made through
it, did the request succeed, and how long did it take. Concurrency is bounded by
a semaphore (`health.concurrency`), and no lock is held while a request is in
flight: the checker works on a snapshot and writes the results straight into the
entries' atomics, so `GET /api/v1/get` keeps reading the pool without waiting for
it.

The background loop is not a fixed full sweep either: **every proxy carries its
own next-check time**. A working proxy is re-probed every `health.interval`
(5m); a failing one backs off from `health.backoff_base` (5s → 10s → 20s → 40s …
up to `health.backoff_max`, 30m). Free pools run ~99% dead — one measured pool
had 9 working proxies out of 1,019 — so re-probing them at the healthy cadence is
bandwidth and file descriptors spent on nothing.

The arithmetic for that pool (1,010 dead + 9 working):

| Window | Before (full sweep every 5m) | Now | Change |
| --- | --- | --- | --- |
| First 5 minutes | 1,019 probes | ~5,059 | **4x more** (a round at 10s, 30s, 70s, 150s) |
| First hour | 12,228 | ~8,188 | **-33%** |
| After the backoff saturates (>1h) | 12,228/hour | ~2,128/hour | **-83%** |

The extra probing up front is deliberate: a freshly fetched dead proxy is worth
confirming a few times in case the failure was transient, and once it is clearly
dead it should not be poked every few minutes. `POST /api/v1/check` ignores the
schedule and sweeps everything now.

Every target is probed through the proxy, and the targets of one proxy are
probed **concurrently**, so a second endpoint costs no extra wall clock time.
`health.require` decides what the results mean:

| `require` | a proxy is alive when            | use it for                                            |
| --------- | -------------------------------- | ----------------------------------------------------- |
| `any` (default) | at least one target answered | a pool that stays usable; `TARGETS` shows what each proxy reaches |
| `all`     | every target answered            | only hand out proxies that reach everything you need   |

`any` is the default because an empty pool is worse than an imperfect one — on a
network where one of the targets is hard to reach, `all` can reject every proxy
you have. Pick `all` when "cannot reach X" makes a proxy useless to you.

Each proxy remembers the per-target outcome (`list` shows it as `2/2`,
`list --json` and `/api/v1/proxies` carry the full breakdown), so a pool of
half-working proxies is visible instead of merely absent.

A probe verdict is authoritative:

* a probe that succeeds makes a proxy alive and resets its failure counter;
* a proxy that **never** answered is dead after its first failure;
* a proxy that was working survives up to `health.max_failures` consecutive
  probe failures, so one flaky timeout does not evict a good upstream;
* the gateway independently counts its own request failures and drops an
  upstream after the same threshold; a later success revives it.

`alive`, `latency` and `failures` are never treated as permanent facts — but the
last result *is* cached: the background loop refreshes it on `health.interval`,
and a hand-out re-checks the chosen proxy only when its verdict is older than
`selection.max_age`. So a burst of `/api/v1/get` calls does not re-probe a
10,000-proxy pool every time, and it does not hand out week-old verdicts either.
Subscriber output is reused for `refresh.interval`.

## State files

Both files live in the cache directory (`state.dir`, else
`$PROXYGATE_CACHE_DIR`, else `~/.cache/proxygate`), which is created `0700` with
`0600` files.

`state.json` — only usage facts, nothing that a fresh check could not re-derive:

```json
{
  "generation": 13,
  "proxies": {
    "4f1c9a2e5b7d8031": { "generation": 13, "last_used_at": "2026-09-17T10:30:00Z" }
  }
}
```

`cache.json` — the last subscriber payload and health results, each with a
timestamp:

```json
{
  "fetched_at": "2026-09-17T10:29:58Z",
  "proxies": ["http://user:pass@1.2.3.4:8080"],
  "checked_at": "2026-09-17T10:30:12Z",
  "health": { "4f1c9a2e5b7d8031": { "alive": true, "latency_ms": 82, "failures": 0 } }
}
```

`cache.json` contains proxy URLs **including credentials** in plaintext (it has
to, to rebuild the pool), which is why the directory is private. Delete both
files to start from scratch; `POST /api/v1/refresh` rebuilds the pool.

## Security notes

* **The config file is trusted input.** A subscriber script runs with the
  server's privileges and can reach the network through `fetch`;
  do not load a config you would not run as a shell script.
* The API has no authentication of its own. It binds `127.0.0.1` by default —
  exposing it publishes your working proxies to whoever can reach the port.
* Set `gateway.auth` before binding the gateway to a public
  interface. Credentials are compared in constant time and `Proxy-Authorization`
  is stripped before forwarding.
* Proxy credentials are masked in `/api/v1/proxies`, the REST API and all logs.
  The one place they appear is `/api/v1/get` (that is its job) and the private
  `cache.json`.

## Not included yet

Deliberate omissions, so the core stays small: no database or Redis, no plugin
framework, no rate limiting or per-client quotas, no `https://` upstream
proxies, no SOCKS5 *server* (clients speak HTTP proxy), and no upstream
selection by geolocation.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test              # unit + integration (fake upstreams, no network)
python3 scripts/loadtest.py        # hammer /api/v1/get, print P50/P95/P99 + metric deltas
cargo doc --no-deps --open   # the Chinese API docs, as published on docs.rs
cargo build --release
```

The integration tests spin up hand written HTTP and SOCKS5 upstreams plus fake
target servers in-process, so they need no network access and no external
binaries:

| File                       | Covers                                                        |
| -------------------------- | ------------------------------------------------------------- |
| `tests/pool.rs`            | dedupe, usage persistence, health thresholds, state round trip |
| `tests/selector.rs`        | the rotation contract, reuse window, restart behaviour         |
| `tests/lua_subscriber.rs`  | Lua: return values, parameter globals, paging, credentials, sandbox, timeouts |
| `tests/gateway.rs`         | CONNECT, plain HTTP, auth, retries, SOCKS5 and SOCKS5 auth     |

## Project layout

This is a **library crate plus a binary that only starts the server**:
`src/lib.rs` holds all the logic and `src/main.rs` is a few dozen lines that
handle `--version`/`--help`, initialize tracing and call `server::run`. There are
no subcommands and no client commands — everything else is HTTP. To use ProxyGate
from another Rust program, add the dependency and `use proxygate::...`.

```text
src/
  lib.rs         crate root: module list, crate docs, embedded SKILL.md
  main.rs        thin shell: --version/--help, tracing, start the server
  server.rs      the server: bind, run gateway/API/loops, handle Ctrl-C
  app.rs         shared runtime: pool + state store + clients + checker + hand-out verification
  progress.rs    progress events for fetching, probing and verification
  config.rs      config.yaml model, defaults, validation
  model.rs       Proxy, stable ids, URL normalization, small codecs
  subscriber.rs  subscriber script execution, return-value mapping, Lua sandbox
  pool.rs        the pool: an ArcSwap snapshot (lock-free reads), merges, health updates, rotation
  checker.rs     health checker + shared upstream client cache
  selector.rs    candidate filtering and the random/latency/score strategies
  gateway.rs     HTTP proxy gateway: CONNECT tunnels, forwarding, auth
  api.rs         axum REST API
  useragent.rs   the built-in user agent pool (100 agents)
  metrics.rs     the metrics registry behind GET /metrics
  state.rs       state.json / cache.json, RFC 3339 timestamps
  error.rs       error type shared by every module
assets/          data embedded in the binary (user agent pool)
scripts/         developer scripts (loadtest.py: /get latency and metric baseline)
tests/           integration tests with in-process fake upstreams
SKILL.md         the manual served by `GET /help`
BENCHMARKS.md    the measured performance baseline and how to reproduce it
```

Splitting out the library lets the integration tests drive the real gateway
directly (`tests/common/mod.rs` holds the fake upstreams) instead of spawning a
child process.

Deviations from the v0.1 design notes, each for a reason found while building it:

* `src/lib.rs` as above; the package is lowercase (`proxygate`) so the binary
  name matches the commands in this document.
* `server.listen` is one address so the REST API and the proxy gateway always share a
  single port, dispatched by request shape.
* Built-in sources can be paginated: the URL carries `{page}` and the catalog
  declares the range, which `normalize` expands into one subscriber per page so
  each page is counted and can fail on its own. rola-ip went from 500 entries to
  all 10 pages (4,724).
* `serve` binds its ports first and initializes in the background; until that
  finishes the REST API answers `503` with `Retry-After` instead of pretending
  the pool is empty.
* `state.json` holds exactly what the notes describe, *plus* a separate
  `cache.json` for subscriber and health results. Without it, one `get` against a
  10,000-proxy pool re-probes all 10,000 every time.
* A proxy that has **never** answered is dead after its first failed probe;
  `health.max_failures` only applies to a proxy that was working. Strictly
  speaking the notes only define the counter, but the literal reading hands out
  proxies that have never worked once.
* `health.targets` is a list probed concurrently, with `health.require`
  choosing between "any" (default) and "all"; the notes only described a single
  target. The default pair is Google plus `cn.bing.com` — one endpoint that only
  answers if the proxy can leave the country, one that proves the tunnel is not
  broken for everything else.
* `https://` upstream proxies are rejected when a list is loaded rather than
  accepted and then failing at CONNECT time.
* Subscribers are Lua scripts and nothing else: the design doc listed
  http/file/exec, but the interesting difference between sources is how they are
  fetched and reshaped, which is exactly what a script is good at. It runs in a
  sandbox whose only exit is `fetch`.
* Payload protocol fields are normalized per the table above. `socks5` becoming
  `socks5h` is deliberate: with poisoned local DNS, resolving on the client side
  hands the proxy a bogus address.
* Every source can carry a `limit`, because pulling tens of thousands of proxies
  makes the health loop unable to keep up with its own interval (0 = no cap).
* Doc comments are written in Chinese (the project's primary audience), so
  docs.rs is readable in Chinese only; this README stays bilingual.

## License

[MIT](LICENSE)
