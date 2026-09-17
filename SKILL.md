---
name: proxygate
description: Obtain a verified working proxy (and a matching desktop User-Agent) from a self-hosted ProxyGate pool, or run that pool as an HTTP gateway and REST API. Use when a task needs to fetch a URL through a proxy, rotate egress IPs, or send a realistic browser User-Agent.
---

# ProxyGate

ProxyGate keeps a pool of working upstream proxies and hands them out one at a
time. It fetches proxies from configured sources, normalizes them, probes them
through multiple health targets, and only gives you ones that passed.

## Get a proxy

```bash
proxygate get
# http://user:pass@1.2.3.4:8080
```

**stdout is exactly one line: the proxy URL.** Every log line goes to stderr, so
this always composes:

```bash
curl -x "$(proxygate get)" https://example.com
```

`proxygate get` may take a few seconds on a cold cache: it fetches the sources
and probes the pool first. Later calls reuse the cached results until
`refresh.interval` / `health.interval` elapse.

### Exit codes

| code | meaning | what to do |
| ---- | ------- | ---------- |
| `0`  | a proxy was printed | use it |
| `3`  | no healthy proxy available | run `proxygate list` to see why, or `proxygate refresh` |
| `1`  | a real error (bad config, unreachable subscriber) | read stderr |

Treat `3` as "try again later", not as a bug: a pool of free proxies is often
empty for a few minutes and then fine again.

## Get a user agent

```bash
proxygate getua
# Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ... Chrome/131.0.6778.86 Safari/537.36
```

100 desktop browsers are built in (Chrome, Edge, Firefox, Safari on Windows,
macOS, Linux — no mobile agents). The pick is uniformly random with **no
rotation and no memory**: the same string can come up twice in a row. Pair it
with a fresh proxy:

```bash
curl -x "$(proxygate get)" -A "$(proxygate getua)" https://example.com
```

## Commands

| command | what it does |
| ------- | ------------ |
| `get [--format text\|json] [--strategy random\|latency] [--no-refresh] [--no-check] [--mask]` | one healthy proxy |
| `getua [--format text\|json]` | one random desktop user agent |
| `list [--alive] [--show-auth] [--json]` | the pool with status, per-target results, latency |
| `check [--json] [--concurrency N] [--alive-only]` | re-probe the pool now |
| `refresh [--json]` | re-fetch the configured sources now |
| `serve [--listen ADDR] [--api ADDR] [--auth USER:PASS]` | run the gateway + REST API |
| `providers [--json]` | list the built-in proxy sources (name, endpoint, format, caveats) |
| `genconfig` | print an annotated example config that enables every built-in source |
| `skill` | print this document |

Global flags: `-c/--config <path>`, `-v`/`-vv`/`-vvv` (info/debug/trace), `-q`.

`--no-refresh` and `--no-check` mean "use the cache even if it is stale" — the
right choice when calling `get` in a tight loop or when offline.

## REST API

When the pool is running as a service (`proxygate serve`), use HTTP instead of
shelling out. Default port is `127.0.0.1:8081`.

| endpoint | response |
| -------- | -------- |
| `GET /api/v1/get` | the proxy URL as `text/plain` (one line) |
| `GET /api/v1/get?format=json` | `{"proxy":"http://...","latency_ms":83,"round":3}` |
| `GET /api/v1/getua` | a user agent as `text/plain` |
| `GET /api/v1/getua?format=json` | `{"user_agent":"Mozilla/5.0 ..."}` |
| `GET /api/v1/proxies` | the pool as JSON, credentials masked as `***:***` |
| `GET /api/v1/health` | `{"status":"ok","proxies":{"total":2,"alive":2,"dead":0}, ...}` |
| `GET /` | an index of the endpoints |

`/api/v1/get` answers `503` when the pool has nothing healthy — same meaning as
exit code `3`.

The API can share the gateway port (`serve --api same`, or `server.api: same`):
`CONNECT` and absolute-form requests go to the proxy, origin-form paths such as
`/api/v1/get` go to the API. On a shared port `--auth` still protects **proxy
requests only** — the API itself stays open, so only do this on a trusted
interface.

## Built-in sources

`proxygate providers` lists the curated catalog of public endpoints shipped in
the binary. One switch subscribes to all of them:

```yaml
builtin-subscribers: enabled
```

or pick one by name:

```yaml
subscribers:
  - name: scdn
    type: builtin
    provider: scdn
```

`proxygate genconfig` writes `builtin-subscribers: enabled`, so
`genconfig > config.yaml` followed by a `get` works with no editing. Each entry
is an HTTP fetch with the catalog's payload format; `url`/`format`/`timeout`/
`limit` can be overridden. Treat these sources as best-effort: they are free
lists, they rate limit, and most of what they return fails the health check —
roughly 25 of 1500 freshly fetched ones passed in testing, and they rot within
minutes, so ask for a new proxy per task instead of caching one.

## Configuration

`proxygate genconfig > config.yaml` writes a documented starting point. The keys
that matter most:

```yaml
subscribers:                 # where proxies come from
  - name: provider
    type: http               # http | file | exec
    url: https://example.com/proxies.txt
    format: plaintext        # plaintext | json | clash

health:
  targets:                   # each is fetched *through* the proxy
    - https://www.google.com/generate_204
    - https://cn.bing.com/
  require: any               # any (default) or all targets must answer
  concurrency: 100

selection:
  strategy: random           # random | latency
  reuse_after: 30m           # prefer proxies not used recently

gateway:
  auth: admin:secret         # require credentials from gateway clients
```

- Proxies already handed out in the current round are skipped; when every
  healthy proxy has been used the round resets immediately.
- `type: exec` runs a command and reads proxy URLs from its stdout — the escape
  hatch for any format the built-in parsers do not understand.
- `https://` upstream proxies are not supported; they are rejected when a list is
  loaded.

## Gateway mode

```bash
proxygate serve --listen 0.0.0.0:8080 --auth admin:secret
curl -x http://admin:secret@127.0.0.1:8080 https://example.com
```

Clients speak plain HTTP proxy (CONNECT and absolute-form GET/HEAD) and never
learn the upstream address or its credentials. Client auth (`--auth`) and
upstream auth are independent. `serve` runs four things: subscriber refresh,
health checking, the REST API and the gateway. Pass `--api same` to put the API
on the proxy port instead of `127.0.0.1:8081`.

## Things to know before trusting the output

* **Free proxy lists are mostly dead.** A pool of 20 freshly fetched free
  proxies often yields 0–1 usable ones, and a proxy that worked a minute ago may
  be gone now. Always request a new one for a new task instead of caching it.
* **Proxies that intercept TLS are rejected.** If a proxy re-signs HTTPS traffic
  with its own certificate, the health check fails it
  (`invalid peer certificate`) and it is never handed out. Do not try to work
  around this: a client validating certificates cannot use such a proxy anyway.
* **Health results are per target.** `list --json` and `/api/v1/proxies` show
  which target each proxy reached (`1/2` means one of two). With the default
  `require: any` a `1/2` proxy is still handed out — check the breakdown if a
  task needs to reach a specific kind of destination.
* **The pool is shared state.** Every `get` marks the proxy used for this round,
  and that state is persisted to disk, so consecutive calls rotate.
* **`--auth` does not cover the REST API when the port is shared.** The API has
  no authentication of its own; keep the shared port on `127.0.0.1` or put a
  firewall in front of it.
* **`proxygate serve` needs a writable cache directory** (`~/.cache/proxygate`,
  or `state.dir` / `$PROXYGATE_CACHE_DIR`). If it cannot write, it keeps serving
  but stops persisting rotation state.
