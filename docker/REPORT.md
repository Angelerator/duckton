# Multi-node Docker harness — duckdb-p2p grid scenario catalog

A real, heterogeneous Docker swarm of `duckdb_p2p` nodes (DuckDB CLI + the loadable
extension) that exercises the **off-chain** scenario catalog in `docker/SCENARIOS.md`.
Every test drives the real `duckdb` CLI and asserts **exact** rows / values / error
substrings / log lines. **No git commits were made; no TON gas was spent.**

## TL;DR

| Item | Result |
|---|---|
| Image | `p2p-node:latest` **rebuilt** from current source (per-job `data_class`, `require_staked_hosts`, measured-capability knobs present) |
| Topology | **16 containers** + 1 client + 1 solo: 3 `seed`, 8 `honest-worker`, 2 `internal-host`, 2 `oom-worker`, 1 `remote-only-node` |
| Per-id catalog result | **152 / 152 PASS, 0 FAIL** (`docker/run_all_scenarios.sh`) |
| Live swarm (over QUIC) | Admin/Config 46, Query-local 6, Hosting 11, Sandbox 5, Settlement-prepared 10, Query-remote 13, Anti-abuse 8 |
| Library tier (real loopback QUIC + MockEngine + in-mem rails) | scenarios 14, antiabuse 7, sandbox 5, transport 3, zero_config+trust 7, resilience 12, settlement 5 |
| Legacy live smoke (back-compat) | `01` 31/31 ✅, `02` 20/20 ✅ (re-dispatch around 4 killed), `03` ✅, `05` free/gate ✅ paid 18/20 (concurrency) |

## Two-tier design (why)

The extension's `p2p_query` returns only the **result rows** — the rich
`QueryOutcome` metadata (`executed_locally`, `verified`, `agreement`, `winner`,
`receipts`) is **not** surfaced to SQL, and the live host engine (`HostEngine`)
always runs **real** SQL with **no fault-injection knob**. So:

* **Live swarm / single container** asserts everything observable end-to-end:
  result values streamed back over QUIC, exact error strings
  (`NoCandidates` / `InsufficientWorkers have:0` / quorum invariant /
  `WalletRequired`), the full SQL admin surface, the in-DuckDB sandbox lockdown,
  prepared settlement gates, and the deny-list surface — plus host-role logs
  (`NODE_ROLE`, `share|node_id`, `listen_addr`) via `docker logs`.
* **Library tier** (`cargo test`, real loopback QUIC + `MockEngine` + in-memory
  stake registry, **no chain**) proves the adversarial / internal invariants the
  live extension cannot inject: cheating/quorum-agreement/canary, hedged-race,
  worker deadline/stall/abandon, phi/SWIM exclusion, re-dispatch, retry-budget,
  infeasible consensus, the broken-commitment fine, escrow split + GlobalParams
  overlay/version-binding, version negotiation, OS sandbox (rlimit/seatbelt),
  cost-gate/rate-limit/blocklist-exclusion/requester-trust, and the
  local-first metadata invariants.

## Heterogeneous topology (`gen_compose.py`)

Role-based, self-documenting service names; each role drives a real behavior via
env/config the node already supports:

| Service | Behavior (env/config) |
|---|---|
| `seed-1..3` | bootstrap mesh, serve `public`, bootstrap each other |
| `honest-worker-1..8` | serve `public` — the REMOTE-OK workhorses (a public-only host = a "free-only host") |
| `internal-host-1..2` | `P2P_SHARE_DATA_CLASSES=internal,sensitive` → **refuse** a `public` offer (data-class routing) |
| `oom-worker-1..2` | tiny donated budget (`P2P_SHARE_MEMORY=32MB` < the 64 MiB per-job lease) → admission rejects "at capacity" |
| `remote-only-node-1` | `P2P_PLANNER_LOCAL_EXEC=false` → never executes locally |

Each container keeps the existing hardening (read-only root, `no-new-privileges`,
all caps dropped) and the `/node/state` tmpfs **uid/gid=1001** fix.

### Roles intentionally NOT baked (no supporting knob — noted, not faked)

The live node/extension has **no env** for these, so they are proven at the
library tier instead of being faked in the swarm:

* **cheating / wrong-result / slow / equivocating workers** — `HostEngine` always
  runs real SQL; result-divergence is `MockEngine`-only → `30_units_scenarios.sh`,
  `31_units_antiabuse.sh`.
* **`staked-host` / `l2-host`** — the live node wires no stake registry and emits
  L0 attestation (bonded stake / measured attestation are not env-settable). The
  fail-closed `require_staked_hosts` path IS shown live (`QRY-REQUIRE-STAKED-NOREG-01`).
* **`blocked-actor`** — blocking is a runtime `p2p_block` deny-list entry, not a
  baked image; the management surface is live (`21`), candidate-exclusion is
  library-tier (`ABU-CAND-EXCLUDE-01`) because `StaticDiscovery` bootstrap
  candidates carry no `node_id` (TOFU), so a node-id block cannot match one at the
  requester.

## Scenario groups → scripts

