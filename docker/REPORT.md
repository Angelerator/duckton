# 100-Node Docker Swarm Test — P2P DuckDB Grid

End-to-end validation of the P2P DuckDB compute grid at scale: a real Docker
swarm of **100 nodes**, each a DuckDB CLI with the `duckdb_p2p` extension loaded,
running distributed queries over QUIC, chaos/resilience under real node deaths,
and a bounded TON economics check. **No git commits were made.**

## TL;DR

| Item | Result |
|---|---|
| In-container extension build | ✅ builds `crates/extension` cdylib (linux_arm64), appends DuckDB metadata, loads in CLI |
| Node count | **100 / 100** launched & hosting (3 seeds + 97 workers) |
| Host resource usage | **~1.03 GiB total, ~10.6 MiB/node idle** on a 7.7 GiB Docker VM |
| Cross-node queries (over QUIC) | **31/31** correct (1 sanity + 30 concurrent), result matches locally-computed expected |
| Chaos / re-dispatch | **20/20** correct after `docker kill` of 4/8 bootstrap workers; grid routes around dead nodes, quorum still reached |
| Resilience guarantees (phi/SWIM + FailedCommitment fine) | **12/12** `resilience.rs` tests pass (in-memory rails) |
| Scale / bounded fan-out | ✅ 40-node bootstrap query completes via the capped candidate sample (no global broadcast) |
| TON | **68/68** Acton emulator tests pass; `fork_testnet` reads **live deployed** testnet vault state. Swarm uses the **mock/in-memory** rail; **no per-node testnet gas spent** |

## How a node is packaged

A "node" = the **DuckDB CLI** + the loadable **`duckdb_p2p`** extension. On start the
entrypoint:

1. `LOAD '/node/duckdb_p2p.duckdb_extension'`
2. `CALL p2p_set('budget.per_job_memory_bytes', …)` — shrink the per-job lease so it
   is admissible under the lean donated budget.
3. `CALL p2p_share(memory=>…, threads=>1, max_jobs=>4, data_classes=>['public'])` —
   becomes a **host/worker**: binds a QUIC endpoint (`0.0.0.0:9494`) and spawns the
   worker accept loop.
4. Holds the DuckDB process open (stdin fed from a FIFO whose write end never
   closes) so the worker keeps serving the swarm.

