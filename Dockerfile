# syntax=docker/dockerfile:1
#
# Build:
#   docker build -t proxygate .
#
# If crates.io is slow or blocked from your network, point cargo at a mirror:
#   docker build --build-arg CARGO_MIRROR=https://rsproxy.cn/index/ -t proxygate .
#
# Run (mount a config.yaml and a persistent state directory):
#   docker run --rm -p 8080:8080 \
#     -v "$PWD/config.yaml:/home/proxygate/config.yaml:ro" \
#     -v proxygate-cache:/home/proxygate/.cache/proxygate \
#     proxygate
#
# The binary takes no arguments: it reads the config and serves. The image sets
# PROXYGATE_CONFIG=/home/proxygate/config.yaml, so the mount above is all it
# takes. That config has to bind `0.0.0.0` (the built-in default binds
# `127.0.0.1`, which is unreachable through a port mapping):
#
#   server:
#     listen: 0.0.0.0:8080

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

# Two C toolchains are needed: mlua compiles the vendored Lua 5.4 sources with
# `cc` (gcc/libc6-dev), and aws-lc-rs (pulled in by rustls) configures with
# cmake. clang/libclang-dev are only required for FIPS or `ssl` feature builds,
# which use bindgen instead of the pregenerated bindings.
# `ForceIPv4` keeps apt from stalling on hosts that advertise AAAA records but
# have no working IPv6 route (common in CI and container runtimes).
RUN echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4 \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        make cmake gcc libc6-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG CARGO_MIRROR=""
RUN if [ -n "${CARGO_MIRROR}" ]; then \
        mkdir -p "${CARGO_HOME}" \
        && printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "sparse+%s"\n' \
            "${CARGO_MIRROR}" > "${CARGO_HOME}/config.toml"; \
    fi

WORKDIR /build
COPY Cargo.toml Cargo.lock ./

# Compile the dependency tree once, with stubs standing in for the real crate.
RUN mkdir -p src \
    && printf 'fn main() {}\n' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# `src/` pulls in three files with `include_str!`: the user agent pool, the
# annotated example config and the skill document. All have to be copied or the
# build fails.
COPY src ./src
COPY assets ./assets
COPY config.example.yaml ./
COPY SKILL.md ./
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && strip target/release/proxygate

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates: needed for HTTPS subscribers and health checks.
# curl: only for the health check below.
RUN echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4 \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 proxygate

COPY --from=builder /build/target/release/proxygate /usr/local/bin/proxygate

USER proxygate
WORKDIR /home/proxygate

# Create the state directory as the runtime user *before* declaring the volume:
# Docker initialises a fresh volume with the ownership of the image's directory,
# so this is what makes the anonymous volume writable for uid 10001.
RUN mkdir -p /home/proxygate/.cache/proxygate

# state.json and cache.json live here (see the volume in the build notes above).
VOLUME ["/home/proxygate/.cache/proxygate"]

# One port: the HTTP proxy gateway and the REST API share it.
EXPOSE 8080

# Assumes the default listen address; override the health check if you move it.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/v1/health || exit 1

# Where the mounted config lives. The binary takes no arguments, so this is the
# only knob the image sets for it.
ENV PROXYGATE_CONFIG=/home/proxygate/config.yaml

ENTRYPOINT ["proxygate"]
