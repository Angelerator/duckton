# DuckGrid Console

A web console for the **duckdb-p2p** project — a peer-to-peer distributed DuckDB
compute grid over QUIC. It surfaces every part of the system (query dispatch,
discovery, workers, trust/attestation, hedged execution, QUIC transport tuning,
storage, and the optional TON settlement layer) as an operator-friendly UI.

## Live mode (realtime, reactive)

By default the console reads a point-in-time `snapshot.json`. For **live, realtime**
data, run the backend grid service — then every chart updates continuously and a
query you run **ripples across the whole app**.

```bash
# terminal 1 — the live grid (real coordinator + workers, axum SSE on :8787)
cargo run -p console-server

# terminal 2 — the web console
cd web && npm run dev
```

- The backend (`crates/console-server`) boots a **real loopback-QUIC grid** and runs
  ambient jobs continuously; it exposes `GET /api/state`, `GET /api/stream` (SSE), and
  `POST /api/query`.
- The frontend (`src/lib/live.tsx`, `LiveProvider`/`useLive`) subscribes to the SSE
  stream and overlays live data on the snapshot. The header shows **LIVE** when
  connected; if the backend is offline it falls back to the snapshot automatically.
- **Dispatch from the Query Console** → a *real* job runs on the grid and shows up
  live in **Jobs**, bumps the **Overview** KPIs/latency series, and lights up the
  **Network** node-communication graph. Worker trust/capacity update as jobs run
  (the cheat/fail nodes' trust really drops).
- Point the frontend at a different backend with `NEXT_PUBLIC_LIVE_URL` (default
  `http://localhost:8787`).

## Real data

This console is **not** backed by mock data. Everything it shows is produced by the
**actual duckdb-p2p crates**. A Rust exporter
(`crates/node/tests/console_export.rs`) brings up a real in-process **loopback-QUIC
grid** — a real coordinator and 8 heterogeneous workers (varied attestation tiers,
capacity, latency, plus a cheating node and a failing node) — runs a batch of real
queries, and serializes everything it observes to `src/data/snapshot.json`:

- **Workers & trust** — real `effective_trust` from the `p2p-trust` engine (the
  cheat/fail nodes really earn `Incorrect`/`Timeout` verdicts, drop to trust 0, and
  get deselected by trust-weighted ranking).
- **Jobs** — real hedged execution: winner, agreed quorum hash, per-candidate
  commit latencies and verdicts, real signed receipts.
- **Verification** — real BLAKE3 canonical hashing (order-independent) and real
  quorum evaluation.
- **Settlement** — real escrow open/settle events, the real payout split
  (winner + κ·B commissions + φ·B fee = escrow B), a real epoch Merkle root with a
  verifying inclusion proof, and a real two-way `ton_proof` wallet↔node binding that
  cryptographically verifies.
- **On-chain (TON)** — the full real contract layer: the on-chain `GlobalParams`
  bps-encoded into a TON cell, real opcodes, a per-job `JobEscrow` address derived
  offline from the compiled contract code (HTLC-locked on the real quorum hash), and
  real message BoCs — all computed by `p2p_settlement`. Plus the **real verified
  testnet deployment**: contract addresses, code hashes, `acton verify` results, the
  measured gas baseline, the e2e scenario checks, and the proven in-place SETCODE
  upgrade — parsed from the repo's `ton/` artifacts.
- **Transport** — the real `GridConfig` QUIC knobs plus a **real loopback benchmark
  sweep** (`cargo test --test benches`) measured on this machine.
- **Config / Protocol** — the real serialized `GridConfig` (17 sections), the real
  documented `p2p.example.toml`, and real serialized wire-message instances.

### Regenerate the data

```bash
web/scripts/generate-data.sh      # runs the exporter + bench sweep, writes src/data/snapshot.json
```

(Requires the Rust toolchain. Uses the fast mock execution engine — no DuckDB
compile needed. The protocol, trust, settlement, hashing, config and transport are
all real.)

## Connect wallet & deploy on-chain

The console is also a real dApp. Connect a **TON wallet** (TON Connect — Tonkeeper,
MyTonWallet, …) from the header, then on **/deploy** edit the on-chain economic
parameters (GlobalParams) and **deploy the settlement contracts** to testnet or
mainnet — your wallet signs and broadcasts. Highlights:

- The init-data cells are built in the browser with **@ton/core**, matching the Tolk
  contract storage layouts. The GlobalParams **EcoParams encoding is verified at
  runtime** to reproduce the exact hash the Rust settlement crate computes, and each
  contract's code BoC is re-hashed against its recorded code hash.
- The deploy target follows the **testnet/mainnet** toggle (the transaction is tagged
  to that chain; the wallet rejects a wrong-network send). Mainnet shows a real-funds
  warning.
- Config can be exported as the `[economics]` TOML the node reads.

> Deploys are real on-chain transactions — start on testnet and fund the wallet from a
> faucet. The wallet-connect/build/encode/verify path is `src/lib/ton-connect.tsx`,
> `src/lib/ton-build.ts`, and `src/app/deploy/`.

## Explanations

Every page opens with a plain-language **Explainer** callout (what it shows + how it
impacts you), jargon terms carry **InfoHint** "?" tooltips, and a dedicated
**/glossary** page defines every term with a "what / impact" pair. The glossary data
lives in `src/lib/glossary.ts`.

## Network mode

A **testnet / mainnet** toggle in the header (persisted to localStorage). Testnet
shows the real deployed+verified contracts, RPC and explorer; selecting **mainnet**
raises an app-wide real-funds warning and the `/ton` panel reflects that mainnet is
guarded (`economics.mainnet_confirmed = false`) and not deployed — the node refuses
on-chain mainnet settlement until explicitly enabled.

## Plots

Interactive **Plotly** visualizations (themed, client-loaded) across the app:
a **circular node-communication graph** (`/network`) built from the real receipts +
quorum agreement; a trust-terms **radar** and latency **histogram** (`/trust`); an
escrow **waterfall** and stake-factor **curve** (`/settlement`); a per-worker latency
**box** and throughput **line** (`/transport`); the gas **bar** (`/ton`); and a latency
**histogram** + worker×job **heatmap** (overview). All driven by the real snapshot.

## Stack

- **Next.js 16** (App Router, Turbopack, React 19, fully static-prerendered)
- **Tailwind CSS v4** (CSS-first theme, dark-first) · **shadcn/ui** primitives
- **Plotly** (`plotly.js-dist-min` + `react-plotly.js`) for the analytics + graph
- **Recharts 3** charts · **lucide-react** icons

## Run

```bash
cd web
npm install
npm run dev      # http://localhost:3000
npm run build && npm run start   # production
```

## Pages

| Route         | What it shows (real data) |
|---------------|---------------|
| `/`           | Overview — workers, real per-job latency, verified/failed, attestation mix |
| `/query`      | Query Console — compose `p2p_query`/`join`/`share`; the simulation is seeded from the real grid |
| `/jobs`       | Real jobs — lifecycle timeline + the k-worker race (commit-first, quorum, RESET) |
| `/network`    | Real swarm + capability records + the real discovery/NAT config (loopback run) |
| `/workers`    | Real workers — capacity, attestation, and trust computed by `p2p-trust` |
| `/transport`  | Real QUIC config + the real measured loopback benchmark sweep |
| `/trust`      | Real trust formula + reputation, real canonical-hash & quorum, real receipts, flagged providers |
| `/storage`    | Object-store providers, formats, scoped credentials, encryption boundary + real storage config |
| `/settlement` | Real escrow/payout split, stake factors, slashing schedule, epoch anchor proof, verified binding |
| `/ton`        | Full on-chain layer — deployed+verified testnet contracts, opcodes, GlobalParams cell, HTLC escrow, gas, SETCODE upgrade, Duckton |
| `/config`     | The real serialized `GridConfig` (17 sections) + the documented `p2p.example.toml` |
| `/protocol`   | Real serialized wire messages, the `Wire` envelope, verdicts, and version negotiation |
| `/glossary`   | Plain-language definitions of every term — what it means and how it impacts you |
| `/deploy`     | Connect a TON wallet, edit the GlobalParams config, deploy contracts to testnet/mainnet |

## Architecture

```
src/
  app/                 one route per folder (server components + client islands)
  components/{ui,common,shell}/   shadcn primitives, shared atoms/charts, app shell
  lib/
    types.ts           the Snapshot schema + row types (mirror the Rust crates)
    data.ts            real data layer over src/data/snapshot.json (single source of truth)
    format.ts          formatting helpers; NOW is pinned to the snapshot generation time
    nav.ts             sidebar navigation
  data/snapshot.json   ← generated by crates/node/tests/console_export.rs
scripts/generate-data.sh   regenerate the snapshot from the real system
```

To wire this to a *live* node instead of a point-in-time snapshot, replace the
imports in `src/lib/data.ts` with `fetch()` calls to a node's status API, keeping the
shapes from `src/lib/types.ts`.
