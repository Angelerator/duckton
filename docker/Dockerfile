# syntax=docker/dockerfile:1
#
# Lean P2P DuckDB grid node image.
#
# A "node" = the DuckDB CLI with the `duckton` loadable extension. On start it
# `CALL p2p_share(...)` (becomes a host/worker that serves queries over QUIC) and
# optionally `CALL p2p_join(bootstrap => [...])` (learns seeds). The DuckDB
# process is kept alive (stdin held open via a FIFO) so the worker accept loop
# keeps serving the swarm.
#
# Stage 1 (builder): installs Rust + clang/libc++ and builds the `p2p` extension
# cdylib from `crates/extension` (default features — no libp2p; static seeds),
# then appends the DuckDB metadata footer so the CLI can LOAD it. The DuckDB
# version is pinned to match the `duckdb` crate (1.5.4).
#
# Stage 2 (runtime): debian-slim + the DuckDB CLI + the built .duckdb_extension
# + the entrypoint. No Rust toolchain in the final image.

ARG DUCKDB_VERSION=v1.5.4

# ---------------------------------------------------------------------------
# Stage 1: build the extension cdylib + append metadata footer
# ---------------------------------------------------------------------------
# TODO: digest-pin via Renovate
# Pinned to multi-arch index digest (resolved 2026-06-15).
# Renovate will keep these current; see renovate.json at repo root.
FROM rust:1-bookworm@sha256:19817ead3289c8c631c73df281e18b59b172f6a31f4f563290f69cddd06c30e9 AS builder

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
# SHA-256 values were computed from the official release zips on 2026-06-15
# (DuckDB does not publish a separate .sha256 checksum file per release asset;
# the hashes below were obtained by downloading and running sha256sum locally).
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      arm64) \
        ddb_asset="duckdb_cli-linux-arm64.zip"; \
        ddb_sha256="377f03fb9f17ab5a78f28f829cbfcb5333da8ab3c2d0788f27694f81df77ed29" \
        ;; \
      amd64) \
        ddb_asset="duckdb_cli-linux-amd64.zip"; \
        ddb_sha256="1f2fa724fb054b3dbe1a9cbd13de5b76997d850e7087ec762ba88db04e0180cf" \
        ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/duckdb.zip \
      "https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/${ddb_asset}"; \
    echo "${ddb_sha256}  /tmp/duckdb.zip" | sha256sum --check; \
    unzip -o /tmp/duckdb.zip -d /usr/local/bin; \
    chmod +x /usr/local/bin/duckdb; \
    rm /tmp/duckdb.zip; \
    duckdb --version

WORKDIR /src

# Copy the workspace. .dockerignore keeps target/, dist/, .git, ton/build out of
# the build context so this stays lean.
COPY . .

# Build the loadable cdylib (release profile for the production image).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build --release -p p2p-extension; \
    mkdir -p /out; \
    cp target/release/libduckton.so /out/libduckton.so

# Append the DuckDB metadata footer → a loadable .duckdb_extension. The output
# basename MUST be `duckton.duckdb_extension` (DuckDB derives the init symbol
# from it).
RUN set -eux; \
    platform="$(duckdb -list -noheader -c 'PRAGMA platform;' | tr -d '[:space:]')"; \
    echo "duckdb platform: $platform"; \
    python3 scripts/append_extension_metadata.py \
        -l /out/libduckton.so \
        -n duckton \
        -p "$platform" \
        -dv v1.0.0 \
        -ev 0.1.0 \
        -o /out/duckton.duckdb_extension; \
    duckdb -unsigned -c "LOAD '/out/duckton.duckdb_extension'; SELECT value FROM p2p_info() WHERE key='protocol_name';"

# ---------------------------------------------------------------------------
# Stage 2: lean runtime
# ---------------------------------------------------------------------------
# TODO: digest-pin via Renovate
# Pinned to multi-arch index digest (resolved 2026-06-15).
FROM debian:bookworm-slim@sha256:96e378d7e6531ac9a15ad505478fcc2e69f371b10f5cdf87857c4b8188404716 AS runtime

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
COPY --from=builder /out/duckton.duckdb_extension /node/duckton.duckdb_extension
COPY docker/entrypoint.sh /node/entrypoint.sh
RUN chmod +x /node/entrypoint.sh

# Create a non-root user (uid/gid 1001) and prepare the writable state directory.
RUN groupadd --gid 1001 p2pnode \
    && useradd --uid 1001 --gid 1001 --no-create-home --shell /usr/sbin/nologin p2pnode \
    && mkdir -p /node/state \
    && chown -R p2pnode:p2pnode /node/state

# Default QUIC bind (each container has its own netns → same port is fine).
# Donated budget (admission accounting only) must exceed the per-job lease a
# requester dispatches; 256MB admits the lean per-job leases with headroom for
# max_jobs concurrent jobs.
ENV P2P_BIND_ADDR=0.0.0.0:9494 \
    P2P_CONFIG_DIR=/node/state \
    P2P_SHARE_MEMORY=256MB \
    P2P_SHARE_THREADS=1 \
    P2P_SHARE_MAXJOBS=2 \
    P2P_SHARE_PERJOB_BYTES=67108864

EXPOSE 9494/udp

# Health check: load the extension and run a trivial query.
HEALTHCHECK --interval=30s --timeout=10s --start-period=15s --retries=3 \
    CMD duckdb -unsigned -c "LOAD '/node/duckton.duckdb_extension'; SELECT 1 FROM p2p_info() LIMIT 1;" || exit 1

USER p2pnode
ENTRYPOINT ["/node/entrypoint.sh"]
