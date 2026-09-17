# Subscribers

A subscriber is anything that produces proxy URLs. ProxyGate has three kinds:

| kind    | what it does                                       | use it for                       |
| ------- | -------------------------------------------------- | -------------------------------- |
| `http`  | fetches a URL and reads the response body          | hosted list, provider API        |
| `file`  | reads a local file                                 | hand-curated lists, tests        |
| `exec`  | runs a command and reads its **stdout**            | everything else                  |

All three end up in the same place: text that is parsed by a built-in format
(`plaintext`, `json`, `clash`), normalized, and merged into the pool.

## The rule

> A subscriber's only job is to produce proxy URLs. If a format is hard, put the
> parsing in an `exec` script — the core stays small.

## The `exec` contract

```
stdout    one proxy URL per line
stderr    progress and errors (logged by ProxyGate at debug level)
exit 0    success
exit ≠ 0  the subscriber failed (the pool keeps its previous proxies)
```

Accepted URL shapes (from [`src/model.rs`](../src/model.rs)):

```text
http://1.2.3.4:8080
http://user:pass@1.2.3.4:8080
user:pass@1.2.3.4:3128          # scheme defaults to http
1.2.3.4:8080                    # scheme defaults to http
socks5://1.2.3.4:1080
socks5h://user:pass@[2001:db8::1]:1080
```

Blank lines, `# comments` and `// comments` are ignored, a UTF-8 BOM is
stripped, and a trailing `# comment` after whitespace is removed. Anything that
cannot be normalized is counted and reported as *rejected* — it does not fail
the subscriber.

Unsupported on purpose (rejected at normalization time): `https://` upstream
proxies (ProxyGate v0.1 cannot do TLS *to* the proxy), `socks4://`, and any
other scheme. Shadowsocks/VMess/Trojan entries in a Clash file are counted as
*skipped*, not rejected.

## `exec` examples

```yaml
subscribers:
  # A Python script that understands a provider's strange API.
  - name: weird-provider
    type: exec
    command: [python3, ./subscribers/example.py, --url, https://provider.example/api/list]
    format: plaintext
    timeout: 30s

  # Anything that prints URLs works — here, a one-liner.
  - name: from-aws
    type: exec
    command: [sh, -c, "aws ssm get-parameter --name /proxy/list --query Parameter.Value --output text"]
    env:
      AWS_PROFILE: proxies

  # Chain tools: fetch, decrypt, extract.
  - name: encrypted-list
    type: exec
    command: [./subscribers/decrypt.sh]
    format: plaintext
```

`example.py` in this directory is a complete, dependency-free starting point: it
fetches a URL and understands JSON APIs, plaintext lists and base64
subscriptions.

```bash
python3 subscribers/example.py --url https://example.com/proxies.txt
python3 subscribers/example.py --stdin < payload.json
python3 subscribers/example.py --url https://example.com/sub --format base64
```

## Security note

`exec` runs an arbitrary command with the privileges of the ProxyGate process.
That is the point of the escape hatch, but it means `config.yaml` is trusted
input: do not load a config file you would not run as a shell script.
