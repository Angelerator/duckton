# SQL function reference

Every capability is a DuckDB **table function**, callable as `SELECT * FROM
fn(...)` or `CALL fn(...)`. Both `name => value` and `name = value` argument
syntaxes work. Setters validate through the typed config store and return a
friendly, actionable message on bad input — never a panic. Reads redact secrets.

## Query & execution

| Function | Purpose |
|---|---|
| `p2p_query(sql, [overrides...])` | Run SQL on the grid (local-first, or `prefer => 'remote'`). See [Running queries](../guides/running-queries.md) for all overrides. |
| `p2p_query_meta(sql, ...)` | The execution/verification metadata for a query (executed_locally, verified, agreement vs quorum, winner, agreed hash, counts). On failure it returns `verified=false` + an `error` row rather than raising. |
| `p2p_node_metadata()` | This node's signed system/capability metadata (machine-class hint; never a trust input). |

## Hosting & networks

| Function | Purpose |
|---|---|
| `p2p_share(memory, threads, max_jobs, data_classes, ...)` | Donate compute and start serving others' jobs. |
| `p2p_pause()` / `p2p_resume()` | Graceful drain: stop / resume accepting new offers (in-flight jobs finish). |
| `p2p_join(bootstrap => [...])` | Join a specific swarm by seed address(es). |
| `p2p_peers()` | List discovered peers with free memory, attestation level, and trust score. |
| `p2p_network()` | The **live swarm membership** this node has learned over the discovery overlay: one row per verified, fresh, self-advertised host (node_id, addr, enabled, attestation, free mem/threads, max jobs, price, networks/groups/region, age). Empty unless running the libp2p overlay (`discovery.mode = kademlia`, built with the `discovery-libp2p` feature). |

## Inspect

| Function | Purpose |
|---|---|
| `p2p_info()` | Protocol identity (name, protocol/min versions, schema, extension version, ALPN). |
| `p2p_status()` | Node / wallet / network / economics state; prominently shows the **active network**. |
| `p2p_config()` _(alias `p2p_settings`)_ | Effective settings, grouped, with secrets redacted. |
| `p2p_blocklist()` | This node's local deny-list. |

## Configuration setters (grouped, validated)

| Function | Sets |
|---|---|
| `p2p_economics(enabled, settlement, network, fee_recipient, confirm)` | Money rail on/off, settlement backend, network mode, treasury. |
| `p2p_pricing(unit_price, max_bid)` | Provider pricing / max bid (whole TON). |
| `p2p_bidding(w_quality, w_stake, w_price)` | Bid/ranking weights. |
| `p2p_selection(replicas, quorum, checksum_min)` | Default redundancy/quorum/canary. |
| `p2p_fees(platform_fee_pct, participation_commission_frac)` | Platform fee φ and verifier commission κ. |
| `p2p_trust(min_trust, min_attest)` | Selection trust + attestation floors. |
| `p2p_planner(prefer, local_execution)` | Local-first vs remote-only routing. |
| `p2p_contracts(global_params, job_escrow, stake_vault, record_anchor)` | Per-network contract addresses. |
| `p2p_wallet(rpc, address, mnemonic_file, api_key_file, ...)` | Wallet + RPC (secrets handled via `0600` file refs). |
| `p2p_set('dotted.key.path', value)` | Generic escape hatch to **any** config key. |
| `p2p_config_reset()` | Restore defaults. |

## Provider stake

| Function | Purpose |
|---|---|
| `p2p_stake(amount => ...)` | Bond stake on-chain (eligibility/priority for paid work). |
| `p2p_unstake(amount => ...)` | Begin the 7-day unbond. |

## Anti-abuse & admin

| Function | Purpose |
|---|---|
| `p2p_block(node_id, reason)` / `p2p_unblock(node_id)` | This node's local deny-list (each node decides independently whom to serve). |
| `p2p_admin_params(...)` | Read/manage the on-chain `GlobalParams` (admin only). |

## Precedence

Per-call overrides on `p2p_query` win over everything; persisted setter state
sits above env, which sits above the config file, which sits above the built-in
defaults:

```text
built-in defaults  <  config file (P2P_CONFIG)  <  P2P_* env  <  SQL setters / p2p_set  <  per-call (=>)
```

See [Configuration](configuration.md) for the layer details.
