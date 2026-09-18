---
name: proxygate
description: Obtain a verified working proxy (and a matching desktop User-Agent) from a self-hosted ProxyGate server, or drive its pool over HTTP. Use when a task needs to fetch a URL through a proxy, rotate egress IPs, or send a realistic browser User-Agent.
---

# ProxyGate

ProxyGate keeps a pool of working upstream proxies and hands them out one at a
time over HTTP. It fetches proxies from configured sources, normalizes them,
probes them through health targets, and only hands over ones that passed.

**There is no command-line client.** One process serves everything: this document
is `GET /help`, the pool is `/api/v1/*`, and the proxy itself is a normal HTTP
proxy on the gateway port.

## Get a proxy

```bash
curl -sf http://127.0.0.1:8081/api/v1/get
# http://user:pass@1.2.3.4:8080
```

Use `-f` (or `--fail`): when the pool has nothing to give, `/get` answers `503`
with an explanatory body, and without `-f` that body would be pasted into `-x`
as if it were a proxy address.

Compose it directly:

```bash
curl -x "$(curl -sf http://127.0.0.1:8081/api/v1/get)" https://example.com
```

The proxy is handed out **verified**: if the cached verdict for the chosen proxy
is older than `selection.max_age` (default 60s), ProxyGate probes it first and
returns it only if it passes; otherwise it marks it dead and tries the next
candidate (up to `selection.verify_attempts`, default 3). Set
`selection.verify: false` to hand out cached verdicts without checking.

### Status codes

| code | body | meaning |
| ---- | ---- | ------- |
| `200` | the proxy URL | use it |
| `503` | `proxygate: no verified proxy available` | nothing usable right now; retry later |
| `503` | `proxygate: still initializing the proxy pool; retry in 5 seconds` | the first fetch/check is still running (`Retry-After: 5`) |
| `404` | – | unknown path |

Treat `503` as "try again shortly", not as a bug: a pool of free proxies is
often empty for a few minutes and then fine again. `GET /api/v1/health` tells
you which of the two `503`s you are in (`ready`, `initializing`,
`proxies.total`).

## Get a user agent

```bash
curl -sf http://127.0.0.1:8081/api/v1/getua
# Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ... Chrome/131.0.6778.86 Safari/537.36
```

100 desktop browsers are built in (Chrome, Edge, Firefox, Safari on Windows,
macOS, Linux — no mobile agents). The pick is uniformly random with **no
rotation and no memory**: the same string can come up twice in a row. Pair it
with a fresh proxy:

```bash
curl -x "$(curl -sf http://127.0.0.1:8081/api/v1/get)" \
     -A "$(curl -sf http://127.0.0.1:8081/api/v1/getua)" https://example.com
```

## Endpoints

| endpoint | response |
| -------- | -------- |
| `GET /help` | this document, as `text/markdown` |
| `GET /` | an index of the endpoints |
| `GET /api/v1/get` | a proxy URL as `text/plain` (one line) |
| `GET /api/v1/get?format=json` | `{"proxy":"http://...","latency_ms":83,"round":3}` |
| `GET /api/v1/getua` | a user agent as `text/plain` |
| `GET /api/v1/getua?format=json` | `{"user_agent":"Mozilla/5.0 ..."}` |
| `GET /api/v1/proxies` | the pool as JSON, credentials masked as `***:***` |
| `GET /api/v1/health` | `{"status":"ok","ready":true,"proxies":{"total":2,"alive":2,"dead":0}, ...}` |
| `GET /api/v1/providers` | the built-in source catalog, as JSON |
| `POST /api/v1/refresh` | `202` — fetch every subscriber now |
| `POST /api/v1/check` | `202` — probe the whole pool now |

To get a starting config you do **not** go through the API (the server needs a
config to run in the first place):

```bash
proxygate --example-config > config.yaml
```

The two `POST`s only wake the background loops (the server owns the work), so
they return immediately: watch the logs for progress, then poll
`/api/v1/health` and `/api/v1/proxies` for the result. A full fetch can take
minutes.

## Configuration

`proxygate --example-config` prints an annotated starting point, and the release
archives ship the same file as `config.example.yaml`.

The server reads `$PROXYGATE_CONFIG`, or `./config.yaml`, or
`~/.config/proxygate/config.yaml` — nothing else, there are no flags. The keys
that matter most:

```yaml
subscribers:                 # where proxies come from
  - name: provider
    type: http               # http | file | exec | builtin
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
  verify: true               # probe the chosen proxy before handing it out
  max_age: 60s               # ...unless its verdict is newer than this
  verify_timeout: 3s         # per-candidate timeout while verifying
  verify_attempts: 3         # how many candidates to try before giving up

gateway:
  auth: admin:secret         # require credentials from gateway clients

server:
  proxy: 127.0.0.1:8080      # the HTTP proxy
  api: 127.0.0.1:8081        # the REST API; `same` shares the proxy port
```

- `builtin-subscribers: enabled` subscribes to every source in the catalog that
  ProxyGate ships (`GET /api/v1/providers` lists them, with endpoints, formats,
  page ranges and caveats); `genconfig` writes `enabled`, and the switch defaults
  to `disabled` so nothing reaches the network unless a config says so.
- Proxies already handed out in the current round are skipped; when every
  healthy proxy has been used the round resets immediately.
- `type: exec` runs a command and reads proxy URLs from its stdout — the escape
  hatch for any format the built-in parsers do not understand. It runs with the
  same privileges as the server, so treat the config file as trusted input.
- `https://` upstream proxies are not supported; they are rejected when a list is
  loaded.

## Gateway mode

The proxy port speaks plain HTTP proxy protocol (CONNECT and absolute-form
GET/HEAD) and never reveals the upstream address or its credentials:

```bash
curl -x http://admin:secret@127.0.0.1:8080 https://example.com
```

Client auth (`gateway.auth`) and upstream auth are independent. With
`server.api: same` both roles share one port: `CONNECT` and absolute-form
requests go to the proxy, origin-form paths such as `/api/v1/get` go to the API.
On a shared port `gateway.auth` still protects **proxy requests only** — the API
itself stays open, so only do that on a trusted interface.

## Things to know before trusting the output

* **Free proxy lists are mostly dead.** In a full live run of the four built-in
  sources, 5,361 entries were fetched and 66 of the 5,157 pooled proxies passed
  the health check. Always request a new one for a new task instead of caching it.
* **Hand-out verification is a snapshot.** A proxy can die seconds after it was
  verified, and it was verified against *ProxyGate's* targets (Google and
  `cn.bing.com` by default), not against the site you are about to fetch. With
  `require: any`, a proxy that reaches only one of them still counts as good.
* **The pool is shared state.** Every `/get` marks the proxy used for this round,
  and that state is persisted to disk, so consecutive calls rotate.
* **Proxies that intercept TLS are rejected.** If a proxy re-signs HTTPS traffic
  with its own certificate, the health check fails it
  (`invalid peer certificate`) and it is never handed out.
* **Health results are per target.** `GET /api/v1/proxies` shows which target
  each proxy reached, and `health.require` decides how many must pass before a
  proxy counts as alive.
* **The server needs a writable cache directory** (`~/.cache/proxygate`, or
  `%LOCALAPPDATA%\proxygate` on Windows, or `state.dir` /
  `$PROXYGATE_CACHE_DIR`). If it cannot write, it keeps serving but stops
  persisting rotation state.
* **`selection.verify` costs one probe per hand-out** when the cached verdict is
  stale. In front of many concurrent clients, either raise `max_age` to roughly
  the duration of one health pass, cap the pool size, or set `verify: false` and
  let clients do the retrying.
