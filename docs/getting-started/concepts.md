# Core concepts

A short mental model. For the full treatment see the
[Architecture deep dive](../ARCHITECTURE.md).

## Roles

- **Requester** — the node that wants a query answered. It runs the
  **coordinator** logic: discover candidates, dispatch, collect results, verify.
- **Host / worker** — a node that donated compute via `p2p_share` and executes
  queries for others under an admission/budget policy.
- A node can be **both** (run its own queries *and* serve others), or a **pure
  requester** (e.g. a thin/mobile client that never executes locally).

## The protocol flow

A query travels through a small, uniform message exchange over QUIC:

```text
Requester --Offer-->     Worker     (job id + query hash + cost hints + scoping)
Worker    --Bid-->       Requester  (accept w/ ETA + attestation + receipts, or reject)
Requester --Dispatch-->  Worker     (full SQL + scoped credential — to the selected top-k)
Worker    --Commit-->    Requester  (result hash FIRST — "commit-first")
Worker    --Chunk*-->    Requester  (the bulk result stream; winner only)
Requester --Cancel-->    Worker     (RESET the losers)
```

The **Offer** is a lightweight probe — it carries the query *hash* and cost
hints, **not** the SQL text or data. Only the selected worker(s) receive the full
**Dispatch**.

## Hedged execution

The coordinator **races `k` workers** (`replicas`), accepts the first result that
reaches **quorum**, and `RESET`s the losers. A slow or stalled host costs you
latency, not a wrong answer; the resilient loop re-dispatches around silent or
failing hosts to a fresh candidate set.

## Verification

- **Canonical hashing** — every result is reduced to an order-independent BLAKE3
  hash, so honest workers on the same `(sql, engine_version)` agree byte-for-byte.
- **Quorum** — `quorum` matching hashes must agree before a result is accepted.
- **Verify modes** — `quorum` (wait for agreement) or `fast` (return the first
  result, verify in the background).
- **Canary audits** — randomized re-checks deter a host that is "honest most of
  the time."

## Trust & selection

Host selection blends, per the configured weights:

- **Reputation** — a confidence-aware (Wilson-shrunk) score from **signed,
  gossiped receipts**.
- **Attestation tier** — `L0` (anonymous), `L1` (TPM measured-boot), `L2`
  (hardware TEE). A bid claiming `> L0` is honored **only** if its evidence
  verifies (fail-closed).
- **Stake** (for paid jobs) and measured **performance** (latency/throughput from
  receipts, not self-reported ETA).

Each node also keeps its **own** deny-list (`p2p_block`/`p2p_unblock`) and decides
independently whom to serve — there is no central authority in the data path.

## Local-first planner

By default a node is **local-first**: the planner runs small/cheap queries for
free in its own locked-down in-process DuckDB and fans out to the grid only when
a query is too big, the node is saturated, or the caller asks (`prefer`). With
no reachable grid, an `auto` query falls back to local rather than failing. You
can force **remote-only** (`p2p_planner(local_execution => false)`) for thin
clients.

## Free vs paid

- **Free (default)** — fully off-chain. No wallet, no escrow, no fees. Jobs are
  still scored (quorum/canary + receipts + reputation).
- **Paid (opt-in)** — settles through a per-job escrow on **TON**: the escrow
  pays the winner its quoted base, a platform fee to the treasury, and a
  commission to each agreeing verifier, refunding the remainder to the requester.
  See [Paid queries](../guides/paid-queries.md) and the
  [economics deep dive](../HOW_IT_WORKS.md).

## Discovery

Discovery returns a **bounded candidate sample** (not the whole swarm), keeping a
requester's fan-out sub-linear as the network grows to thousands of hosts.
Backends are pluggable (static seeds, a membership table, libp2p gossip).

## Configuration layers

Everything is configurable, with a clean precedence chain (lowest → highest):

```text
built-in defaults  <  config file (P2P_CONFIG)  <  P2P_* env  <  SQL/runtime  <  per-call
```

See [Configuration](../reference/configuration.md).
