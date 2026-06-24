# syntax=docker/dockerfile:1
#
# console-server: the live grid backend for the web console / homepage.
#
# Runs a REAL in-process loopback-QUIC grid (coordinator + workers, real DuckDB
# on honest nodes) and serves live metrics over SSE. Built for PUBLIC hosting
# behind a Cloudflare Tunnel: see deploy/console-server/docker-compose.yml.
#
# Stage 1 (builder): rust + clang/libc++ to compile the bundled DuckDB that the
# `duckdb-engine` feature links. Stage 2 (runtime): debian-slim + the binary.

# Pinned to the same base as docker/Dockerfile (multi-arch index digest).
FROM rust:1-bookworm@sha256:19817ead3289c8c631c73df281e18b59b172f6a31f4f563290f69cddd06c30e9 AS builder

ENV DEBIAN_FRONTEND=noninteractive

# clang/libc++ are required to compile bundled DuckDB (duckdb-engine feature).
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        libc++-dev \
        libc++abi-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy the workspace. .dockerignore keeps target/, dist/, .git out of context.
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build --release -p console-server; \
    mkdir -p /out; \
    cp target/release/console-server /out/console-server

# ---------------------------------------------------------------------------
# Stage 2: lean runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim@sha256:96e378d7e6531ac9a15ad505478fcc2e69f371b10f5cdf87857c4b8188404716 AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# Bundled DuckDB needs the C++ runtime + libgomp at run time.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libstdc++6 \
        libgomp1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/console-server /usr/local/bin/console-server

RUN groupadd --gid 1001 console \
    && useradd --uid 1001 --gid 1001 --no-create-home --shell /usr/sbin/nologin console

# Public posture: listen on the container interface, read-only (no /api/query),
# CORS limited to the production web origins. Override at run time as needed.
ENV CONSOLE_BIND=0.0.0.0:8787 \
    CONSOLE_READONLY=1 \
    CONSOLE_ALLOWED_ORIGINS=https://duckton.com,https://console.duckton.com

EXPOSE 8787

USER console
ENTRYPOINT ["/usr/local/bin/console-server"]
