# Contributing

Duckton is Apache-2.0 and welcomes contributions. The repository is
[Angelerator/duckton](https://github.com/Angelerator/duckton).

## Workspace layout

```text
crates/
  config/      p2p-config     layered, validated config (defaults < file < env < per-call)
  proto/       p2p-proto      wire messages, identity, attestation, versioning, value model
  transport/   p2p-transport  Quinn QUIC + mTLS pinned to Ed25519 identities
  trust/       p2p-trust      canonical hashing, quorum, receipts, reputation, canary, Sybil, sealing
  node/        p2p-node       coordinator (hedging), worker (admission), discovery, engines, storage
  extension/   duckton        loadable DuckDB C-API extension (the p2p_* table functions)
  settlement/  p2p-settlement TON settlement: escrow/stake/anchor builders, wallet, BoC/cell
ton/                          TON smart contracts (Tolk) + tests + deploy scripts (Acton)
config/p2p.example.toml       documented example configuration
docs/                         this documentation site (MkDocs Material)
```

## Toolchain

CI pins the Rust toolchain to **1.85.0** — format and test with the same to avoid
drift:

```bash
rustup toolchain install 1.85.0 --component rustfmt clippy
cargo +1.85.0 fmt --all
```

## Build & test

```bash
cargo build --workspace
cargo +1.85.0 fmt --all --check
cargo clippy --workspace
cargo test --workspace                          # fast suite (mock engine)

# Real locked-down DuckDB engine + e2e (compiles DuckDB):
export SDKROOT=$(xcrun --show-sdk-path)          # macOS only
cargo test -p p2p-node --features duckdb-engine
```

!!! note "Don't run `cargo test --workspace` with the extension + duckdb-engine together"
    The extension links DuckDB in *loadable* mode while `duckdb-engine` bundles it;
    unifying both in one test process triggers a `libduckdb-sys` init clash. Test
    the real engine via `-p p2p-node --features duckdb-engine`, and the extension
    separately.

## Smart contracts (TON / Tolk)

The contracts live in `ton/` and use the **Acton** toolchain:

```bash
cd ton
acton build         # compile contracts
acton test          # run the on-chain (emulator) test suite
acton wrapper <C>   # regenerate a contract wrapper after changing its messages/storage
```

## The loadable extension

```bash
scripts/build_extension.sh                       # → dist/duckton.duckdb_extension
duckdb -unsigned -c "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
```

## The documentation site

This site is MkDocs Material:

```bash
python3 -m pip install -r requirements-docs.txt
mkdocs serve     # preview at http://127.0.0.1:8000
mkdocs build     # render to ./site
```

It deploys to GitHub Pages automatically on push to `main` (see
`.github/workflows/docs.yml`).

## Pull requests

- Keep the tree green: build + the test suites + `fmt --check` + clippy, and the
  loadable-extension `LOAD` against a **matching** DuckDB CLI.
- Don't commit secrets (`*.mnemonic`, `*.wallets.toml`, `.env` with real values
  are gitignored). The public testnet proof env (addresses/tx hashes only) is fine.
- For a contract change, run `acton build && acton test` and regenerate affected
  wrappers.

## Releasing the published extension

Shipping a new `duckton` version to the DuckDB community registry: bump the
workspace version, keep the `duckdb`/`libduckdb-sys` crate versions and the
`Makefile` `TARGET_DUCKDB_VERSION` aligned to the registry's DuckDB version, tag
the release, then open a PR updating `extensions/duckton/description.yml` (a fresh
PR per release) in
[duckdb/community-extensions](https://github.com/duckdb/community-extensions).
