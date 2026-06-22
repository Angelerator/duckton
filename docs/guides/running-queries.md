# Running queries

`p2p_query(sql, [overrides...])` runs SQL on the grid (or locally on the free
path). Every override is **optional** and applies to **that call only** — the
highest-precedence config layer. `p2p_query_meta(sql, ...)` returns the same
query's execution/verification metadata instead of the rows.

## Per-call overrides

| Parameter | Type | Meaning |
|---|---|---|
| `replicas` | int | How many workers to race (default `3`). |
| `quorum` | int | Matching result hashes required to accept (default `2`, ≤ `replicas`). |
| `verify` | `'quorum'` \| `'fast'` | Wait for quorum, or return the first result and verify in the background. |
| `prefer` | `'local'` \| `'remote'` \| `'auto'` | Where to run it (default `auto` = local-first). |
| `payment` | `'free'` \| `'paid'` \| `'auto'` | Off-chain vs. settled on TON (default per config; see [Paid queries](paid-queries.md)). |
| `min_trust` | float | Minimum effective trust score for an eligible host. |
| `min_attest` / `min_attestation` | `'L0'`/`'L1'`/`'L2'` | Minimum **verified** attestation tier. |
| `require_staked_hosts` | bool | Restrict selection to bonded/staked hosts. |
| `data_class` | `'public'`/`'internal'`/`'sensitive'` | Sensitivity class; raises the attestation/trust selection floors. |
| `network` | string | Target a logical grid partition. |
| `groups` | list | The requester's group claims (a grouped host serves only on a shared group). |
| `regions` | list | Accept only hosts in one of these regions. |
| `nodes` | list | Pin the job to **exact node id(s)** — see [Node targeting](node-targeting.md). |
| `result_parallelism` | int | Concurrent result-transfer streams for this call. |
| `compression` | `'none'`/`'lz4'`/`'zstd'` | Wire compression for the result. |
| `dispatch_timeout_ms`, `attempt_deadline_ms`, `max_retries`, `max_total_duration_ms` | int | Resilience / re-dispatch tuning. |

## Examples

```sql
-- A redundant, quorum-verified analytic query over object storage:
SELECT * FROM p2p_query(
  'SELECT region, count(*) FROM ''s3://bucket/events/*.parquet'' GROUP BY region',
  replicas => 5, quorum => 3, verify => 'quorum'
);

-- Latency-first: take the fastest result, verify in the background:
SELECT * FROM p2p_query('SELECT ...', verify => 'fast');

-- Force the grid (never run locally), only highly-trusted L1+ hosts:
SELECT * FROM p2p_query('SELECT ...', prefer => 'remote', min_trust => 0.8, min_attest => 'L1');

-- Run it entirely on this machine, for free:
SELECT * FROM p2p_query('SELECT ...', prefer => 'local');
```

## Inspecting what happened

```sql
SELECT key, value FROM p2p_query_meta('SELECT * FROM range(3)');
```

`p2p_query_meta` surfaces `executed_locally`, `verified`, `agreement` vs.
`quorum`, the winning host, the agreed hash, row count, and
participant/receipt counts — so verification is observable, not a black box.

## Remote-only (thin clients)

A low-power or mobile client can route **100% of execution to remote hosts** and
never compile/run anything locally:

```sql
CALL p2p_planner(local_execution => false);   -- hard gate (survives restart)
CALL p2p_planner(prefer => 'remote');          -- sticky default
```

When `local_execution = false` the node **never** runs a query locally — not even
a per-call `prefer => 'local'` (the gate overrides preference). With no reachable
hosts you get a clear `NoCandidates` error rather than a silent local fallback;
`p2p_status()` then shows `execution_mode = remote-only`.

## Friendly failures

Errors are actionable, never a stack trace. For example, `payment => 'paid'`
with no wallet configured returns a message telling you to pass `payment =>
'free'` or configure a wallet. See [Observability &
troubleshooting](../operations/observability.md).
