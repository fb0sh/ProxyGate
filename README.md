# ProxyGate

Turn any proxy source into a uniform, always-ready proxy pool.

ProxyGate collects proxies from HTTP endpoints, local files or arbitrary scripts,
normalizes whatever it finds into `scheme://user:pass@host:port`, checks which
ones actually work, and then hands them out — through a CLI, a REST API, or as a
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
     CLI get       REST API      Gateway
                                HTTP Proxy
```

From the moment a proxy enters the pool, nothing cares where it came from.

## Quick start

```bash
# 1. One real upstream proxy.
proxygate get
http://user:pass@1.2.3.4:8080

# 2. Use it directly.
curl -x "$(proxygate get)" https://example.com

# 3. Or run the always-on gateway.
proxygate serve
curl -x http://127.0.0.1:8080 https://example.com

# 4. Or ask the REST API.
curl http://127.0.0.1:8081/api/v1/get
```

The gateway can also require credentials of its own:

```bash
proxygate serve --listen 0.0.0.0:8080 --auth admin:secret
curl -x http://admin:secret@127.0.0.1:8080 https://example.com
```

Two layers of authentication stay completely independent: your clients
authenticate to ProxyGate, ProxyGate authenticates to the upstream provider.

## Install

```bash
cargo build --release
install -m755 target/release/proxygate ~/.local/bin/proxygate
```

Or with Docker (see [`Dockerfile`](Dockerfile)):

```bash
docker build -t proxygate .
docker run --rm -p 8080:8080 -p 8081:8081 \
  -v "$PWD/config.yaml:/home/proxygate/config.yaml:ro" \
  -v proxygate-cache:/home/proxygate/.cache/proxygate \
  proxygate
```

Requirements: Rust 1.85+ to build. No database, no Redis, no async runtime
beyond Tokio.

CI ([`.github/workflows/build.yml`](.github/workflows/build.yml)) builds two
platforms on every push and attaches a ready-to-run archive to the run:
Linux amd64 and macOS arm64.

## Configuration

ProxyGate looks for a config file in this order:

1. `--config <path>` (or `$PROXYGATE_CONFIG`)
2. `./config.yaml`
3. `~/.config/proxygate/config.yaml`

With no config file it starts with an empty pool, which is useful for testing
but not much else. Start from [`config.example.yaml`](config.example.yaml).

| Key                   | Default                                   | Meaning                                              |
| --------------------- | ----------------------------------------- | ---------------------------------------------------- |
| `server.proxy`        | `127.0.0.1:8080`                          | HTTP proxy gateway address                            |
| `server.api`          | `127.0.0.1:8081`                          | REST API address                                      |
| `subscribers`         | `[]`                                      | Where proxies come from (see below)                   |
| `refresh.interval`    | `10m`                                     | How long a fetched list is reused                     |
| `refresh.timeout`     | `20s`                                     | Per-subscriber timeout                                |
| `health.targets`      | Google 204 + `cn.bing.com`                | URLs fetched *through* each proxy, probed concurrently |
| `health.require`      | `any`                                     | `any` target may answer, or `all` of them must          |
| `health.interval`     | `30s`                                     | Health pass interval, and health cache lifetime       |
| `health.timeout`      | `5s`                                      | Per-proxy probe timeout                               |
| `health.concurrency`  | `100`                                     | Proxies probed in parallel                            |
| `health.max_failures` | `3`                                       | Consecutive failures tolerated for a working proxy    |
| `selection.strategy`  | `random`                                  | `random` or `latency`                                 |
| `selection.reuse_after` | `30m`                                   | Prefer proxies unused in this window                  |
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

### Subscribers

Three kinds, and an escape hatch for everything else:

```yaml
subscribers:
  - name: provider-a
    type: http
    url: https://example.com/proxies.txt
    format: plaintext        # plaintext (default) | json | clash
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

A subscriber only has to produce proxy URLs — one per line on stdout for `exec`.
The built-in parsers cover plaintext lists, JSON APIs (arrays, bare
`host:port` strings, objects with `ip`/`host`/`server` + `port` + credentials,
and arbitrarily nested envelopes such as
`{"code":200,"data":{"proxies":["1.2.3.4:8080"]}}`) and Clash/Clash.Meta
`proxies:` lists. Anything else belongs in a script; see
[`subscribers/README.md`](subscribers/README.md) and
[`subscribers/example.py`](subscribers/example.py).

