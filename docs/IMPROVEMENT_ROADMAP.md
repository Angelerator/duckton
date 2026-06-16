# duckdb-p2p — Capstone Analysis & Improvement Roadmap

> Status: analysis/roadmap deliverable. **No features are implemented here.** Every
> claim is grounded in the real code (file:line) and the just-produced test
> evidence: the off-chain 100-node swarm bundle at `/tmp/p2pgrid_capstone/`
> (**100/100 nodes, 152/152 PASS**), the on-chain TON testnet runs
> (`testnet_e2e` 14/14 live, comprehensive runner 21/21 positive live, Acton
> emulator 90/90), and the catalog specs in `docker/SCENARIOS.md` + `ton/SCENARIOS.md`.
>
> **Scale caveat:** the 100-node run is an **ephemeral breadth check** — each
> catalog scenario run **once** against **mostly-idle** workers — not a
> sustained-throughput/load benchmark. The **reproducible in-repo harness is 16
> nodes** (`docker/REPORT.md`). Read "100-node / 152-PASS" as breadth-at-scale,
> not a capacity result (see §3.5).

---

## 1. Executive summary

duckdb-p2p is a genuinely impressive, mostly-finished distributed query grid that
loads as a single DuckDB extension and "just works" local-first and free with
zero config. The 100-node heterogeneous swarm passed the **entire** off-chain
catalog (`scenario_results.txt`: 152 PASS / 0 FAIL) at ~11.2 MiB/node idle
(`INDEX.md` headline), and the TON settlement contracts are real, upgradeable
(in-place SETCODE → V2), and proven live on testnet. The engineering quality is
high: clean trait-per-collaborator seams, careful fail-closed security gates, and
honest, self-documenting code comments that flag their own deferred work.

The project's central tension is **maturity asymmetry**. Three subsystems are
production-grade and battle-tested (the local-first/zero-config requester path,
the resilient hedged scheduler, and the on-chain contracts). Three are
**scaffolded but not load-bearing in production**: (a) the economics money rail
is wired end-to-end only for `mock`/`noop`; the live `ton` paid path is
admittedly blocked on the un-threaded `JobEscrow` candidate-commitment seam
(`coordinator.rs:1554-1563`); (b) the pre-flight working-set **estimator** is a
rich, well-tested module that **nothing in the live query path ever calls**
(`run_query` always passes `None`, `coordinator.rs:475`); and (c) the
**self-learner** loop has its foundation laid (signed `CapabilityProfile` store,
measured-capability receipts) but the host never feeds its own executions into it
(`CapabilityStore::observe` has no production caller — grep: tests only).

A fourth, cross-cutting limitation is **observability**: the extension surfaces
only result *rows* to SQL and discards the rich `QueryOutcome` metadata
(`extension/src/lib.rs:1471` drops everything but `.result`), and the live host
engine has **no fault-injection knob** (`HostEngine`/`Worker` always run real
SQL). This is precisely why the harness is two-tier — adversarial and metadata
invariants are only provable at the cargo/library tier, never live.

The headline judgment: **this is a strong system whose remaining work is mostly
"close the last mile on already-designed seams," not "invent new mechanisms."**
The highest-leverage moves are unglamorous wiring tasks with outsized payoff.

### One-screen scorecard

Ratings: ⭐⭐⭐⭐⭐ exceptional · ⭐⭐⭐⭐ strong · ⭐⭐⭐ solid w/ gaps · ⭐⭐ partial/scaffolded · ⭐ absent.

| Quality | Rating | Headline evidence |
|---|---|---|
| Simple / least-config | ⭐⭐⭐⭐⭐ | Zero-config local-first works with no `p2p_join`/`p2p_share`/file/env (`node.rs:87-164`, `QRY-LOCAL-01`); seed-1 log shows `economics_enabled=false`, `public_jobs=free` by default. |
| Powerful | ⭐⭐⭐⭐ | Full SQL admin surface (46 ADM-* PASS), grid quorum returns `500500` over QUIC (`QRY-REMOTE-OK-01`), real estimator + 13 wire/transport features — but estimator is dead in prod (§3.11). |
| Highly customizable | ⭐⭐⭐⭐⭐ | Layered config (default→file→env→SQL) + per-call `QueryOverrides` (10 named params, `lib.rs:1528-1542`); 46 admin scenarios + `ADM-SET-03` invariant-not-persisted-on-fail. |
| Modular | ⭐⭐⭐⭐⭐ | 8 crates, trait-per-collaborator (`Discovery`/`TrustStore`/`QueryEngine`/`LocalOrRemotePlanner`/`Settlement`/`StakeRegistry`/`RecordAnchor`), each with mock+real impls. |
| Scalable | ⭐⭐⭐⭐ | 100 containers, 152/152 PASS, ~1.12 GiB total idle; paid tier 18/20→**20/20** at 100 nodes (`INDEX.md` delta). Single-requester coordinator + `block_on` extension bridge are untested ceilings (§3.5). |
| Solid | ⭐⭐⭐⭐ | 152 live+lib + 90 emulator + 14/14 testnet, exact value/error/log assertions; `ADM-PEERS-02` survives corrupt config. Estimator/self-learner code carried but unexercised in prod. |
| Trustable | ⭐⭐⭐⭐ | Confidence-aware (Wilson) reputation, signed receipts, fail-closed `require_staked_hosts` (`QRY-REQUIRE-STAKED-NOREG-01`), upgradeable audited contracts; but no live stake registry wiring. |
| Fast | ⭐⭐⭐⭐ | Hedged race + commit-first + immediate loser RESET (`coordinator.rs:1018-1032`), parallel result streams, lz4/zstd. No latency benchmarks captured; estimator-driven local routing inactive. |
| Unbreakable | ⭐⭐⭐⭐ | Re-dispatch around 4 `docker kill`ed nodes still reached quorum (`02_chaos`, `RES-*` 12/12); retry budget + backoff + wall-clock cap; OS sandbox + DuckDB lockdown (`SBX-*`). Liveness/phi off by default. |
| Anti-cheat | ⭐⭐⭐⭐ | Quorum + minority-cheat penalty (`QRY-MINORITY-CHEAT-01`), canary, nondeterminism detection, deny-list (8 ABU-* live) — but all adversarial proofs are **library-tier only** (no live fault injection). |
| Self-improver / learner | ⭐⭐ | Foundation present: grid-wide measured `observe_capability` IS wired (`coordinator.rs:1203`) + signed `CapabilityProfile` store, but `capability_weight` defaults 0 (no-op) and self-measurement → profile → gossip loop is **unclosed**. |

