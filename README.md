# Duckton

A peer-to-peer **distributed DuckDB** compute grid, settled on **TON**. Ordinary
machines run DuckDB + the **`duckton`** extension and donate a slice of their
RAM/CPU. A requester broadcasts a query (over data in S3 / ADLS / GCS) to many
hosts; several accept and run it redundantly; the **first correct result wins** and
the rest are cancelled. Machines talk **directly over QUIC** — there is no central
broker in the data path.

```sql
INSTALL duckton FROM community;   -- (once published)
LOAD duckton;
SELECT * FROM p2p_query('SELECT 42 AS x');
```

> The published loadable extension is **`duckton`**; the in-repo crate names stay
> `p2p-*` and the SQL surface stays `p2p_*` (`p2p_query`, `p2p_share`, …).

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
  short-lived credentials** encrypted to the chosen worker. *Status:* the
  coordinator now **attaches a per-job scoped credential** into each `Dispatch`
  when a credential provider is wired (off by default — only when an operator opts
  into remote object-store reads via `storage.enable_remote_access`), and the
  worker resolves it. The credential is currently scoped to the provider root and
  delivered **unsealed**; attestation-bound sealing to the worker's key still needs
  the per-offer attested key handshake (real TEE hardware, see below).
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
**Production nodes ship NO attestor and NO verifier by default** — real L1/L2
attestation needs TPM/TEE hardware that is not yet shipped, so a production node
emits L0 and an L2 (sensitive) policy admits *nobody* until that hardware lands.
The **`console-server` demo** exercises the gate end-to-end *honestly*: its L1/L2
hosts carry a software `MockAttestor` that emits real per-offer, nonce-bound
evidence, and the demo coordinator wires the matching `AllowlistVerifier` — so the
gate is genuine (mock attestor + real verification), not a spoofable integer
compare. That wiring is demo-only and never enabled on a production `Node`. See the
architecture doc for the full reasoning and what remains (per-offer evidence
production + a
network-identity-bound key handshake).

## Documentation

- [**docs/HOW_IT_WORKS.md**](docs/HOW_IT_WORKS.md) — how Duckton works in plain terms:
  the free-vs-paid economic model, **real captured TON testnet proof** of the on-chain
  split (15% treasury / 5% verifier / winner base, free-winner refunds, rejected
  negatives, staking lifecycle), the wallet-setup walkthrough via the extension, the
  four contracts explained, and an honest security/threat model.
- [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) — full architecture, trust mechanism,
  protocol flow, security model, versioning/compatibility, config system, pluggable
  traits, roadmap, and an honest "implementation status & deviations" section.
- [**docs/BLOCKCHAIN_ECONOMICS.md**](docs/BLOCKCHAIN_ECONOMICS.md) — **design only
  (not implemented)**: an optional TON-based economic/incentive layer (wallet↔identity
  binding, provider earnings formula, stake-weighted bidding/selection, slashing,
  payment channels, and on-chain Merkle-root-anchored job records) that augments the
  trust model above.

## Live on-chain proof (TON testnet)

Duckton settles on TON with a simple, non-custodial economic model. **Free queries
run entirely off-chain** — no chain client, no escrow, no fees. A **paid** query
locks the requester's max bid `B` in a per-job `JobEscrow`; on settle the contract
pays the **winner** its quoted base, a **15% platform fee** to the admin treasury
(`GlobalParams.fee_recipient`, enforced on-chain), a **5% commission** to each
agreeing wallet verifier, and **refunds the remainder to the requester**. A
**free (walletless) winner** is paid nothing and its base is refunded — but the
15% fee and 5% commission are *still* collected, so the platform and verifiers
earn on every paid job regardless of the node mix.

The flows below were **broadcast live on TON testnet** and read back from chain.
Amounts are scaled to **1/10** of the prior full-amount run (base = 0.004 TON) for
a low-cost re-verification; the **split percentages (15% / 5%) are identical**. The
full-amount proof — all seven scenarios incl. the complete staking
deposit → 1:1 receipt mint → unbond lifecycle and every negative — is in
[**docs/HOW_IT_WORKS.md**](docs/HOW_IT_WORKS.md).

Each escrow permanently retains `MIN_TONS_FOR_STORAGE` = **0.05 TON** as a storage
reserve (funded from the deploy buffer, not from `B`), and every payout leg pays a
tiny per-message storage rent (~0.00006 TON) on landing — so a recipient's balance
delta is its gross leg minus that rent, and the numbers reconcile exactly.

| # | Scenario | Real captured money flow (TON) | On-chain |
|---|---|---|---|
| 1 | **Paid, wallet winner** | winner `+0.003941` (base 0.004) · treasury `+0.000541` (15%) · verifier `+0.000141` (5%) · requester refund `0.029048` · escrow keeps `0.05` reserve | [escrow](https://testnet.tonviewer.com/kQAu9lXxoz85k_Vybi0gc7L-qOaha2LbWl9pncoL5ZCNVaYz) |
| 2 | **Paid, free (walletless) winner** | winner `0` · base `0.004` refunded to requester · treasury `+0.000548` (15%, still collected) · verifier `+0.000148` (5%) · escrow keeps `0.05` | [escrow](https://testnet.tonviewer.com/kQA7gKDku9jO_ejMabspoXraox9a1U9zXLg3wnavlIo9KCBy) |
| 4 | **Staking 7-day lock** | immediate `StakeWithdraw` → on-chain abort `exit_code=203 COOLDOWN_NOT_ELAPSED`; `readyAt` = `unbondingAt + 604800` = **exactly 7 days**; receipt jetton `0.1` minted 1:1 and transfer-locked; vault state unchanged | [vault](https://testnet.tonviewer.com/kQAYPc8qAo5YUKpgcTANIAi1umHrEhrp2nG_Lnkycvo0G29q) |

Verified live and reused (not redeployed): the
[`GlobalParams`](https://testnet.tonviewer.com/kQC_cuafJQo9cycuivJfPHE5XGMrvZUP-1sN1Kq0jtZg3dna)
singleton (`platform_fee_bps=1500`, `participation_commission_bps=500`,
`fee_recipient` = treasury). Full per-scenario addresses, tx hashes and balance
deltas are captured in `ton/deployments/readme_proof.testnet.env`.

Proven at **full amounts** in the prior run (not re-broadcast here, to save gas) —
see [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md): the **wrong-fee** reject
(`exit_code=285 FEE_MISMATCH`), the **under-funded** reject
(`exit_code=226 PAYOUT_EXCEEDS_ESCROW`), the **mismatched-treasury** honest-
coordinator refusal (escrow `get_fee_recipient` ≠ `GlobalParams.fee_recipient`),
and the full staking deposit → receipt-mint → unbond lifecycle.

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
  extension/   duckton        loadable DuckDB C-API extension (table functions)
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
duckdb -unsigned -c "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
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
