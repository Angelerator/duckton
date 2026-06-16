# duckdb-p2p

A peer-to-peer **distributed DuckDB** compute grid. Ordinary machines run DuckDB +
this extension and donate a slice of their RAM/CPU. A requester broadcasts a query
(over data in S3 / ADLS / GCS) to many hosts; several accept and run it redundantly;
the **first correct result wins** and the rest are cancelled. Machines talk **directly
over QUIC** — there is no central broker in the data path.

## Why

DuckDB is an in-process engine. The new [Quack protocol](https://duckdb.org/docs/current/core_extensions/quack)
turns it into a single client-server database. This project goes further: a
**decentralized, many-host grid** with a built-in **trust model** so a requester can
reason about which untrusted hosts to trust with a job.

## Core ideas

- **Transport:** QUIC (TLS 1.3 mandatory, mutual auth, multiplexed streams) via
  **Quinn + rustls**. Nothing is readable on the wire.
- **Data:** lives in cloud object storage, encrypted at rest (Parquet Modular
  Encryption). Hosts are pure compute; the design delivers per-job **scoped,
  short-lived credentials** encrypted to the chosen worker. *Status:* the scoped
  cloud-read path (sealed credential → prefix-scoped DuckDB secret) is built and
  unit-tested, but the coordinator does **not yet attach per-job credentials**
  (`credential: None`), so the object-store read path is not exercised end-to-end.
- **Trustworthiness:** identity (Ed25519, Sybil-resistant) + **attestation tiers**
  (anonymous / TPM measured-boot / hardware TEE) + **reputation from signed receipts**
  + **verification** (canonical result hashing, quorum agreement, canary audits).
- **Hedged execution:** race `k` workers, accept the first result that reaches quorum,
  `RESET` the losers.

## Honest security boundary

Transport, at-rest, and integrity are protected on any machine. **Confidentiality from
a malicious host operator's RAM is only achievable on confidential-computing hardware
(TEEs).** Commodity laptops cannot guarantee it — so sensitive data is routed only to
hosts **claiming** an attested tier (L2).

*Attestation status (be precise):* requester selection no longer trusts a
self-reported level — a bid claiming **> L0** is honored only if its evidence
verifies against a wired `AttestationVerifier` (trusted-authority signature over an
allowlisted measurement + the offer nonce); absent/invalid evidence is treated as
**L0** (the `> L0` gate fails closed, so a spoofed level can't reach sensitive data).
However, **real L1/L2 attestation needs TPM/TEE hardware that is not yet shipped** —
**all current hosts emit L0**, and no production `AttestationVerifier` is wired by
default, so today an L2 (sensitive) policy admits *nobody*. See the architecture doc
for the full reasoning and what remains (per-offer evidence production + a
network-identity-bound key handshake).

## Documentation

- [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) — full architecture, trust mechanism,
  protocol flow, security model, versioning/compatibility, config system, pluggable
  traits, roadmap, and an honest "implementation status & deviations" section.
- [**docs/BLOCKCHAIN_ECONOMICS.md**](docs/BLOCKCHAIN_ECONOMICS.md) — **design only
  (not implemented)**: an optional TON-based economic/incentive layer (wallet↔identity
  binding, provider earnings formula, stake-weighted bidding/selection, slashing,
  payment channels, and on-chain Merkle-root-anchored job records) that augments the
  trust model above.

## Workspace layout

```
crates/
  config/      p2p-config     layered, validated config (defaults<file<env<per-call)
  proto/       p2p-proto      wire messages, identity, attestation, versioning, value model
  transport/   p2p-transport  Quinn QUIC + mTLS pinned to Ed25519 identities; version handshake
  trust/       p2p-trust      canonical hashing, quorum, receipts, reputation, canary,
                              Sybil PoW/vouch, capability tokens, attestation, sealing
  node/        p2p-node       coordinator (hedging), worker (admission), discovery,
                              membership, query engines (mock + locked-down DuckDB), storage
  extension/   duckdb_p2p     loadable DuckDB C-API extension (table functions)
config/p2p.example.toml       documented example configuration
scripts/                      build_extension.sh, append_extension_metadata.py
```

## Build & test

Requires a Rust toolchain (stable). DuckDB CLI + `python3` enable the extension
load test; macOS bundled-DuckDB builds need `SDKROOT`.

```bash
# Core suite (fast; mock engine; ~130 tests)
cargo test --workspace

# Real locked-down DuckDB engine + DuckDB-backed e2e scenarios (compiles DuckDB)
export SDKROOT=$(xcrun --show-sdk-path)          # macOS only
cargo test -p p2p-node --features duckdb-engine

# Build the loadable DuckDB extension and smoke-test it in the duckdb CLI
scripts/build_extension.sh
duckdb -unsigned -c "LOAD 'dist/duckdb_p2p.duckdb_extension'; SELECT * FROM p2p_info();"
```

The end-to-end **scenario suite** lives in `crates/node/tests/scenarios.rs`
(functional, hedging/trust, admission, versioning, config, resilience/churn),
`crates/node/tests/scenarios_duckdb.rs` (real-engine e2e + sandbox, feature-gated),
and `crates/extension/tests/load.rs` (extension LOAD via the duckdb CLI).

## Platform support

The workspace builds and runs on **Linux, macOS, and Windows**; a CI matrix
(`.github/workflows/ci.yml`) runs build/test/clippy/fmt plus the loadable-extension
LOAD smoke test on all three. Notes:

- **Windows** is supported as a host for the **loadable extension**. Build it with
  `scripts/build_extension.ps1` (the PowerShell mirror of `build_extension.sh`);
  it needs the `duckdb` CLI and a Python interpreter on `PATH`.
- The **`duckdb-engine`** feature (the bundled, locked-down DuckDB engine, off by
  default) compiles DuckDB from source, so it needs a working **C/C++ toolchain**
  (MSVC Build Tools on Windows; Xup/Command-Line Tools on macOS — set
  `SDKROOT=$(xcrun --show-sdk-path)`; a C++ compiler on Linux). The default mock
  engine needs none of this.
- The **`ton-live`** settlement path shells out to a **`curl`** executable at
  runtime (present by default on modern Windows/macOS/Linux); it is off by default.
- Per-user secret/runtime files are restricted to the owner: `0600`/`0700` on Unix,
  and an owner-only protected DACL on Windows.

> **OS sandbox status:** the host now wires the configured `[sandbox]` policy +
> anti-abuse runtime into the live worker, but the **default backend is `noop`**
> (no OS isolation — jobs run in-process under the DuckDB configuration lockdown).
> Real OS enforcement (rlimits / Seatbelt / cgroups / Job Object) is **opt-in**
> via `[sandbox].process_per_job` + the `p2p-job-exec` child binary (architecture
> §9.4); the in-process path is the default and what runs unless you enable it.

## Transport performance tuning

QUIC is tuned for low latency + high throughput, with everything configurable
under `[transport]` in the config (see `config/p2p.example.toml` and
ARCHITECTURE.md §20): UDP **GSO/GRO** offload, **flow-control windows** (sized
directly or from a bandwidth-delay-product target), **congestion control**
(`bbr`/`cubic`/`newreno`) + pacing, **parallel result streaming** over multiple
unidirectional QUIC streams (per-call overridable), optional **wire compression**
(`none`/`lz4`/`zstd`, default off on LAN), and **0-RTT/session resumption**.

A loopback **throughput + latency benchmark** lives in
`crates/node/tests/benches.rs`:

```bash
# defaults are small & CI-fast; print the numbers with --nocapture
cargo test -p p2p-node --test benches -- --nocapture
# scale it up:
P2P_BENCH_ROWS=200000 P2P_BENCH_PARALLELISM=4 P2P_BENCH_COMPRESSION=zstd \
  cargo test --release -p p2p-node --test benches -- --nocapture
```

## Status

Phases 0–4 implemented and tested; Phase 5 scaffolded. See the roadmap and the
"implementation status & deviations" section in the architecture doc for exactly
what is real vs. mocked (mock attestor for TEE, local-fake object storage, etc.).
