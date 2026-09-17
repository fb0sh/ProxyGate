# ProxyGate

[中文文档](README.md) · **English**

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

> The Rust API docs (**Chinese**) live at <https://docs.rs/proxygate> — the doc
> comments in the source are what is published. Locally:
> `cargo doc --no-deps --open`.

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

> The generated config carries **no client authentication**: it assumes the
> gateway stays reachable only from this machine. Add `--auth user:pass` on the
> command line when you expose it — that keeps credentials out of a checked-in
> file.

| Key                   | Default                                   | Meaning                                              |
| --------------------- | ----------------------------------------- | ---------------------------------------------------- |
| `server.proxy`        | `127.0.0.1:8080`                          | HTTP proxy gateway address                            |
| `server.api`          | `127.0.0.1:8081`                          | REST API address; `same` shares the gateway port      |
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

> **Enabling every built-in grows the pool a lot.** A cold start of all 13
> subscribers (rola-ip's 10 pages plus three others) fetches ~5,200 proxies, and
> one health pass over them takes minutes (5,157 checked in ~3.5 minutes with 66
> alive, measured). With the default `health.interval` of 30s the checker is then
> busy almost continuously — raise it to `10m`, or cap a source with `limit`, if
> you want it to rest.

### Sharing one port with the API

`server.api` also accepts `same` (or `proxy`, or a literal copy of the
`server.proxy` address):

```yaml
server:
  proxy: 127.0.0.1:8080
  api: same          # the REST API shares 127.0.0.1:8080 with the gateway
```

The port splits traffic by request shape:

| Request received                    | Verdict       | Destination |
| ----------------------------------- | ------------- | ----------- |
| `CONNECT host:443`                  | proxy request | HTTP gateway |
| `GET http://host/path` (absolute)   | proxy request | HTTP gateway |
| `GET /api/v1/get` (origin-form)     | API request   | REST API     |

> **Auth covers proxy requests only.** `gateway.auth` (`--auth user:pass`)
> rejects proxy requests, but an API sharing the port stays open — anyone who can
> reach the port can read `/api/v1/proxies`. Only share the port on a trusted
> interface (the default is `127.0.0.1`). `proxygate serve` logs a warning when
> you do. Expose it publicly with the API back on its own port, or behind a
> firewall rule.

There is no functional difference between the two layouts; share a port when
mapping one container port is convenient, split them when you want them
separable while debugging.

### Subscribers

Four kinds: `builtin` (a curated source), `http`, `file`, and the `exec` escape
hatch.

`builtin` refers to the catalog maintained in the code. One switch subscribes to
all of it:

```yaml
builtin-subscribers: enabled      # subscribe to every catalog entry
# disabled                        # use only the subscribers you write
```

`proxygate providers` lists the whole catalog (endpoint, format, caveats, docs)
and takes `--json`. The switch defaults to `disabled` — nothing reaches the
network unless a config says so — and `genconfig` writes `enabled`.

A single source can also be picked by name and tuned. A builtin is an HTTP fetch
underneath, so it takes the same overrides plus `limit`:

```yaml
subscribers:
  - name: scdn-cn
    type: builtin
    provider: scdn
    url: https://proxy.scdn.io/api/get_proxy.php?protocol=http&count=20&country_code=CN
    format: json      # defaults to the catalog format
    timeout: 20s      # the catalog can give a source a longer one
    limit: 200        # keep at most this many usable proxies (0 = no cap)
```

`limit` exists because the health checker probes every proxy it is given: 16,000
of them is a twelve minute pass at the default concurrency. The catalog caps the
one huge source at 1000, taking entries in the order returned (big lists are
ordered fastest-first).

The other three kinds, for everything else:

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

Protocol fields in payloads are understood in the shapes lists actually use:
`protocol` as a string, `protocols` as an array, and joined strings like
`"socks4+socks5"`. The mapping:

| The payload says | Becomes | Why |
| ---------------- | ------- | --- |
| `http` / `https` / `ssl` | `http://` | in a list, "https" means the proxy can CONNECT to HTTPS, not TLS-to-proxy |
| `socks5` / `socks5h` / `socks` | `socks5h://` | the proxy resolves names: with poisoned local DNS, `socks5://` hands the proxy a bogus address and both the probe and real use fail |
| `socks4`, anything else | dropped | v0.1 cannot tunnel it |

An entry offering both picks `http`.

Accepted URL shapes:

```text
http://1.2.3.4:8080          socks5://1.2.3.4:1080
user:pass@1.2.3.4:3128       socks5h://user:pass@[2001:db8::1]:1080
1.2.3.4:8080                 # scheme and port get sensible defaults
```

The first catalog entry is
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

`proxygate --help` (and every subcommand's `--help`) is printed in Chinese: clap
uses the doc comments as help text. Only clap's own structural headings
(`Usage:`, `Options:`) and its generated error messages stay English — clap has
no localization hook for them.

```text
proxygate get     [--format text|json] [--strategy random|latency] [--no-refresh] [--no-check] [--mask]
proxygate list    [--alive] [--all] [--show-auth] [--json] [--no-refresh] [--no-check]
proxygate refresh [--json]
proxygate check   [--json] [--concurrency N] [--alive-only]
proxygate serve   [--listen ADDR] [--api ADDR] [--auth USER:PASS] [--no-refresh]
proxygate genconfig                       # the annotated example config, to stdout
proxygate getua   [--format text|json]    # one random user agent
proxygate skill                           # the agent-facing SKILL.md, to stdout
proxygate providers [--json]              # the built-in proxy source catalog
```

Global flags: `-c/--config <path>`, `-v` (info), `-vv` (debug), `-vvv` (trace),
`-q` (errors only). `RUST_LOG` overrides the log filter.

`proxygate get` writes **exactly one line** — the proxy URL — to stdout; every
log line goes to stderr. That is what makes `curl -x "$(proxygate get)"` work.

Exit codes: `0` success, `1` error, `3` no proxy available.

`genconfig` prints the annotated example config that is embedded in the binary,
so an installed `proxygate` can bootstrap a config with no checkout around:

```bash
proxygate genconfig > config.yaml
```

`getua` prints one user agent out of 100 built into the binary
([`assets/user_agents.txt`](assets/user_agents.txt) — desktop Chrome, Edge,
Firefox and Safari on Windows, macOS and Linux; no mobile agents, on purpose).
It is uniformly random and stateless: no rotation, no memory of previous calls,
so the same string can come up twice in a row. Handy for pairing with a fresh
proxy:

```bash
curl -x "$(proxygate get)" -A "$(proxygate getua)" https://example.com
```

`skill` prints [`SKILL.md`](SKILL.md) — the same contract this document
describes, written for an AI agent to read on its own (commands, exit codes, the
API, the config keys that matter, and what not to trust). It only writes to
stdout; where the text ends up is the caller's business:

```bash
proxygate skill > SKILL.md      # keep it
proxygate skill | pbcopy        # or hand it straight to an agent
```

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
| `GET /api/v1/getua`             | one random user agent as `text/plain`                         |
| `GET /api/v1/getua?format=json` | `{"user_agent": "Mozilla/5.0 ..."}`                          |
| `GET /api/v1/proxies`           | the pool as JSON, credentials masked                         |
| `GET /api/v1/health`            | `{"status": "ok", "proxies": {"total": 2, "alive": 2, ...}}` |
| `GET /`                         | a small index of the above                                   |

```console
$ curl http://127.0.0.1:8081/api/v1/get
http://user:pass@1.2.3.4:8080

$ curl -s http://127.0.0.1:8081/api/v1/health
{"status":"ok","version":"0.1.0","uptime_seconds":42,"generation":3,
 "strategy":"random","health_targets":["https://cn.bing.com/"],
 "health_require":"any","ready":true,"initializing":false,
 "initialization_attempts":1,"initialization_error":null,
 "proxies":{"total":2,"alive":2,"dead":0}}
# While the first pass runs, `status` is "initializing" and `ready` is false.
```

With `server.api: same`, use port `8080` in the examples above; the paths
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
cargo test              # unit + integration (fake upstreams, no network)
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
| `tests/subscriber.rs`      | file/http/exec, the three formats, failure isolation           |
| `tests/gateway.rs`         | CONNECT, plain HTTP, auth, retries, SOCKS5 and SOCKS5 auth     |

## Project layout

This is a **library crate with a thin binary**: `src/lib.rs` holds all the
logic and `src/main.rs` is a few dozen lines that parse arguments, initialize
tracing, dispatch and map errors to exit codes. To use ProxyGate from another
Rust program, add the dependency and `use proxygate::...` — the CLI is optional.

```text
src/
  lib.rs         crate root: module list, crate docs, embedded SKILL.md
  main.rs        thin shell: parse, init tracing, dispatch, exit code
  app.rs         shared runtime: pool + state store + HTTP clients + checker
  commands.rs    one function per CLI subcommand
  cli.rs         clap definitions
  config.rs      config.yaml model, defaults, validation
  model.rs       Proxy, stable ids, URL normalization, small codecs
  subscriber.rs  builtin/http/file/exec subscribers and the built-in parsers
  providers.rs   the built-in source catalog (printed by `proxygate providers`)
  pool.rs        the pool: merge, health updates, selection (rounds)
  checker.rs     health checker + shared upstream client cache
  selector.rs    candidate filtering and the random/latency strategies
  gateway.rs     HTTP proxy gateway: CONNECT tunnels, forwarding, auth
  api.rs         axum REST API
  useragent.rs   the built-in user agent pool (100 agents)
  state.rs       state.json / cache.json, RFC 3339 timestamps
  error.rs       error type shared by every module
assets/          data embedded in the binary (user agent pool)
subscribers/     exec subscriber contract + example.py
tests/           integration tests with in-process fake upstreams
SKILL.md         the agent-facing document printed by `proxygate skill`
```

Splitting out the library lets the integration tests drive the real gateway
directly (`tests/common/mod.rs` holds the fake upstreams) instead of spawning a
child process.

Deviations from the v0.1 design notes, each for a reason found while building it:

* `src/lib.rs` as above; the package is lowercase (`proxygate`) so the binary
  name matches the commands in this document.
* `server.api` accepts `same` so the REST API and the proxy gateway can share a
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
* A fourth subscriber kind, `builtin`, plus a `builtin-subscribers` switch, point
  at the curated catalog so endpoints, formats and rate-limit notes live in one
  place; it is still just an HTTP fetch underneath.
* Payload protocol fields are normalized per the table above. `socks5` becoming
  `socks5h` is deliberate: with poisoned local DNS, resolving on the client side
  hands the proxy a bogus address.
* A source can carry a default `limit`, because pulling tens of thousands of
  proxies makes the health loop unable to keep up with its own interval.
* Doc comments are written in Chinese (the project's primary audience), so
  docs.rs is readable in Chinese only; this README stays bilingual.

## License

[MIT](LICENSE)