| Catalog surface | Script | Tier | Count |
|---|---|---|---|
| A. Admin/Config | `scenarios/10_admin_config.sh` | solo | 46 |
| B. Query/Dispatch (local) | `scenarios/11_query_local.sh` | solo | 6 |
| C. Hosting/Swarm (share/join) | `scenarios/12_hosting.sh` | solo | 11 |
| J. Sandbox/Security (in-DuckDB) | `scenarios/13_sandbox.sh` | solo | 5 |
| E. Settlement (prepared/gates) | `scenarios/14_settlement_prepared.sh` | solo | 10 |
| B. Query/Dispatch (remote) | `scenarios/20_query_remote.sh` | swarm | 13 |
| H. Anti-abuse (deny-list) | `scenarios/21_antiabuse_live.sh` | solo | 8 |
| B/D/F/G. scenarios | `scenarios/30_units_scenarios.sh` | cargo | 14 |
| H. anti-abuse selection | `scenarios/31_units_antiabuse.sh` | cargo | 7 |
| J. OS sandbox | `scenarios/32_units_sandbox.sh` | cargo | 5 |
| F. transport versioning | `scenarios/33_units_transport.sh` | cargo | 3 |
| B/E. zero_config + trust | `scenarios/34_units_trust.sh` | cargo | 7 |
| G/E. resilience + fine | `scenarios/35_units_resilience.sh` | cargo | 12 |
| E. paid settlement | `scenarios/36_units_settlement.sh` | cargo | 5 |

Representative live evidence (exact, captured during the run):

* `ADM-INFO-01` → `protocol_name|duckdb-p2p`, `alpn|duckdb-p2p/1`, 6 rows.
* `ADM-NET-01` → `… switching to MAINNET puts REAL TON at stake …`.
* `QRY-REMOTE-OK-01` → `500500` streamed from honest workers over QUIC.
* `QRY-MINTRUST-EXCLUDES-ALL-01` → `not enough trustworthy workers: have 0, need quorum 2`.
* `QRY-REQUIRE-STAKED-NOREG-01` → `no hosts available to run this query on the grid …`.
* `QRY-DATA-CLASS-ROUTE-MISMATCH-01` → public job to `internal-host`s → `have 0`.
* `HST-ADMIT-MEM-01` → public job to `oom-worker`s → `have 0` (admission reject).
* `SBX-LOCAL-FILE-01` → `File system LocalFileSystem has been disabled by configuration`; `/etc/passwd` never leaks.
* `SBX-RELOCK-01` → `… the configuration has been locked`.
* `SET-STAKE-01` → `stake|status|prepared — submit on-chain via the configured wallet + RPC …`.

## How to reproduce

```bash
docker build -f docker/Dockerfile -t p2p-node:latest .
docker/run_all_scenarios.sh                 # build-if-needed, up, run all, tally, teardown
#   KEEP_UP=1   leave the swarm running       NO_UNITS=1  skip the cargo library tier
#   BUILD=1     force a rebuild
```

Single groups (swarm must be up for `20/21` and the legacy live smoke):

```bash
bash docker/run_swarm.sh && bash docker/scenarios/00_wait_ready.sh
bash docker/scenarios/10_admin_config.sh      # solo groups need no swarm
bash docker/scenarios/20_query_remote.sh
bash docker/scenarios/33_units_transport.sh   # cargo (sets SDKROOT on macOS)
bash docker/stop_swarm.sh
```

## Harness bugs found & fixed (vs product behavior)

* **Harness:** `group` is a reserved word in DuckDB — `SELECT group||…` parse
  error; fixed by quoting `"group"` (escaped for bash) in the admin assertions.
* **Harness:** the `_common.sh` `req_query` swallowed stderr, hiding the error
  strings; added `req_query_all` (combined stdout+stderr) for the error-path
  assertions.
* **Harness (carried over):** `/node/state` tmpfs needs `uid=1001,gid=1001` —
  preserved in the new `gen_compose.py`.

## Honest limitations (product, not harness)

* `QRY-EMPTYCOLS-01` (synthesized `result` column) is **not live-triggerable**:
  the host DuckDB (duckdb-rs) always reports ≥1 column, so the column-synthesis
  branch fires only for an engine returning zero columns. Documented in `11`.
* `SET-STAKE-MAINNET-01`: the mainnet guard is enforced at config-set time (the
  network switch itself is blocked without `confirm`), so an unconfirmed-mainnet
  stake state is unreachable via SQL; the guard is unit-proven in config tests.
* Legacy `05_economics_modes.sh` under 20-way concurrency on the smaller 8-honest
  swarm occasionally reports paid 18/20 (a couple of paid offers time out under
  load) — the free/gate divergence is always green, and the deterministic paid
  guarantees (escrow split, overlay, version-binding, the fine) are green in
  `36_units_settlement.sh`.
* Paid self-broadcast stays `prepared` unless built `--features ton-live`
  (by design; no gas spent).

## Host / environment

* Docker 29.4.2 (Desktop), macOS, ~8 GiB / 6 CPU VM, `linux/arm64`.
* 16 swarm containers + a 1.5 GiB client + a 1 GiB solo fit comfortably
  (idle RSS ≈ 10 MiB/node). No scale-down required.