---

## 2. How to read this document

For each quality below: **Current state** (code + test evidence), **Gaps/risks**
(what the 100-node and on-chain runs exposed or could not prove), and
**Recommendations** tiered Quick win / Medium / Larger with the exact
files/modules to change and the expected payoff. Confirmed gaps are stated as
fact with citations; hypotheses are labeled *(hypothesis)*.

---

## 3. Per-quality deep analysis

### 3.1 Simple to use / least-config (zero-config defaults) — ⭐⭐⭐⭐⭐

**Current state.** This is the project's strongest quality and it is real.
`Node::auto` / `Node::with_config` (`crates/node/src/node.rs:87-164`) assemble a
working requester from nothing but `GridConfig::default`: ephemeral Ed25519
identity, loopback QUIC on an OS-assigned port, empty bootstrap ⇒ local-first,
free/no-chain payment. `Node::query` (`node.rs:283-333`) does the friendly thing
automatically: in `auto` with no reachable grid it rewrites `prefer→Local` and
runs in-process for free, and it gracefully falls back local on
`NoCandidates`/`InsufficientWorkers` unless the user pinned remote.
Evidence: `QRY-LOCAL-01` (`executed_locally=true, quorum=0, verified=true`),
`QRY-REMOTE-FALLBACK-01`, the zero_config suite (10 passed,
`units_zeroconfig.log`), and the seed-1 status log showing safe defaults
(`network=testnet`, `economics_enabled=false`, `public_jobs=free`,
`settlement=noop`). The friendly error strings are first-class
(`CoordinatorError::NoCandidates` literally tells the user to call `p2p_join`,
`coordinator.rs:51-57`).

**Gaps/risks.** (1) The single biggest *latent* footgun is that the polished
zero-config story stops the moment a user types `payment => 'paid'`: that path is
non-functional on the live chain (§3.7). The error is friendly
(`NodeError::WalletRequired`, `node.rs:59-65`) but a user who *does* configure a
wallet still hits the un-threaded escrow seam silently. (2) `ADM-PEERS-02`
proves a corrupt `P2P_CONFIG` degrades to a single diagnostic row instead of
failing `LOAD` — excellent — but there is no `p2p_doctor`-style "is my node
healthy / why did my last query route local?" surface, so a misconfigured grid
*looks* identical to a healthy local-only node.

**Recommendations.**
- **Quick win** — Add a `p2p_explain_routing('SELECT …')` table function
  (`extension/src/lib.rs`, mirroring `QueryVTab`) that calls the planner and
  returns the `PlanDecision { route, reason }` (already a clean enum,
  `planner.rs:30-70`) without executing. Payoff: turns the invisible local-vs-grid
  decision into a one-line diagnostic; directly closes the "looks healthy but
  isn't" gap.
- **Medium** — Surface a `p2p_status` row for "grid reachable? N candidates
  responding" by reusing `Discovery::find_candidates` at status time. Payoff:
  the #1 support question ("why is my query not using the grid?") becomes
  self-serve.
- **Larger** — Make the paid path *fail loudly at config time* rather than at
  query time when the live escrow seam is unthreaded (§3.7), so "simple" never
  silently means "broken."

### 3.2 Powerful — ⭐⭐⭐⭐

**Current state.** The capability surface is broad and genuinely distributed: a
full SQL admin/economics/wallet/contracts surface (46 `ADM-*` PASS), real grid
dispatch returning computed values over QUIC (`QRY-REMOTE-OK-01` → `500500`),
quorum verification with winner selection and signed receipts
(`coordinator.rs:946-1257`), parallel/compressed result streaming
(`result_stream`, `TRN-*`, `QRY-INFLIGHT-RESULTSTREAM-01`), and a sophisticated
pure-Rust pre-flight estimator with projection/predicate pushdown across
Parquet/Delta/Iceberg/CSV/JSON + EXPLAIN parsing (`estimator.rs`, all unit-tested).

**Gaps/risks (confirmed).** The estimator's power is **stranded**. The live query
path `Node::query → Coordinator::run_query` always calls
`try_local_execution(sql, &cfg, None)` (`coordinator.rs:475`) — i.e. it *never*
computes a `WorkingSetEstimate`. The estimate-aware entry `run_query_planned`
(`coordinator.rs:1284`) exists but has **no production caller** (grep confirms
only the definition + a doc reference). Consequently `auto` routing in production
can only ever hit `PlanReason::NoEstimate → Remote` (`planner.rs:164-168`) or the
forced-local zero-grid shortcut in `node.rs:315`. So a key advertised feature
("small queries run locally, big ones fan out") is **dead in production**; it is
proven only by `planner.rs`/`estimator.rs` unit tests, never end-to-end.
Separately, `QRY-EMPTYCOLS-01` is documented as not live-triggerable
(`REPORT.md` honest-limitations), and the extension exposes no way to read query
*metadata* (§3.6).

