# Installation

## Option A — Community extension (recommended)

```sql
INSTALL duckton FROM community;
LOAD duckton;
```

!!! warning "DuckDB version must match exactly"
    `duckton` is built against a **specific DuckDB version** (currently
    **v1.5.4**). DuckDB's loadable-extension ABI requires an exact match, so the
    installing DuckDB must be on the matching version. If `INSTALL` reports a
    version mismatch, **upgrade DuckDB first**, then retry.

The community registry rebuilds the extension for each supported platform; see
the [duckdb/community-extensions](https://github.com/duckdb/community-extensions)
listing.

## Option B — Build the loadable extension from source

Requires a stable **Rust toolchain**, the **`duckdb` CLI**, and **`python3`** on
`PATH`. On macOS, set `SDKROOT` for the bundled-DuckDB build.

=== "Linux / macOS"

    ```bash
    git clone https://github.com/Angelerator/duckton.git
    cd duckton
    export SDKROOT=$(xcrun --show-sdk-path)   # macOS only
    scripts/build_extension.sh                # → dist/duckton.duckdb_extension
    duckdb -unsigned -c \
      "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
    ```

=== "Windows (PowerShell)"

    ```powershell
    git clone https://github.com/Angelerator/duckton.git
    cd duckton
    scripts\build_extension.ps1               # needs duckdb CLI + python on PATH
    duckdb -unsigned -c "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
    ```

The `-unsigned` flag is required to load a locally-built (unsigned) extension.

## Platform support

Duckton builds and runs on **Linux, macOS, and Windows** (native glibc / MSVC).
A CI matrix runs build/test/clippy/fmt plus the loadable-extension `LOAD` smoke
test on all three.

Excluded targets (the QUIC/TLS stack — quinn + rustls + ring — and the async
runtime aren't supported there): **WebAssembly**, **musl**, and the
**MinGW/RTools** Windows toolchains.

## Optional build features

| Feature | What it adds | Cost |
|---|---|---|
| _(default)_ | The **mock engine** + full protocol/scheduler/trust stack. | No native deps. |
| `duckdb-engine` | The bundled, **locked-down real DuckDB** engine (off by default). | Compiles DuckDB from source → needs a C/C++ toolchain. |
| `ton-live` | The live on-chain settlement path. | Shells out to `curl` at runtime; off by default. |

## Verify the build (for contributors)

```bash
cargo build --workspace
cargo test --workspace                          # fast suite, mock engine
export SDKROOT=$(xcrun --show-sdk-path)          # macOS only
cargo test -p p2p-node --features duckdb-engine  # real-engine e2e
```

See [Contributing](../project/contributing.md) for the full developer workflow.
