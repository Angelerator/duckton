# Quickstart

Get from zero to a verified query in under a minute.

## 1. Install the extension

```sql
INSTALL duckton FROM community;
LOAD duckton;
```

(Building from source instead? See [Installation](installation.md).)

## 2. Confirm it loaded

```sql
SELECT * FROM p2p_info();
```

```text
┌───────────────────────┬──────────────┐
│          key          │    value     │
├───────────────────────┼──────────────┤
│ protocol_name         │ duckdb-p2p   │
│ protocol_version      │ 1.0.0        │
│ min_supported_version │ 1.0.0        │
│ schema_version        │ 1            │
│ extension_version     │ 0.6.2        │
│ alpn                  │ duckdb-p2p/1 │
└───────────────────────┴──────────────┘
```

## 3. Run a query

With no peers configured, this executes on the **free, in-process, locked-down
DuckDB** and streams verified rows straight back through SQL:

```sql
SELECT * FROM p2p_query('SELECT 42 AS answer');
```

Inspect *what actually happened* (did it run locally? was it verified?):

```sql
SELECT key, value FROM p2p_query_meta('SELECT * FROM range(3)')
WHERE key IN ('executed_locally', 'verified', 'result_rows');
```

```text
┌──────────────────┬─────────┐
│ executed_locally │ true    │
│ verified         │ true    │
│ result_rows      │ 3       │
└──────────────────┴─────────┘
```

## 4. Customize per call (optional)

Every routing decision is a one-liner override on `p2p_query` — you only touch
these when you *want* to:

```sql
SELECT * FROM p2p_query(
  'SELECT region, count(*) FROM ''s3://bucket/events/*.parquet'' GROUP BY region',
  replicas   => 3,         -- how many workers to race
  quorum     => 2,         -- matching hashes required to accept
  verify     => 'quorum',  -- 'quorum' | 'fast'
  prefer     => 'auto',    -- 'local' | 'remote' | 'auto'
  min_trust  => 0.8,
  min_attest => 'L1'
);
```

## 5. Join a swarm / become a host (optional)

```sql
-- Join a specific network by seed (otherwise local-first):
CALL p2p_join(bootstrap => ['quic://seed1.example:9494', 'quic://seed2.example:9494']);

-- Donate this machine's compute and start serving others:
CALL p2p_share(memory => '4GB', threads => 2, max_jobs => 3, data_classes => ['public']);

-- See who's out there and how trusted they are:
SELECT node_id, free_mem, attestation_level, trust_score FROM p2p_peers();
```

## Next steps

- [Core concepts](concepts.md) — understand coordinator/worker, hedging, quorum, and trust.
- [Running queries](../guides/running-queries.md) — the full set of per-call overrides.
- [Node targeting](../guides/node-targeting.md) — route a whole job to one specific node.
- [Paid queries (TON)](../guides/paid-queries.md) — opt into accountable, settled compute.