**Recommendations.**
- **Quick win** — Wire a *cheap* estimate for the obvious case: when the SQL is a
  single-table scan over a local Parquet/CSV path, call the existing
  `estimator::csv_metadata`/`parquet_metadata_from_resultset` and route via
  `run_query_planned` instead of `run_query`. Files: `node.rs:283` (query path),
  call into `estimator`. Payoff: turns dead code into the headline auto-routing
  feature for the most common workload with zero new mechanism.
- **Medium** — Build the SQL-source analyzer the deferred work calls for: extract
  referenced tables/columns/predicates + `QueryShape` (group_by/join/sort flags)
  from the parsed SQL or an `EXPLAIN` probe on `HostEngine`, feeding
  `estimate_working_set`. Files: new `crates/node/src/sql_analyze.rs`, consumed in
  the query path. Payoff: full estimate-driven `auto` routing live.
- **Larger** — Add the engine-backed Parquet/EXPLAIN probes behind
  `duckdb-engine` so the estimator reads *real* footers, not synthetic metadata
  (`estimator.rs:33-41` flags this scope). Payoff: accurate routing on real data
  lakes.

### 3.3 Highly customizable when needed — ⭐⭐⭐⭐⭐

**Current state.** Excellent and well-tested. Config layers cleanly
(default → file via `P2P_CONFIG` → `P2P_*` env → SQL/runtime `ConfigStore`), and
every knob is reachable three ways: grouped friendly setters
(`p2p_economics`/`p2p_selection`/`p2p_trust`/…, generated by the `group_setter!`
macro `lib.rs:275-396`), a generic `p2p_set('dotted.key', value)` escape hatch,
and per-call `QueryOverrides` (10 named params: replicas, quorum, min_trust,
min_attest, verify, prefer, payment, data_class, require_staked_hosts —
`lib.rs:1528-1542`). Validation is real and atomic: `ADM-SET-03` proves an
invariant-violating quorum is **not persisted** on failure; `VRC-CONFIG-UNKNOWN-FIELD-01`
proves `deny_unknown_fields`; `ADM-RESET-01` restores defaults; settings survive
restart at `0600` (`ADM-PERSIST-01`). Secret redaction is first-class
(`ADM-WALLET-01`, `SET-SECRET-REDACT-01`).

**Gaps/risks.** Customizability is *config-surface* complete but **runtime
introspection** of the effect is thin — there is no way to see, per query,
*which* overrides actually took effect or why (same root as §3.1/§3.6). Also,
several powerful knobs are inert by default and undiscoverable: `capability_weight`,
`exploration_rate`, `require_staked_hosts`, liveness — a user has no signal that
turning them on does anything.

**Recommendations.**
- **Quick win** — Document the inert-by-default knobs in `p2p_config` output with
  an "(active: no — defaults to no-op)" hint sourced from the resolved value.
  File: `crates/config/src/store.rs` `settings()`. Payoff: discoverability of the
  system's advanced levers.
- **Medium** — Echo the *effective* `QueryOverrides` back as a metadata row when
  metadata surfacing lands (§3.6). Payoff: closes the "did my override apply?" gap.

### 3.4 Modular — ⭐⭐⭐⭐⭐

**Current state.** Textbook. Eight crates with crisp seams
(`config`, `proto`, `transport`, `trust`, `node`, `settlement`, `extension`,
`console-server`), and the dependency-inverting trait-per-collaborator pattern is
used consistently: `Discovery`, `TrustStore`, `QueryEngine`,
`LocalOrRemotePlanner` (`planner.rs:105`), `Settlement`/`StakeRegistry`/
`RecordAnchor`/`ParamsSource` (`settlement/src/traits.rs`). Each has a
deterministic `mock`, a genuine `noop`, and a real impl — which is exactly what
made the two-tier harness possible. The `SettlementStack` single-construction-site
(`wiring.rs:39-81`) is a clean example of resolving a whole subsystem from config
with safe defaults.

**Gaps/risks.** Two modularity smells, both minor: (1) the extension cannot reuse
`p2p-node`'s `DuckDbEngine` (feature/linking conflict), so it re-implements a
parallel `HostEngine` (`lib.rs:1232-1326`) that must be kept lockdown-identical by
hand (the comment at `lib.rs:1262-1270` is the only thing keeping them in sync — a
drift risk). (2) The `Coordinator` is a single ~1700-line struct carrying 15+
optional collaborators; it is well-organized but is the natural place a future
refactor would split requester-scheduling from settlement.

**Recommendations.**
- **Quick win** — Extract the DuckDB lockdown PRAGMA set into one shared
  `const`/helper in `p2p-config` (or `p2p-proto`) consumed by both
  `duckdb_engine.rs` and the extension's `HostEngine`. Payoff: eliminates the
  silent-drift risk between the two sandbox lockdowns (a security-relevant
  divergence).
- **Larger** *(optional)* — If the coordinator grows further, split the
  settle/anchor/fine block (`coordinator.rs:1219-1278, 1512-1613`) into a
  `SettlementCoordinator` collaborator. Payoff: testability; not urgent.

### 3.5 Scalable — ⭐⭐⭐⭐

