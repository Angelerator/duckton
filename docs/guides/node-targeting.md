# Node targeting & single-node routing

By default the coordinator discovers a candidate set, offers to several hosts,
and dispatches to the top-`k` by score. Sometimes you instead want to send a job
to **exactly one node** — or a specific small set. Duckton supports this at every
layer, composably.

## The `nodes` override

`p2p_query(..., nodes => ['b3:...'])` restricts a job to the **exact node id(s)**
you list. It is **fail-closed**: a candidate whose id is unknown (or not in the
set) is never selected, and a target that matches no reachable candidate yields a
clear `NoCandidates` error — there is **no silent fallback** to a different node.

```sql
-- Send the whole job to one specific node, accept its single result:
SELECT * FROM p2p_query(
  'SELECT ...',
  nodes    => ['b3:7b1f…the-node-id…'],
  replicas => 1,
  quorum   => 1
);
```

You can find node ids with `p2p_peers()`:

```sql
SELECT node_id, free_mem, attestation_level, trust_score FROM p2p_peers();
```

How it works: the target set is enforced **inside discovery** (so a targeted node
is reliably returned, never randomly sampled out) **and** re-checked when the
coordinator selects winners.

## The three "one node" modes

| Goal | How |
|---|---|
| **Run it on myself** (no network at all) | `prefer => 'local'` — executes in-process; no offer/bid/dispatch, no other node ever sees it. |
| **Dispatch to exactly one node (no redundancy)** | `replicas => 1, quorum => 1` — the full SQL goes to one selected worker; first result returns. |
| **Send to one *specific* node** | `nodes => ['b3:...']` (optionally with `replicas => 1, quorum => 1`). |

To also limit the **offer** fan-out so only one node is even contacted, set
`discovery.candidate_sample_size = 1` in config (it auto-widens to `≥ replicas`).

## Targeting by scope instead of id

When you don't know an exact id but want to constrain *which* nodes are eligible,
use the scoping overrides:

- `network => '...'` — a logical grid partition.
- `groups => [...]` — a private pool a host must share.
- `regions => [...]` — geographic residency (fail-closed).

In [private / enterprise mode](../PRIVATE_MODE.md), the identity allowlist plus
`candidate_sample_size = 1` lets you pin to a single roster member.

## Validation

`replicas ≥ 1`, `quorum ≥ 1`, `quorum ≤ replicas`, and
`candidate_sample_size ≥ replicas` — so `replicas = 1, quorum = 1,
candidate_sample_size = 1` is a valid single-node configuration.
