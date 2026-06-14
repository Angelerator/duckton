# syntax=docker/dockerfile:1
#
# Lean P2P DuckDB grid node image.
#
# A "node" = the DuckDB CLI with the `duckdb_p2p` loadable extension. On start it
# `CALL p2p_share(...)` (becomes a host/worker that serves queries over QUIC) and
# optionally `CALL p2p_join(bootstrap => [...])` (learns seeds). The DuckDB
# process is kept alive (stdin held open via a FIFO) so the worker accept loop
# keeps serving the swarm.
#
# Stage 1 (builder): installs Rust + clang/libc++ and builds the `p2p` extension
# cdylib from `crates/extension` (default features — no libp2p; static seeds),
# then appends the DuckDB metadata footer so the CLI can LOAD it. The DuckDB
# version is pinned to match the `duckdb` crate (1.5.3).
#
# Stage 2 (runtime): debian-slim + the DuckDB CLI + the built .duckdb_extension
# + the entrypoint. No Rust toolchain in the final image.

ARG DUCKDB_VERSION=v1.5.3

# ---------------------------------------------------------------------------
# Stage 1: build the extension cdylib + append metadata footer
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

ARG DUCKDB_VERSION
ENV DEBIAN_FRONTEND=noninteractive

# clang/libc++ + python3 (metadata appender) + unzip (duckdb cli) + ca-certs.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        libc++-dev \
        libc++abi-dev \
        python3 \
        unzip \
        curl \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Fetch the DuckDB CLI matching the crate's DuckDB version (used both to read the
# platform string for the metadata footer and copied into the runtime stage).
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      arm64) ddb_asset="duckdb_cli-linux-arm64.zip" ;; \
      amd64) ddb_asset="duckdb_cli-linux-amd64.zip" ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/duckdb.zip \
      "https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/${ddb_asset}"; \
    unzip -o /tmp/duckdb.zip -d /usr/local/bin; \
    chmod +x /usr/local/bin/duckdb; \
    rm /tmp/duckdb.zip; \
    duckdb --version

WORKDIR /src

# Copy the workspace. .dockerignore keeps target/, dist/, .git, ton/build out of
# the build context so this stays lean.
COPY . .

# Build the loadable cdylib (debug profile: faster to compile, smaller RAM
# footprint than a full release build of the whole dependency tree).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build -p p2p-extension; \
    mkdir -p /out; \
    cp target/debug/libduckdb_p2p.so /out/libduckdb_p2p.so

# Append the DuckDB metadata footer → a loadable .duckdb_extension. The output
# basename MUST be `duckdb_p2p.duckdb_extension` (DuckDB derives the init symbol
# from it).
RUN set -eux; \
    platform="$(duckdb -list -noheader -c 'PRAGMA platform;' | tr -d '[:space:]')"; \
    echo "duckdb platform: $platform"; \
    python3 scripts/append_extension_metadata.py \
        -l /out/libduckdb_p2p.so \
        -n duckdb_p2p \
        -p "$platform" \
        -dv v1.0.0 \
        -ev 0.1.0 \
        -o /out/duckdb_p2p.duckdb_extension; \
    duckdb -unsigned -c "LOAD '/out/duckdb_p2p.duckdb_extension'; SELECT value FROM p2p_info() WHERE key='protocol_name';"

# ---------------------------------------------------------------------------
# Stage 2: lean runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive

# DuckDB CLI needs libstdc++/libgomp; bash/coreutils provide the FIFO keep-alive.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libstdc++6 \
        libgomp1 \
        bash \
        coreutils \
        procps \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/duckdb /usr/local/bin/duckdb
COPY --from=builder /out/duckdb_p2p.duckdb_extension /node/duckdb_p2p.duckdb_extension
COPY docker/entrypoint.sh /node/entrypoint.sh
RUN chmod +x /node/entrypoint.sh

# Default QUIC bind (each container has its own netns → same port is fine).
ENV P2P_BIND_ADDR=0.0.0.0:9494 \
    P2P_CONFIG_DIR=/node/state \
    P2P_SHARE_MEMORY=64MB \
    P2P_SHARE_THREADS=1 \
    P2P_SHARE_MAXJOBS=2

EXPOSE 9494/udp
ENTRYPOINT ["/node/entrypoint.sh"]