**Current state.** The headline scale result is strong and real: **100 containers**
(3 seed, 92 honest-worker, 2 internal-host, 2 oom-worker, 1 remote-only), all
`NODE_READY` (`node_roles.txt`), full catalog **152/152 PASS**, ~1122 MiB total /
~11.2 MiB/node idle (`docker_stats_idle.txt`, `INDEX.md`). Crucially the
scale-up *fixed* a small-swarm flake: legacy `05_economics_modes` paid tier went
18/20 → **20/20** because 92 honest workers gave ample capacity vs. 8
(`INDEX.md` delta) — strong evidence the dispatch/offer path scales horizontally
on the worker side. The discovery layer samples a bounded candidate set
(`candidate_sample_size=16`, `coordinator.rs:655-658`), so per-query fan-out is
O(sample), not O(grid).

**Gaps/risks.** (1) Scale was proven *breadth-first* (152 distinct scenarios once
each), **not** load/throughput: there is no evidence for sustained QPS, many
concurrent requesters, or large result sets at 100 nodes. (2) The
single-requester `Coordinator` holds all in-flight state in process `HashMap`s and
the extension bridges sync DuckDB ↔ async via a **2-worker-thread** runtime that
`block_on`s each query (`lib.rs:1155-1164, 1468-1470`) — a single DuckDB session
issues queries serially through one node; concurrency across sessions shares those
2 threads *(hypothesis: this is the practical throughput ceiling, untested)*.
(3) `ProgressTracker` and trust state are unbounded-per-job in memory; fine at
100 nodes, unknown at 10k *(hypothesis)*.

**Recommendations.**
- **Medium** — Add a load/throughput scenario to the harness: N concurrent
  requesters × M queries against the 100-node swarm, asserting p50/p95 latency and
  zero correctness loss. Files: new `docker/scenarios/22_load.sh`. Payoff: turns
  the scalability rating from "breadth-proven" to "throughput-proven."
- **Medium** — Make the extension runtime worker-thread count config-driven
  (`lib.rs:1158`) instead of hard-coded 2. Payoff: lets a busy host actually use
  its cores.
- **Larger** — Benchmark coordinator memory/CPU under churn at 1k+ simulated
  candidates (library tier) to find the in-memory-state ceiling before it bites.

### 3.6 Solid — ⭐⭐⭐⭐

**Current state.** The test surface is exceptionally broad and *exact*: 152
live+lib assertions (`scenario_results.txt`), 90 emulator tests, 14/14 live
testnet, and the per-suite `cargo` results are all green (e.g.
`units_resilience.log` 12 passed, `units_settlement.log` 11 passed,
`units_scenarios.log` 18 passed). Assertions check exact values (`500500`), exact
error substrings (`not enough trustworthy workers: have 0, need quorum 2`), and
host-role log lines — not just exit codes. Robustness-to-bad-input is proven
(`ADM-PEERS-02` corrupt config survives `LOAD`; `ADM-SET-03` no partial writes).

**Gaps/risks (confirmed).** "Solid" is bifurcated by the two-tier design. The
production *binary* (extension) is solid for everything it actually does, but
three sizeable modules ship in the binary yet are **never exercised in
production**: the estimator (§3.2), the self-learner profile store (§3.11), and
the live `ton` paid rail (§3.7). They are unit-tested, so they are "correct code,"
but "untested-in-integration" is a real solidity risk the moment they're wired.
The deliberate honesty about this (`REPORT.md` "Honest limitations") is a
credit, but the gap is real.

**Recommendations.**
- **Quick win** — Add a CI gate that fails if `run_query_planned` /
  `CapabilityStore::observe` remain without a non-test caller, so "scaffolded"
  code can't silently rot. Payoff: prevents the gap from widening.
- **Medium** — As each scaffolded seam is wired (§3.2/§3.7/§3.11), promote its
  library-tier proof to a live scenario. Payoff: monotonic increase in
  integration coverage.

### 3.7 Trustable — ⭐⭐⭐⭐

**Current state.** The trust model is sophisticated and the on-chain root of trust
is real. Selection uses a confidence-aware reputation (Beta/Wilson lower bound,
`coordinator.rs:1394-1408`, `TRU-REP-CONFIDENCE-01`) so thin-history nodes aren't
over-trusted; receipts are Ed25519-signed (`make_receipt`, `coordinator.rs:1445`);
replay is deduped (`TRU-REP-REPLAY-01`); the fail-closed `require_staked_hosts`
gate drops unverifiable TOFU peers and yields `NoCandidates` with no registry
(`coordinator.rs:683-697`, `QRY-REQUIRE-STAKED-NOREG-01`). On-chain, the
contracts are audited-grade and **upgradeable in place** (RecordAnchor→V2 and
GlobalParams→V2 via SETCODE at a stable address, proven live; StakeVault→V2
time-gated by design = the staker exit window), with disputes (uphold/reject),
pull-ledger bounce safety, and a candidate-commitment HTLC escrow
(`JobEscrow`, `ton/SCENARIOS.md` §3). Dead ABI codes (134/135/220/221) are
deliberately reserved, never renumbered, to keep the frozen ABI stable.

**Gaps/risks (confirmed).** The live node **wires no stake registry and emits L0
attestation only** (`Attestation::stub_l0()`, `node.rs:269`); `staked-host` /
`l2-host` behaviors are unprovable live and only the *gate* (not bonded
selection) is shown (`REPORT.md` "roles intentionally NOT baked"). So today's
"trust" in production is reputation + signatures, **not** economic stake — the
stake side is contract-complete but not connected to the running grid. The
candidate-commitment escrow seam is explicitly **not threaded** from the
coordinator (`coordinator.rs:1554-1563`), so a live paid settle would be rejected
on-chain. Attestation is a stub, so `min_attestation` above L0 excludes everyone
live (proven indirectly by the gate tests).

