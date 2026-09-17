#!/usr/bin/env python3
"""Example `exec` subscriber.

ProxyGate's core understands three payload formats (`plaintext`, `json`,
`clash`). Everything else belongs here: a small script that fetches whatever
the provider offers and prints proxy URLs, one per line, on stdout.

    stdout   one proxy URL per line
    stderr   human readable progress and errors (logged by ProxyGate)
    exit 0   success, exit non-zero to mark this subscriber as failed

The URLs may be written in any shape the normalizer accepts:

    http://1.2.3.4:8080
    user:pass@1.2.3.4:3128
    socks5://user:pass@5.6.7.8:1080
    socks5h://5.6.7.8:1080
    1.2.3.4:8080

Only the standard library is used, so this runs anywhere Python 3.8+ does.

Usage:

    ./example.py --url https://provider.example/api/list --format auto
    ./example.py --url https://provider.example/sub --format base64
    ./example.py --stdin < payload

Config:

    - name: provider
      type: exec
      command: [python3, ./subscribers/example.py, --url, https://provider.example/api/list]
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

# Fields that hold a full proxy URL, in priority order.
URL_KEYS = ("url", "proxy", "uri")
HOST_KEYS = ("server", "host", "hostname", "ip", "address", "addr")
PORT_KEYS = ("port",)
USER_KEYS = ("username", "user")
PASS_KEYS = ("password", "pass")
SCHEME_KEYS = ("type", "scheme", "protocol", "proxy_type")

# Protocols ProxyGate can actually tunnel through.
SUPPORTED_SCHEMES = {"http", "socks5", "socks5h", "socks"}

URL_PATTERN = re.compile(r"(?:https?|socks5h?|socks)://[^\s\"'<>\\]+")

# `host:port`, with optional credentials, without a scheme.
BARE_PATTERN = re.compile(r"^(?:[^\s@/]+@)?(?:\[[0-9a-fA-F:]+\]|[A-Za-z0-9._-]+):\d{1,5}$")


def looks_like_proxy(line: str) -> bool:
    if "://" in line:
        return URL_PATTERN.match(line) is not None
    return BARE_PATTERN.match(line) is not None


def log(message: str) -> None:
    print(message, file=sys.stderr)


def first(mapping: dict, keys: tuple[str, ...]) -> str | None:
    for key in keys:
        value = mapping.get(key)
        if isinstance(value, (str, int)) and str(value).strip():
            return str(value).strip()
    return None


def proxy_from_object(entry: dict) -> str | None:
    """Builds a proxy URL from the field names most APIs use."""
    for key in URL_KEYS:
        value = entry.get(key)
        if isinstance(value, str) and "://" in value:
            return value.strip()

    host = first(entry, HOST_KEYS)
    if host is None:
        return None

    scheme = (first(entry, SCHEME_KEYS) or "http").lower()
    if scheme not in SUPPORTED_SCHEMES:
        log(f"skipping unsupported protocol: {scheme}")
        return None
    if scheme == "socks":
        scheme = "socks5"

    port = first(entry, PORT_KEYS)
    username = first(entry, USER_KEYS)
    password = first(entry, PASS_KEYS)

    credentials = ""
    if username:
        credentials = urllib.parse.quote(username, safe="")
        if password:
            credentials += ":" + urllib.parse.quote(password, safe="")
        credentials += "@"

    if ":" in host and not host.startswith("["):
        host = f"[{host}]"

    return f"{scheme}://{credentials}{host}{':' + port if port else ''}"


def walk(value, found: list[str]) -> None:
    """Finds proxies in an arbitrarily nested JSON payload."""
    if isinstance(value, list):
        for item in value:
            walk(item, found)
    elif isinstance(value, str):
        found.extend(URL_PATTERN.findall(value))
    elif isinstance(value, dict):
        for key in ("proxies", "data", "items", "list", "result"):
            if isinstance(value.get(key), list):
                walk(value[key], found)
                return
        candidate = proxy_from_object(value)
        if candidate:
            found.append(candidate)


def parse_json(payload: str) -> list[str]:
    found: list[str] = []
    walk(json.loads(payload), found)
    return found


def parse_plaintext(payload: str) -> list[str]:
    lines = []
    for line in payload.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        if looks_like_proxy(line):
            lines.append(line)
        else:
            log(f"ignoring line that is not a proxy: {line!r}")
    return lines


def parse_base64(payload: str) -> list[str]:
    """Common for subscription links: a base64 blob of `host:port` lines."""
    compact = "".join(payload.split())
    try:
        decoded = base64.b64decode(compact + "=" * (-len(compact) % 4)).decode("utf-8", "replace")
    except (binascii.Error, ValueError) as error:
        log(f"not valid base64: {error}")
        return []
    return parse_plaintext(decoded)


def detect_and_parse(payload: str, fmt: str) -> list[str]:
    if fmt == "plain":
        return parse_plaintext(payload)
    if fmt == "json":
        return parse_json(payload)
    if fmt == "base64":
        return parse_base64(payload)

    stripped = payload.lstrip()
    if stripped.startswith(("{", "[")):
        try:
            return parse_json(payload)
        except json.JSONDecodeError as error:
            log(f"payload looked like JSON but did not parse: {error}")
    found = parse_plaintext(payload)
    if found:
        return found
    return parse_base64(payload)


def fetch(url: str, header: list[str], timeout: float) -> str:
    request = urllib.request.Request(url, headers=dict(h.split(":", 1) for h in header))  # type: ignore[arg-type]
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.read().decode("utf-8", "replace")


def main() -> int:
    parser = argparse.ArgumentParser(description="Example ProxyGate exec subscriber")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--url", help="endpoint to fetch")
    source.add_argument("--stdin", action="store_true", help="read the payload from stdin")
    parser.add_argument("--format", default="auto", choices=["auto", "plain", "json", "base64"])
    parser.add_argument("--header", action="append", default=[], help="extra request header")
    parser.add_argument("--timeout", type=float, default=20.0, help="request timeout in seconds")
    args = parser.parse_args()

    try:
        payload = sys.stdin.read() if args.stdin else fetch(args.url, args.header, args.timeout)
    except (urllib.error.URLError, OSError) as error:
        log(f"cannot fetch {args.url}: {error}")
        return 1

    proxies = detect_and_parse(payload, args.format)

    # Deduplicate while keeping the original order.
    seen = set()
    unique = [p for p in proxies if not (p in seen or seen.add(p))]

    if not unique:
        log("no proxies found in the payload")
        return 1

    log(f"found {len(unique)} proxies")
    for proxy in unique:
        print(proxy)
    return 0


if __name__ == "__main__":
    sys.exit(main())
