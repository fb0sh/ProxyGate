# syntax=docker/dockerfile:1
#
# Build:
#   docker build -t proxygate .
#
# If crates.io is slow or blocked from your network, point cargo at a mirror:
#   docker build --build-arg CARGO_MIRROR=https://rsproxy.cn/index/ -t proxygate .
#
# Run (mount a config.yaml and a persistent state directory):
#   docker run --rm -p 8080:8080 -p 8081:8081 \
#     -v "$PWD/config.yaml:/home/proxygate/config.yaml:ro" \
#     -v proxygate-cache:/home/proxygate/.cache/proxygate \
#     proxygate
#
# `serve` is the default command, so the usual overrides work:
#   docker run --rm proxygate get
#   docker run --rm proxygate refresh

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

# `rust:*-slim` already ships gcc, libc headers and perl, so only the build
# driver is missing: aws-lc-rs (pulled in by rustls) configures with cmake.
# clang/libclang-dev are only required for FIPS or `ssl` feature builds, which
# use bindgen instead of the pregenerated bindings.
# `ForceIPv4` keeps apt from stalling on hosts that advertise AAAA records but
# have no working IPv6 route (common in CI and container runtimes).
RUN echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4 \
    && apt-get update \
    && apt-get install -y --no-install-recommends make cmake ca-certificates \
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

COPY src ./src
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

EXPOSE 8080 8081

# Assumes the default API port (8081); override the health check if you move it.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8081/api/v1/health || exit 1

ENTRYPOINT ["proxygate"]
# Inside a container the loopback address is not reachable from the host, so
# bind to all interfaces and let the port mapping be the access control.
CMD ["serve", "--listen", "0.0.0.0:8080", "--api", "0.0.0.0:8081"]