**Recommendations.**
- **Medium** — Wire a real `StakeRegistry` impl backed by the on-chain
  `StakeVault.is_eligible`/`stake_of` getters (the `ton` crate already reads
  getters) and inject it via `Node::with_wallet`. Files:
  `crates/settlement/src/ton.rs` (new registry), `node.rs:168`. Payoff: makes
  `require_staked_hosts` and the `stake_factor` selection term *real* in
  production, not just unit-tested.
- **Larger** — Thread the dispatched-workers' payout-wallet candidate set through
  `open_escrow_with_terms` → on-chain `candidatesHash` and present it byte-
  identically at `settle` (`coordinator.rs:1554-1571`, `traits.rs:36-45`). Payoff:
  unblocks the entire live paid path — the single biggest correctness unlock.
- **Larger** — Implement a real attestation provider (≥ L1) to replace
  `stub_l0()` so data-class floors (Internal L1/0.85, Sensitive L2/0.80) are
  enforceable live, not just in `HST-ADMIT-DATACLASS-01` (lib).

### 3.8 Fast — ⭐⭐⭐⭐

**Current state.** The latency-critical paths are designed well. The scheduler
hedges and commits-first, then RESETs losers **immediately, before** the winner's
(possibly large) download so losers stop computing at once
(`coordinator.rs:1009-1032`) — a genuinely good tail-latency design. `verify=fast`
takes the fastest committer (`coordinator.rs:934-943`); results stream in parallel
across uni-streams with lz4/zstd above a min size (`worker.rs:689-697`,
`TRN-PARALLEL-CLAMP-01`, `QRY-COMPRESSION-ONOFF-01`). Local-first execution avoids
the network entirely for small/zero-grid queries. The free local path gives DuckDB
a real `memory_limit` with spill tolerance (`coordinator.rs:1341-1347`).

**Gaps/risks.** (1) **No latency numbers were captured** — the bundle has resource
(MiB) snapshots but no p50/p95 query latency, offer/dispatch timing, or
throughput. "Fast" is therefore an architectural judgment, not a measured one.
(2) The dead estimator (§3.2) means the system can't yet *avoid* shipping a small
query to the grid when local would be faster — it conservatively goes remote on
`NoEstimate`, the slower choice for small data. (3) The extension's 2-thread
`block_on` runtime (§3.5) serializes the async work behind each synchronous SQL
call.

**Recommendations.**
- **Quick win** — Capture timing in the existing harness: have `_common.sh` record
  wall-clock per `QRY-REMOTE-OK-01`/`QRY-LOCAL-01` and emit a `latency_summary.txt`
  alongside `resource_summary.txt`. Payoff: converts the Fast rating from
  asserted to measured with near-zero effort.
- **Medium** — Wiring the cheap estimate (§3.2 quick win) directly improves
  small-query latency by keeping them local. Payoff: latency + cost win on the
  common case.

### 3.9 Unbreakable — ⭐⭐⭐⭐

**Current state.** Resilience is a clear strength, and it's the one adversarial-ish
property proven **live at scale**: `02_chaos.sh` `docker kill`ed 4 bootstrap
workers (honest-worker-{21,32,33,65}, `Exited (137)` in `container_inventory.txt`)
and re-dispatch still reached quorum with 20/20 correct post-chaos
(`INDEX.md`, `all_results.txt` SCENARIO 2 PASS). The resilient loop is
comprehensive: re-dispatch to a fresh set on silence/stall, bounded exponential
backoff + jitter, a global retry token bucket, `max_retries`, and a wall-clock
`max_total_duration` cap (`coordinator.rs:516-632`), all 12 `RES-*` library tests
green (`units_resilience.log`). Defense-in-depth at the host: OS sandbox +
DuckDB lockdown (`enable_external_access=false`, `disabled_filesystems`,
`lock_configuration=true`, `lib.rs:1271-1282`), proven live (`SBX-LOCAL-FILE-01`
never leaks `/etc/passwd`; `SBX-RELOCK-01`). Bounce-safety on-chain via pull-ledger
(`StakeClaim`/`EscrowClaim`).

**Gaps/risks.** (1) The phi-accrual + SWIM **liveness detector is off by default**
(`with_liveness` not wired in `node.rs`/extension; `coordinator.rs:208,370-373`),
so the production grid relies on per-attempt timeouts rather than proactive
exclusion of flapping peers — the `RES-LIVENESS-EXCLUDE-01`/`RES-SWIM-*` proofs are
library-only. (2) The loud "NO OS-LEVEL SANDBOX" warning (`worker.rs:216-225`)
fires when the backend is `noop` + remote access on — meaning in many real
deployments the *only* isolation is the DuckDB lockdown, which "cannot scope
network egress." (3) Chaos was node-kill only; no network partition, slow-loris,
or malformed-frame fuzzing at the live tier.