Bootstrap seeds are supplied via the **`P2P_BOOTSTRAP`** env (read by the
extension's config layer) rather than a second `p2p_join` CALL — because every
`p2p_share`/`p2p_join` rebuilds and **re-binds** the fixed port, so issuing both on
one node collides ("address already in use"). One `p2p_share` + env bootstrap =
one bind.

Requesters are short-lived `duckdb` processes (`p2p_set` per-job mem → `p2p_join`
→ `FROM p2p_query(…, prefer=>'remote')`) run from a dedicated client container so
they don't compete with a hosting node's tight `mem_limit`.

### The image (`docker/Dockerfile`, multi-stage)

* **builder** (`rust:1-bookworm`): installs clang + libc++ + python3, downloads the
  DuckDB **v1.5.3** CLI (matches the `duckdb` crate `1.10503.1` → DuckDB 1.5.3),
  runs `cargo build -p p2p-extension` (default features — **no libp2p**, static
  seeds), then appends the DuckDB metadata footer via
  `scripts/append_extension_metadata.py` and smoke-tests `LOAD … p2p_info()`.
* **runtime** (`debian:bookworm-slim`): just the DuckDB CLI + the built
  `.duckdb_extension` + entrypoint + `libstdc++6`/`libgomp1`. **No Rust toolchain
  in the final image.** Image size ≈ **397 MB**.

The in-container extension build was **not** a blocker — it compiles the whole
workspace (proto/config/transport/trust/settlement/node/extension) in ~56 s and
produces a `linux_arm64` loadable extension that the CLI loads cleanly.

## Topology

`docker/gen_compose.py` generates an explicit per-node compose file (stable DNS
names so requesters can target many distinct nodes — `resolve_seeds` only takes
the first A record per host, so distinct hostnames are required for real fan-out):

* `seed1..seed3` — bootstrap mesh (bootstrap each other).
* `node1..node97` — workers, `P2P_BOOTSTRAP` = the seeds.

Each container: `mem_limit 224m`, `cpus 0.4`, `pids_limit 64`, `max_jobs 4`,
donated budget `512MB` (admission accounting only — actual idle RSS ≈ 10 MiB).

## Scenario results

### 1. Cross-node queries (`docker/scenarios/01_cross_node_query.sh`)
A client runs `FROM p2p_query('SELECT sum(i) AS s FROM range(1,1001)…',
prefer=>'remote', replicas=>3, quorum=>2)` routed over QUIC to **other** nodes;
the worker executes on its **host DuckDB** and streams the verified result back.
Expected `500500` (computed independently). **31/31 correct** (1 sanity + 30
concurrent). Queries are dispatched, quorum-verified, and streamed — not faked.

### 2. Chaos / resilience (`docker/scenarios/02_chaos.sh`)
Picks 8 bootstrap workers, `docker kill`s 4 of them, then runs 20 remote queries
against the **same** 8-node bootstrap (4 dead). **20/20 correct** — the
coordinator routes around dead candidates (failed offers are dropped) and still
reaches `quorum=2` from the survivors.

The deeper resilience guarantees are proven **deterministically** by
`cargo test -p p2p-node --test resilience` (`docker/scenarios/04_resilience_units.sh`),
**12/12 pass** over real loopback QUIC + mock engine + **in-memory** stake
registry (NO network, NO live TON), including:
* `phi_convicted_node_is_excluded_from_selection` — phi-accrual/SWIM exclusion.
* `host_job_timeout_abandons_and_redispatches`, `all_silent_redispatches_to_a_fresh_set`,
  `progress_stall_detected_redispatches` — resilient re-dispatch.
* **`paid_broken_commitment_is_fined`** — a node that accepts a paid job then fails
  to deliver is fined `FailedCommitment` (= 10% of bonded stake) against the
  in-memory rail; the deliverer is paid, not fined.

> Note: the extension's live `Node` wires plain `StaticDiscovery` + free
> settlement, so phi/SWIM exclusion and the paid fine are exercised at the
> **library** layer (the deterministic suite) rather than over the live swarm,
> which demonstrates re-dispatch routing under real container deaths.

### 3. Scale / health (`docker/scenarios/03_health.sh`)
A requester given a **40-node** bootstrap still completes via the bounded
candidate sample (`candidate_sample_size`, default 16) — **no global-broadcast
blowup**, sub-linear fan-out. Swarm stable; resource snapshot ~1 GiB / 100 nodes.

## TON (bounded, gas-aware)

* The **swarm runs free jobs on the mock / in-memory settlement rail** — each node
  reports `economics_enabled=false`, `settlement=noop`. **No paid jobs and no
  testnet gas were spent across the 100 nodes.**
* The **paid economics path** (escrow/stake/slash/anchor) is validated by the
  **Acton emulator suite**: `acton test` → **68 passed in 8 files** (stake 22,
  escrow 11, global_params 12, anchor 9, receipt_wallet 6, fuzz 6, e2e_flow 1,
  fork_testnet 1), plus the in-memory `FailedCommitment` fine test above.
* **Live testnet:** four economic contracts are deployed on testnet
  (`ton/deployments/economics.testnet.toml`: stake_vault, job_escrow,
  record_anchor, global_params) and `tests/fork_testnet.test.tolk` **reads the
  live deployed vault state** (an on-chain read — no gas). The
  `e2e-testnet` / `testnet_live` harnesses exist for live broadcasts but were
  **not** run here (no Toncenter API key configured in this environment, and to
  avoid spending gas per node, exactly as instructed).

## Infra files added (all under `docker/`)

| File | Purpose |
|---|---|
| `docker/Dockerfile` | Multi-stage image: builds the extension cdylib + metadata, lean runtime |
| `docker/entrypoint.sh` | Node startup: load ext, set per-job mem, `p2p_share`, keep-alive FIFO |
| `docker/gen_compose.py` | Generate an N-node compose (explicit seeds + workers) |
| `docker/run_swarm.sh` | Generate + `compose up -d` the swarm |
| `docker/stop_swarm.sh` | Tear down swarm + client |
| `docker/scenarios/_common.sh` | Shared helpers (bootstrap lists, requester exec, client) |
| `docker/scenarios/00_wait_ready.sh` | Wait for N nodes to report `NODE_READY` |
| `docker/scenarios/01_cross_node_query.sh` | Cross-node distributed query + assertions |
| `docker/scenarios/02_chaos.sh` | Kill nodes mid-flight; assert re-dispatch/quorum |
| `docker/scenarios/03_health.sh` | Bounded fan-out + resource snapshot |
| `docker/scenarios/04_resilience_units.sh` | Deterministic phi/SWIM + FailedCommitment fine |
| `.dockerignore` | Keep the build context lean |
| `docker/compose.generated.yml` | Generated artifact (regenerated by `run_swarm.sh`) |

## How to reproduce

```bash
docker build -f docker/Dockerfile -t p2p-node:latest .
./docker/run_swarm.sh 100 3 224m 0.4
./docker/scenarios/00_wait_ready.sh 100 100
bash docker/scenarios/01_cross_node_query.sh 30 3 2
bash docker/scenarios/02_chaos.sh 8 4 20
bash docker/scenarios/04_resilience_units.sh
bash docker/scenarios/03_health.sh
./docker/stop_swarm.sh
```

## Host / environment

* Docker 29.3.1 (Desktop 4.66.1), VM 7.7 GiB / 10 CPUs, `linux/arm64`.
* 100 containers fit comfortably (~1 GiB actual). 100 was the target and was met
  — no scale-down required.

## Blockers / honest notes

* **Initial QUIC "have 0" debugging.** Cross-node queries first failed with "not
  enough trustworthy workers". Root causes (all fixed in the infra):
  1. The image baked `P2P_SHARE_MEMORY` / `gen_compose` set a donated budget
     **smaller** than the default 1 GiB per-job lease → workers rejected every
     offer "at capacity". Fixed by shrinking the per-job lease (`p2p_set`) on both
     worker and requester and sizing the budget above it.
  2. Under concurrency, calling `docker compose ps` **per query** intermittently
     returned empty → empty bootstrap → `p2p_join` error. Fixed by caching the
     worker list once.
  3. Requesters exec'd inside tight-`mem_limit` hosting nodes got starved; moved
     to a dedicated client container.
* **`p2p_query` per-call knobs:** only `replicas/quorum/min_trust/min_attest/
  verify/prefer/payment` are exposed by the extension; the resilience timeout
  knobs (`attempt_deadline_ms`, `max_retries`, …) are config/env-only.
* Background `docker pull` of the `rust` base image kept dying across tool calls;
  pulled it in the foreground once, after which builds were fast.
* No source code was committed; two temporary debug edits to
  `crates/node/src/{coordinator,worker}.rs` were **reverted** (verified clean).
