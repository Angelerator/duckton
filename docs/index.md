# Duckton

**Turn DuckDB into a node of a trustless, peer-to-peer query grid.** Ordinary
machines run DuckDB plus the **`duckton`** extension and donate a slice of their
RAM/CPU. A requester broadcasts a query (over data in S3 / ADLS / GCS); several
independent hosts accept and run it **redundantly** over **QUIC**; the **first
result that reaches a verifiable quorum wins** and the rest are cancelled. There
is **no central broker in the data path**.

```sql
INSTALL duckton FROM community;
LOAD duckton;
SELECT * FROM p2p_query('SELECT 42 AS x');   -- runs locally + verified, out of the box
```

!!! tip "It works with zero configuration"
    With no peers configured, `p2p_query` runs on a **free, in-process,
    locked-down DuckDB** (no network egress, no local-filesystem access,
    configuration locked). It's useful and safe the moment you `LOAD` it — point
    it at a swarm only when you want to.

## Why it exists

DuckDB is a fast in-process engine, but it can't *prove* a result you didn't
compute yourself. Duckton answers **"can I trust a result a stranger computed?"**
with **cryptography instead of faith**:

- A query is dispatched to several independent hosts that run it redundantly.
- Each result is reduced to a canonical, order-independent **BLAKE3 hash**.
- An answer is accepted only once a configurable **quorum** of those hashes agree
  **byte-for-byte** — a single broken or dishonest host can't slip a wrong answer
  past the quorum.

It's distributed compute you can actually verify, all from plain SQL table
functions, with no external daemon to run.

## Core ideas

| Pillar | What it means |
|---|---|
| **Transport** | QUIC (TLS 1.3, mutual auth, multiplexed streams) via Quinn + rustls — nothing is readable on the wire. |
| **Identity** | Ed25519 node identities, Sybil-resistant (proof-of-work + vouching), pinned to the TLS certificate. |
| **Verification** | Canonical result hashing, quorum agreement, and randomized canary audits. |
| **Trust** | Reputation from signed, gossiped receipts + attestation tiers (L0/L1/L2) + stake weighting feed host selection. |
| **Hedged execution** | Race `k` workers, accept the first result that reaches quorum, `RESET` the losers. |
| **Data at rest** | Lives in cloud object storage, encrypted (Parquet Modular Encryption); hosts are pure compute with per-job scoped, short-lived credentials. |
| **Economics (optional)** | Public jobs are free and fully off-chain; paid jobs settle through a per-job escrow on **TON** — strictly opt-in, default-off. |

## Honest security boundary

Transport, at-rest encryption, and result integrity are protected on **any**
machine. **Confidentiality from a malicious host operator's RAM is only
achievable on confidential-computing hardware (TEEs)** — commodity laptops can't
guarantee it, so sensitive data is routed only to hosts that present a verified
attested tier. See [Security](project/security.md) and the
[Architecture deep dive](ARCHITECTURE.md) for the precise, no-hand-waving model.

## Where to go next

<div class="grid cards" markdown>

- :material-rocket-launch: **[Quickstart](getting-started/quickstart.md)** — install and run your first verified query.
- :material-school: **[Core concepts](getting-started/concepts.md)** — coordinator, worker, hedging, quorum, trust tiers, free vs paid.
- :material-database-search: **[SQL functions](reference/sql-functions.md)** — the complete `p2p_*` surface.
- :material-cog: **[Configuration](reference/configuration.md)** — the layered config system and key knobs.
- :material-currency-usd: **[Paid queries (TON)](guides/paid-queries.md)** — wallet setup and the on-chain split.
- :material-office-building: **[Private / enterprise mode](PRIVATE_MODE.md)** — a fully closed company grid.

</div>

## Project status

Phases 0–4 are implemented and tested; Phase 5 is scaffolded. The
[Architecture](ARCHITECTURE.md) document includes an honest "implementation
status & deviations" section spelling out exactly what is real vs. mocked (mock
attestor for TEE, local-fake object storage, etc.). The codebase is Apache-2.0
and builds on Linux, macOS, and Windows.