**Recommendations.**
- **Quick win** — Wire `LivenessView` into the node by default-on for grids with
  ≥ N seeds (it's already built and tested). Files: `node.rs:138-155`,
  `coordinator.with_liveness`. Payoff: proactive flapping-peer exclusion in
  production, not just in unit tests.
- **Medium** — Add a live chaos scenario beyond kill: pause/SIGSTOP a worker
  mid-dispatch (proves stall→re-dispatch live, not just lib) and a malformed-frame
  injection at the transport. Files: `docker/scenarios/02_chaos.sh`. Payoff:
  promotes resilience proofs from lib to live.
- **Larger** — Make a real OS sandbox backend the default where available so the
  warning path is the exception, not the rule.

### 3.10 Anti-cheat — ⭐⭐⭐⭐

**Current state.** The anti-cheat *design* is comprehensive and correct: quorum
verification with minority-cheat detection + reputation penalty
(`QRY-MINORITY-CHEAT-01`, `coordinator.rs:1108-1140`), equivocation/SPLIT handled
loudly with no silent winner (`coordinator.rs:889-908`), canary jobs with known
answers (`TRU-CANARY-01`), nondeterminism detection that marks `random()` queries
unverifiable (`QRY-NONDET-01`, `is_nondeterministic`), consensus-infeasible fault
attribution that refunds and stops without blaming providers
(`coordinator.rs:911-920`, `QRY-INFEASIBLE-01`), worker-side cost gates + free-job
rate limiting (`worker.rs:320-360`, `ABU-COSTGATE-ROWS-01`/`ABU-RATELIMIT-01`),
and a per-node deny-list with auto-block below a trust floor
(`coordinator.rs:751-768`, 8 `ABU-*` live). Broken-commitment fining slashes
staked providers on paid feasible jobs (`fine_failed_commitment`,
`SET-FINE-COMMIT-01`).

**Gaps/risks (confirmed, structural).** **Every adversarial invariant is
library-tier only** — and not by choice but by a real product limitation: the live
`HostEngine`/`Worker` always run *real* SQL with **no fault-injection knob**
(`REPORT.md` two-tier rationale; `worker.rs:442-449` always calls the real
engine). So cheating/wrong-result/slow/equivocating workers cannot be staged in
the live swarm; `ABU-CAND-EXCLUDE-01` is lib-only because `StaticDiscovery`
bootstrap candidates carry no `node_id` (TOFU), so a node-id block can't match at
the requester. Net: the anti-cheat machinery is **proven correct in simulation but
has never run against a live adversary**, and the broken-commitment slash depends
on the un-wired stake registry (§3.7) to have teeth in production.

**Recommendations.**
- **Medium (also unlocks observability, §5)** — Add a **fault-injection config
  surface** to the worker/engine: an opt-in `[fault_injection]` block (off by
  default, refused unless an explicit `allow_fault_injection` flag) that can make a
  host return a wrong hash, stall, or equivocate. Files: `crates/config` (new
  section), `worker.rs`/`HostEngine`. Payoff: lets the live swarm prove
  `QRY-MINORITY-CHEAT-01`, equivocation, and slash-with-real-stake end-to-end —
  the single biggest closure of the lib-only gap.
- **Quick win** — Have `StaticDiscovery` learn and carry `node_id` after the first
  handshake (TOFU pin) so requester-side `node_id` blocks can match live, making
  `ABU-CAND-EXCLUDE-01` provable in the swarm. Files:
  `crates/node/src/discovery.rs`.
- **Larger** — Connect the slash path to a live `StakeRegistry` (§3.7) so cheating
  actually costs money in production.

### 3.11 Self-improver / self-learner — ⭐⭐ (the biggest opportunity)

**Current state.** The *foundation* is laid and partly live. (a) The grid-wide
**counterparty-measured** capability signal IS wired: on every Correct receipt the
requester records the measured result rows/bytes and the trust store folds it into
a proven-capability aggregate (`coordinator.rs:1085-1093, 1199-1204`;
`trust/src/reputation.rs:294` `observe_capability`, deduped, ignores failures,
ratchets maxima — `observe_capability_ratchets_maxima_dedups_and_ignores_failures`
test). (b) A selection term consumes it: `capability_weight * capability_confidence`
(`coordinator.rs:1430-1442`). (c) A durable, **signed, rollback-guarded
`CapabilityProfile` store** exists for a node's *own* self-measured maxima
(`capability_store.rs`, monotonic `seq`, node-id-bound, tamper-resistant,
fully unit-tested). (d) Cold-start exploration bonus exists
(`exploration_bonus`, `coordinator.rs:1420-1424`).

**Gaps/risks (confirmed).** The learning loop is **open at three joints**:
1. `capability_weight` **defaults to 0.0** (`coordinator.rs:1430-1441`), so the
   measured-capability term is a strict no-op until an operator opts in — and
   nothing surfaces that it exists (`VRC-CAPWEIGHT-DEFAULT-NOOP-01`,
   `TRU-CAP-NOINFLATE-01`). The grid does not actually *use* what it measures.
2. The node never feeds its **own** executions into `CapabilityStore`:
   `CapabilityStore::observe` / `MeasuredExecution` have **no production caller**
   (grep: only `capability_store.rs` tests + the `lib.rs:43` re-export). The
   worker computes `row_count` and latency (`worker.rs:666-668`) but discards peak
   memory / temp-dir spill and never calls `observe`. So the self-measured profile
   is never written in production.
3. There is no **gossip** of the signed `CapabilityProfile` (the signing
   primitive `sign_capability_profile` and ad PoW exist — `TRU-CAPAD-POW-01`,
   `TRU-CAPPROF-ROLLBACK-01` — but nothing advertises/consumes profiles between
   nodes), and `exploration_rate` also defaults to 0 (pure exploitation).
Combined with the dead estimator (§3.2), the system has **no closed
sense→model→act loop** in production: it neither learns its own capacity nor
routes by it.

**Recommendations (build on the landed foundation).**
- **Quick win** — Close joint #2 inside the worker: after `ExecOutcome::Ok`, call
  `CapabilityStore::observe(&signer, MeasuredExecution{...})` with the measured
  rows/bytes (and peak memory/temp-dir once the engine reports them). Files:
  `worker.rs:637-668`, using the node identity as `Signer`. Payoff: the node
  starts accumulating a real, signed, monotonic record of the largest workloads it
  has actually completed — the substrate for everything else.
- **Quick win** — Ship a non-zero **default** `exploration_rate` (small, e.g. the
  documented ε) so new honest nodes are actually sampled and can build reputation;
  today cold-start is pure exploitation. Files: `crates/config/src/economics.rs`
  (`ranking.exploration_rate` default). Payoff: the grid self-heals its own
  cold-start bias.
- **Medium** — Close joint #3: advertise the signed `CapabilityProfile` over the
  existing gossip/ad path and consume peers' profiles as an input to
  `capability_confidence`, gated by PoW + signature + epoch nonce (primitives
  already exist). Then raise `capability_weight`'s default off 0 once the signal is
  trustworthy. Files: `crates/trust/src/capability.rs`, `coordinator.rs:1430`.
  Payoff: selection biases toward peers *proven* (not self-claimed) to handle real
  work — the grid gets measurably smarter over time.
- **Larger (depends on §3.2)** — Feed the node's own `CapabilityProfile` maxima
  back into the planner's local-vs-remote estimate as a learned ceiling (e.g.
  "I've handled 8 GiB spill before, so this fits"), closing the sense→model→act
  loop: measure real runs → update profile → route future queries by learned
  capacity. Files: `planner.rs` + the estimator wiring. Payoff: a genuinely
  self-tuning router — the project's marquee differentiator if delivered.

