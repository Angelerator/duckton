# Configuration

Duckton is configured through a **layered, validated** system. You almost never
need to touch a file — zero-config defaults are safe and frictionless, and most
tuning is a per-call override or a one-line `CALL`.

## Precedence (lowest → highest)

```text
1. built-in defaults            (compiled into the binary — GridConfig::default())
2. config file                  (P2P_CONFIG=/path/to/p2p.toml, or P2P_CONFIG_DIR)
3. environment variables        (P2P_* — e.g. P2P_REPLICAS, P2P_BIND_ADDR)
4. SQL / runtime setters        (p2p_economics, p2p_selection, p2p_set, …)
5. per-call SQL parameters      (replicas =>, prefer =>, nodes =>, …)
```

Layer 4 (SQL setters) is **persisted** to a sparse runtime-overrides file
(default `<config-dir>/runtime.toml`, override with `P2P_RUNTIME_CONFIG`) so it
**survives restart**. Your hand-edited base file is never rewritten, and secrets
are kept out (in separate `0600` files). Unknown keys are a hard error
(`deny_unknown_fields`) — typos fail fast.

## The example config

A fully-documented example lives at
[`config/p2p.example.toml`](https://github.com/Angelerator/duckton/blob/main/config/p2p.example.toml).
Copy it, set `P2P_CONFIG=/path/to/p2p.toml`, and uncomment only what you need.

## Key sections

| Section | What it controls |
|---|---|
| `[protocol]` | Advertised/min protocol version; optional engine-version matching for quorum determinism. |
| `[network]` | QUIC bind/advertised address, timeouts, flow-control windows, chunk sizes. |
| `[transport.quic]` / `[transport.result]` / `[transport.compression]` | GSO/GRO, congestion control, 0-RTT, parallel result streaming, wire compression, result-size caps. See [transport tuning](../ARCHITECTURE.md). |
| `[identity]` | Key path, **pinning mode** (`tofu`/`allowlist`), peer allowlist. |
| `[security]` | `mode` = `public` / `private` (closed grid). See [Private mode](../PRIVATE_MODE.md). |
| `[discovery]` | Bootstrap seeds, `candidate_sample_size` (bounded fan-out). |
| `[scheduler]` | `replicas`, `quorum`, `verify_mode`, re-dispatch/backoff, `require_staked_hosts`. |
| `[budget]` | Host resource donation: memory, threads, max jobs, per-job caps, spill caps. |
| `[planner]` | Local-first vs `prefer = remote`; `local_execution` hard gate; spill tolerance. |
| `[trust]` | `min_trust`, `min_attestation`. |
| `[membership]` | Networks, groups (+ token issuers), region (+ region trust tier). |
| `[storage]` | Object-store providers, `enable_remote_access`, credential mode (presigned / scoped-secret / sealed), Parquet encryption keys. |
| `[sandbox]` | OS isolation backend (`noop` default; `process_per_job` opt-in), egress allow-list. |
| `[antiabuse]` | Deny-list/auto-block, free-job rate limit, cost gate, fault attribution. |
| `[economics]` | On/off, network (testnet/mainnet + confirm guard), fees, pricing, bidding, slashing, per-network wallet/contracts. |
| `[metadata]` | Signed system-profile capture interval (routing hint only). |

## Common knobs

```toml
[scheduler]
replicas = 3
quorum   = 2

[discovery]
candidate_sample_size = 16     # set to 1 to contact a single node (see node targeting)

[planner]
prefer           = "auto"      # "local" | "remote" | "auto"
local_execution  = true        # false = hard remote-only (thin clients)
```

Equivalent env / SQL:

```bash
export P2P_REPLICAS=3 P2P_QUORUM=2 P2P_CANDIDATE_SAMPLE_SIZE=16
```

```sql
CALL p2p_selection(replicas => 3, quorum => 2);
CALL p2p_planner(prefer => 'remote', local_execution => false);
CALL p2p_set('discovery.candidate_sample_size', 1);   -- generic escape hatch
```

## Secrets

Wallet mnemonics and API keys are **never** stored in the config file or echoed.
Pass file references (`mnemonic_file` / `api_key_file`) pointing **outside the
repo**; inline secrets are moved to a `0600` file under the config dir's
`secrets/` and only the path is persisted. `p2p_config()` redacts them
everywhere. See [Security](../project/security.md).
