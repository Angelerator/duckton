# Observability & troubleshooting

Duckton emits **structured logs** (via `tracing`) at the security-critical
decision and failure points, so you can answer "why did *that* happen?" without
guessing. Combined with `p2p_query_meta()` and `p2p_status()`, behavior is
observable end to end.

## Enabling logs

Logs use the standard `RUST_LOG` env filter:

```bash
# Lifecycle + posture (recommended baseline):
export RUST_LOG="p2p_node=info,p2p_settlement=info"

# Add per-offer / attestation / cancel decisions:
export RUST_LOG="p2p_node=debug,p2p_settlement=info"

# Everything (very verbose):
export RUST_LOG="p2p_node=trace,p2p_settlement=trace,p2p_transport=debug"
```

- **`info`** — node startup security-posture summary, on-chain escrow lifecycle
  (open → fund → confirm, settle, refund), and high-level outcomes.
- **`debug`** — per-offer admission decisions (why a host declined), dispatch
  authorization, attestation downgrades, cancellation/interrupt events.
- **`warn`** — every security-relevant rejection or on-chain failure.

## What gets logged where

| Question | Look for (level) |
|---|---|
| "Why isn't my host getting jobs?" | `declining offer: <reason>` (debug) — standby, blocklist, wrong group/region/network, over-budget, data-class not served. |
| "Why was my dispatch rejected?" | `rejecting unauthorized dispatch …` (warn) — no live accepting-bid authorization from this peer. |
| "Why was a host treated as L0 despite claiming L2?" | `attestation verification failed …` / `no AttestationVerifier wired …` (debug). |
| "Why did my query get aborted?" | timeout / metered-cap / loser-reset lines noting the engine interrupt (warn/debug). |
| "Why did settlement fail / not confirm?" | escrow open/settle/refund lifecycle + `not confirmed (on-chain abort or unverifiable outcome)` (info/warn). |
| "Is my node configured/securing as intended?" | the **startup posture summary** (info): bind addr, pinning mode, security mode, sandbox backend, remote-access + egress-confined, economics on/off. |

No secrets are ever logged — only ids, addresses, sizes, and reasons.

## Per-query introspection

```sql
SELECT key, value FROM p2p_query_meta('SELECT ...');
-- executed_locally, verified, agreement vs quorum, winner, agreed hash, counts
SELECT * FROM p2p_status();   -- active network, endpoints, execution mode, warnings
```

## Common situations

??? question "`NoCandidates` error"
    No host matched the request. Causes: no bootstrap seeds (you're local-first
    with no grid), all candidates filtered out by `min_trust` / `min_attest` /
    `require_staked_hosts` / scoping, or a `nodes => [...]` target that no
    reachable peer matches. Add seeds with `p2p_join`, relax the floors, or check
    the target id with `p2p_peers()`.

??? question "Paid query says I need a wallet"
    `payment => 'paid'` with no wallet configured returns an actionable error.
    Either run it free (`payment => 'free'`) or configure a wallet — see
    [Paid queries](../guides/paid-queries.md).

??? question "`FEE_MISMATCH` / `COMMISSION_MISMATCH` / `PAYOUT_EXCEEDS_ESCROW` on settle"
    The on-chain escrow rejected a settle whose fee, verifier commission, or total
    didn't match the bound `GlobalParams`. Usually a stale fee/treasury config vs.
    the synced chain policy; the coordinator now binds the synced values, so
    ensure your node has reached `GlobalParams` (check `p2p_status()`).

??? question "Extension won't `LOAD` (version error)"
    DuckDB's loadable-extension ABI requires an **exact** version match. Upgrade
    your DuckDB CLI to the version `duckton` targets (currently v1.5.4), then
    retry. See [Installation](../getting-started/installation.md).

??? question "Remote-only node falls back to local (or vice versa)"
    Set the hard gate `CALL p2p_planner(local_execution => false)` to *never* run
    locally (overrides per-call `prefer => 'local'`); `p2p_status()` then shows
    `execution_mode = remote-only`.

## On-chain debugging

For the TON settlement path, the testnet runbook covers deploy, the live
end-to-end loop, explorer verification, and a troubleshooting section — see the
[TON testnet runbook](../TESTNET.md).