---

## 4. Cross-cutting themes

Each theme is a root cause that blocks several qualities at once.

| # | Theme | Root cause (code) | Qualities it blocks |
|---|---|---|---|
| C1 | **Economics scaffolded but partly unwired** | Live `ton` paid rail resolves to noop without `ton-live`; even with it, the `JobEscrow` candidate-commitment seam isn't threaded (`coordinator.rs:1554-1571`); no live `StakeRegistry` (`node.rs:269` stub L0). | Trustable, Anti-cheat (slash has no teeth), Powerful, Solid |
| C2 | **Estimation path dead in production** | `run_query` always passes `None`; `run_query_planned` has no caller (`coordinator.rs:475,1284`). Rich estimator unexercised end-to-end. | Powerful, Fast, Self-learner |
| C3 | **Extension hides `QueryOutcome` from SQL** | `QueryVTab::bind` keeps only `outcome.result`, discarding verified/agreement/winner/receipts/executed_locally (`extension/src/lib.rs:1471`). | Trustable (no proof to user), Solid (weak introspection), Simple (can't see routing), Customizable (can't confirm overrides) |
| C4 | **No live fault-injection knob → observability gap** | `HostEngine`/`Worker` always run real SQL (`worker.rs:442`); all adversarial/metadata invariants are library-tier only (`REPORT.md` two-tier rationale). | Anti-cheat, Solid, Unbreakable (adversarial cases lib-only) |
| C5 | **Self-learning loop open at three joints** | `capability_weight`=0 default, `CapabilityStore::observe` uncalled in prod, no profile gossip, `exploration_rate`=0. | Self-learner, Fast, Powerful |
| C6 | **Latency never measured** | Harness captures MiB, not ms; no QPS/p95. | Fast (asserted not measured), Scalable (breadth not throughput) |

The striking pattern: **C2, C3, C4, C5 are all the same shape** — a capable
mechanism exists in the binary but the *last wire* connecting it to live behavior
is missing. The project is one "wiring sprint" away from a large step-change in
its weakest qualities.

---

## 5. Prioritized roadmap

Ranked across all qualities by leverage (impact × how many qualities it unblocks)
vs. effort. Dependencies noted.

| Rank | Change | Effort | Impact | Unblocks | Depends on |
|---|---|---|---|---|---|
| **P0-1** | Surface `QueryOutcome` metadata to SQL (new `p2p_query_meta()` or extra columns: verified/agreement/winner/executed_locally/receipt count) | S–M | High | C3 → Trustable, Solid, Simple, Customizable | — |
| **P0-2** | Thread the candidate-commitment set through `open_escrow_with_terms`→`settle` to unblock the live paid path | M–L | High | C1 → Trustable, Powerful; enables real slashing | embedded escrow code (present) |
| **P0-3** | Add an opt-in, default-off `[fault_injection]` worker/engine surface | M | High | C4 → Anti-cheat, Unbreakable, Solid (promote lib proofs live) | — |
| **P1-1** | Wire a real on-chain-backed `StakeRegistry` (read `StakeVault` getters) + inject via `with_wallet` | M | High | C1 → Trustable, Anti-cheat (slash teeth), staked-host selection | P0-2 for full paid loop |
| **P1-2** | Wire a cheap pre-flight estimate for single-table local-file scans via `run_query_planned` | M | High | C2 → Powerful, Fast, Self-learner | — |
| **P1-3** | Feed host self-measurements into `CapabilityStore::observe` after each successful execution | S | Med-High | C5 joint #2 → Self-learner | — |
| **P1-4** | Capture query latency (p50/p95) in the harness | S | Med | C6 → Fast/Scalable measured | — |
| **P2-1** | SQL-source analyzer (tables/cols/predicates/QueryShape) for full estimate-driven `auto` routing | L | High | C2 → Powerful, Fast | P1-2 |
| **P2-2** | Gossip + consume signed `CapabilityProfile`; default `capability_weight`/`exploration_rate` off 0 | M–L | Med-High | C5 joints #1,#3 → Self-learner | P1-3 |
| **P2-3** | Default-on `LivenessView` (phi+SWIM) for multi-seed grids | S–M | Med | Unbreakable (live, not lib) | — |
| **P2-4** | TOFU-pin `node_id` in `StaticDiscovery` so requester-side blocks match live | S | Med | Anti-cheat (ABU-CAND-EXCLUDE live) | — |
| **P2-5** | Concurrent-requester load/throughput scenario at 100 nodes | M | Med | Scalable (throughput-proven) | P1-4 |
| **P3-1** | Engine-backed real Parquet/EXPLAIN probes behind `duckdb-engine` | L | Med | Powerful (accurate routing on real lakes) | P2-1 |
| **P3-2** | Self-tuning planner using learned `CapabilityProfile` ceilings | L | High (differentiator) | Self-learner closed loop | P1-2, P1-3, P2-1 |
| **P3-3** | Real ≥L1 attestation provider replacing `stub_l0()` | L | Med | Trustable (data-class floors live) | — |
| **P3-4** | Shared DuckDB-lockdown const across `duckdb_engine` + `HostEngine` | S | Low-Med | Modular (drift safety) | — |
| **P3-5** | `config-driven` extension runtime thread count | S | Low-Med | Scalable/Fast | — |