Accepted URL shapes:

```text
http://1.2.3.4:8080          socks5://1.2.3.4:1080
user:pass@1.2.3.4:3128       socks5h://user:pass@[2001:db8::1]:1080
1.2.3.4:8080                 # scheme and port get sensible defaults
```

A real provider wired up as the first subscriber of
[`config.example.yaml`](config.example.yaml) is
[proxy.scdn.io](https://proxy.scdn.io/api_docs.php): it answers with a JSON
envelope holding bare `host:port` entries, which the `json` format reads
directly. Since the payload carries no scheme, such entries are treated as HTTP
proxies — ask for `protocol=http`, and use an `exec` wrapper if you want its
`socks4`/`socks5` endpoints.

Two things to expect from free lists like that one, both of which the health
check is designed to surface: most entries are simply dead, and a fair share of
the "HTTPS-capable" ones intercept TLS and present a certificate signed by
themselves. ProxyGate rejects those (`invalid peer certificate`) — a proxy that
re-signs traffic is not a proxy you want, and a client that verifies
certificates could not use it anyway.

## CLI

```text
proxygate get     [--format text|json] [--strategy random|latency] [--no-refresh] [--no-check] [--mask]
proxygate list    [--alive] [--all] [--show-auth] [--json] [--no-refresh] [--no-check]
proxygate refresh [--json]
proxygate check   [--json] [--concurrency N] [--alive-only]
proxygate serve   [--listen ADDR] [--api ADDR] [--auth USER:PASS] [--no-refresh]
```

Global flags: `-c/--config <path>`, `-v` (info), `-vv` (debug), `-vvv` (trace),
`-q` (errors only). `RUST_LOG` overrides the log filter.

`proxygate get` writes **exactly one line** — the proxy URL — to stdout; every
log line goes to stderr. That is what makes `curl -x "$(proxygate get)"` work.

Exit codes: `0` success, `1` error, `3` no proxy available.

```console
$ proxygate list
PROXY                           STATUS   TARGETS  LATENCY
http://***:***@127.0.0.1:18080  alive    2/2      824ms
http://127.0.0.1:18083          alive    1/2      817ms    # only cn.bing.com answers
http://203.0.113.7:3128         dead     0/2      -

$ proxygate get --format json
{"proxy":"http://user:pass@127.0.0.1:18080","latency_ms":824,"round":3}
```

## REST API

| Endpoint                        | Returns                                                     |
| ------------------------------- | ----------------------------------------------------------- |
| `GET /api/v1/get`               | one proxy URL as `text/plain`                                |
| `GET /api/v1/get?format=json`   | `{"proxy": "...", "latency_ms": 83, "round": 3}`             |
| `GET /api/v1/proxies`           | the pool as JSON, credentials masked                         |
| `GET /api/v1/health`            | `{"status": "ok", "proxies": {"total": 2, "alive": 2, ...}}` |
| `GET /`                         | a small index of the above                                   |

```console
$ curl http://127.0.0.1:8081/api/v1/get
http://user:pass@1.2.3.4:8080

$ curl -s http://127.0.0.1:8081/api/v1/health
{"status":"ok","version":"0.1.0","uptime_seconds":42,"generation":3,
 "strategy":"random","health_target":"https://example.com/",
 "proxies":{"total":2,"alive":2,"dead":0}}
```

`/get` answers `503` when no healthy proxy is available. Credentials are never
exposed by `/proxies` (they are replaced with `***:***`).

## Gateway

`proxygate serve` runs four things on one Tokio runtime: subscriber refresh,
health checking, the REST API and the HTTP proxy gateway.

* **CONNECT** (HTTPS) becomes a byte tunnel. The upstream is chosen, dialled and
  handshaked *before* the client sees `200`, so a broken upstream is retried
  transparently. One tunnel is pinned to one upstream for its whole lifetime.
* **Plain HTTP** is forwarded with the absolute request target preserved, so the
  upstream does the DNS and the connecting.
* Client credentials (`--auth`) are consumed by ProxyGate and never forwarded;
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
supported in v0.1 — they are rejected when the list is loaded rather than
silently entering the pool as proxies that cannot serve CONNECT.

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
proxygate get   →  A
proxygate get   →  B
proxygate get   →  C
proxygate get   →  round 2 starts, A/B/C are all fair game again
```

`state.json` persists the round number and each proxy's last use, so the
rotation continues across CLI invocations and restarts. A proxy that is added
later (a refresh found a new one) is unused in the current round and therefore
handed out first — new proxies get exercised instead of gathering dust.

## Health checking

For every proxy the checker asks three questions: can a request be made through
it, did the request succeed, and how long did it take. Concurrency is bounded by
a semaphore (`health.concurrency`), and the pool lock is never held while a
request is in flight — the checker works on a snapshot and writes results back
in one short critical section.

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
last result *is* cached for `health.interval` (30s by default) so that a burst of
`proxygate get` calls does not re-probe a 10,000-proxy pool every time. Likewise,
subscriber output is reused for `refresh.interval`. `--no-check` / `--no-refresh`
tell the CLI to trust those caches even when they are stale, which is what you
want in a tight loop or an air-gapped test.

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
files to start from scratch; `proxygate refresh` rebuilds the pool.

## Security notes

* **`exec` subscribers run arbitrary commands.** `config.yaml` is trusted input;
  do not load a config you would not run as a shell script.
* The API has no authentication of its own. It binds `127.0.0.1` by default —
  exposing it publishes your working proxies to whoever can reach the port.
* Set `gateway.auth` (or `--auth`) before binding the gateway to a public
  interface. Credentials are compared in constant time and `Proxy-Authorization`
  is stripped before forwarding.
* Proxy credentials are masked in `proxygate list`, the REST API and all logs.
  The one place they appear is `proxygate get` (that is its job) and the private
  `cache.json`.

## Not in v0.1

Deliberate omissions, so the core stays small: no database or Redis, no plugin
framework, no rate limiting or per-client quotas, no `https://` upstream
proxies, no SOCKS5 *server* (clients speak HTTP proxy), and no upstream
selection by geolocation or provider.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test              # 101 tests: unit + integration (fake upstreams, no network)
cargo build --release
```

The integration tests spin up hand written HTTP and SOCKS5 upstreams plus fake
target servers in-process, so they need no network access and no external
binaries:

| File                       | Covers                                                        |
| -------------------------- | ------------------------------------------------------------- |
| `tests/pool.rs`            | dedupe, usage persistence, health thresholds, state round trip |
| `tests/selector.rs`        | the rotation contract, reuse window, restart behaviour         |
| `tests/subscriber.rs`      | file/http/exec, the three formats, failure isolation           |
| `tests/gateway.rs`         | CONNECT, plain HTTP, auth, retries, SOCKS5 and SOCKS5 auth     |

## Project layout

```text
src/
  main.rs        binary: CLI dispatch, App runtime, background loops
  cli.rs         clap definitions
  config.rs      config.yaml model, defaults, validation
  model.rs       Proxy, stable ids, URL normalization, small codecs
  subscriber.rs  http/file/exec subscribers and the built-in parsers
  pool.rs        the pool: merge, health updates, selection (rounds)
  checker.rs     health checker + shared upstream client cache
  selector.rs    candidate filtering and the random/latency strategies
  gateway.rs     HTTP proxy gateway: CONNECT tunnels, forwarding, auth
  api.rs         axum REST API
  state.rs       state.json / cache.json, RFC 3339 timestamps
  error.rs       error type shared by every module
subscribers/     exec subscriber contract + example.py
tests/           integration tests with in-process fake upstreams
```

Two deviations from a single-binary crate, both for testability: the code lives
in a library (`src/lib.rs`) with a thin binary on top, so the integration tests
can drive the real gateway, and `tests/common/mod.rs` holds the fake upstreams.

Deviations from the v0.1 design notes, each for a reason found while building it:

* `src/lib.rs` as above; the package is lowercase (`proxygate`) so the binary
  name matches the commands in this document.
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

## License

[MIT](LICENSE)