### Quick wins shippable now (low effort, no dependencies)

1. **P1-3** — call `CapabilityStore::observe` from the worker on success
   (`worker.rs:637`). One call site; turns a fully-tested dormant module on.
2. **P1-4** — record per-query wall-clock in `docker/scenarios/_common.sh`. Makes
   "Fast" measured.
3. **P0-1 (minimal form)** — add `executed_locally`, `verified`, `agreement`,
   `winner` as columns from the already-returned `QueryOutcome`
   (`extension/src/lib.rs:1471`). Pure read of data already in hand.
4. **§3.1 quick win** — `p2p_explain_routing()` returning the `PlanDecision`.
5. **P2-4** — TOFU-pin `node_id` in `StaticDiscovery`.
6. **P3-4** — extract the lockdown PRAGMA set into one shared const.
7. **§3.6 quick win** — CI gate that fails if scaffolded entry points
   (`run_query_planned`, `CapabilityStore::observe`) lack a non-test caller.

---

## 6. Observability / testing gaps and how to close them

What the 100-node and on-chain runs **could not assert live**, and the concrete
surface that would close each:

| Gap (what's unprovable live today) | Why | How to close it |
|---|---|---|
| Query metadata (verified/agreement/winner/receipts/executed_locally) | Extension returns only result rows (`lib.rs:1471`) | **P0-1**: expose a `p2p_query_meta()` / extra columns. Then `QRY-REMOTE-OK-01` can assert `verified=true, agreement>=2` *live*, not just in lib. |
| Cheating / wrong-result / slow / equivocating workers | No fault-injection knob; `HostEngine` always runs real SQL (`worker.rs:442`) | **P0-3**: opt-in `[fault_injection]`. Promotes `QRY-MINORITY-CHEAT-01`, equivocation, `ABU-FAULTATTR-*` from lib to live. |
| Staked-host / L2-host selection, real slashing | No live `StakeRegistry`; `stub_l0()` attestation (`node.rs:269`) | **P1-1** + **P3-3**: on-chain stake registry + real attestation. Makes `require_staked_hosts` select (not just gate) and gives `SET-FINE-COMMIT-01` teeth live. |
| Requester-side node-id blocking | TOFU candidates carry no `node_id` | **P2-4**: TOFU-pin. Makes `ABU-CAND-EXCLUDE-01` a live scenario. |
| Live paid settlement (escrow open→settle) | Candidate-commitment seam unthreaded (`coordinator.rs:1554-1571`); negatives are emulator-by-design | **P0-2** threads the seam; then a *positive* paid settle can broadcast (the negatives stay correctly emulator-only). |
| Query latency / throughput / concurrent load | Harness captures MiB, not ms; one-shot per scenario | **P1-4** + **P2-5**: latency capture + concurrent-requester scenario. |
| Full-speed on-chain CI | Keyless Toncenter self-throttles to ~1 RPS; faucet 2 airdrops/24h/IP (`ton/SCENARIOS.md` §7) | Provision a `TON_TESTNET_API_KEY` (Toncenter testnet key) in CI secrets, exported as `TONCENTER_TESTNET_API_KEY`, so the comprehensive runner isn't rate-limited; pre-fund a long-lived test wallet to dodge the faucet cap. |
| StakeVault→V2 live upgrade | Timelock == unbonding window (≥7 days) **by design** (the staker exit window) | Keep emulator-only (`G`); do **not** degrade the delay live (that would defeat the property). Document as an intentional non-gap. |

**Guiding principle for closing these:** the two-tier harness is the *right*
design given the current binary; the goal is not to delete the library tier but to
**migrate proofs up to the live tier as each seam is wired** (P0-2/P0-3/P1-1),
shrinking the set of "simulation-only" guarantees over time.

---

## 7. Bottom line

duckdb-p2p is a high-quality, mostly-complete system whose few weak spots share a
single signature: **a capable mechanism is built and unit-tested but the final
wire to live behavior is missing.** Nothing on the critical path requires
inventing new mechanisms — it requires connecting existing ones:

- **Make it provable** (P0-1 metadata, P0-3 fault injection) — so the system's
  real strengths (verification, anti-cheat) are demonstrable live, not just in
  simulation.
- **Make economics real** (P0-2 escrow seam, P1-1 stake registry) — so trust and
  slashing have teeth in production.
- **Make it learn** (P1-2 estimate wiring, P1-3 self-measurement, P2-1/P2-2/P3-2)
  — closing the sense→model→act loop the foundation already supports.

Do the seven quick wins first (a few days of work, no dependencies), then the four
P0/P1 wiring tasks. That sequence converts the three weakest qualities
(self-learner ⭐⭐, economics-dependent facets of trustable/anti-cheat, and
observability) into demonstrated strengths without touching the architecture that
already earned 152/152 at 100 nodes.
