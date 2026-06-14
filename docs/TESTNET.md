# TON testnet — deploy + live end-to-end runbook

This is a **turnkey** runbook for taking the on-chain economic/settlement layer
of the P2P DuckDB-over-QUIC grid live on the **TON testnet**. The contracts
(`StakeVault`, `StakeReceiptWallet`, `JobEscrow`, `RecordAnchor`, `GlobalParams`)
live in [`ton/`](../ton) (Tolk, built/tested with Acton 1.1.0); the off-chain
settlement logic (message ABI, `ton_proof` binding, Merkle proofs) lives in
[`crates/settlement`](../crates/settlement). See
[`docs/BLOCKCHAIN_ECONOMICS.md`](BLOCKCHAIN_ECONOMICS.md) for the full design.

A fresh run **redeploys all four deployable contracts** — `StakeVault` (+ its
`StakeReceiptWallet` jetton master), `RecordAnchor`, `JobEscrow` and the
platform-wide `GlobalParams` — and exercises the full scenario set against them.

Everything is driven by **one** script — [`scripts/testnet_e2e.sh`](../scripts/testnet_e2e.sh) —
which deploys, verifies, points config at the deployed addresses, and runs the
full live scenario. **You only need to supply a funded testnet wallet + a
Toncenter testnet API key.** Nothing here touches a network until you run the
script with those inputs.

## Verified live testnet deployments

All four core contracts have been **deployed and exercised on the live TON
testnet** (workchain 0). Explorer: `https://testnet.tonviewer.com/<address>`.

| Contract | Testnet address | Live checks |
| --- | --- | --- |
| **GlobalParams** | [`kQAagsi-ThkgbOxxVwXd0CHbSXpZwbLmdfjFAx76PL4bwOna`](https://testnet.tonviewer.com/kQAagsi-ThkgbOxxVwXd0CHbSXpZwbLmdfjFAx76PL4bwOna) | `get_params_version` → `1`; resilience fields present |
| **StakeVault** (Duckton master) | [`kQD90f-cm-a4EKkUFV1q7khQgsBbxZpQL6NySZjXAbIVrrkp`](https://testnet.tonviewer.com/kQD90f-cm-a4EKkUFV1q7khQgsBbxZpQL6NySZjXAbIVrrkp) | indexer renders **Duckton / DUCKTON / 9**; total_supply 0.2; mintable |
| **Duckton holder** (receipt wallet) | [`kQBPNBIv2op4Qd_na03As9YEc8FX3-8ng-RVseTEO9jEXR6r`](https://testnet.tonviewer.com/kQBPNBIv2op4Qd_na03As9YEc8FX3-8ng-RVseTEO9jEXR6r) | balance **0.2 Duckton**, minted 1:1, transfer-locked |
| **RecordAnchor** | [`kQDhHXS08DLghUFm_ofEI_NWM--4ICUIVv5IV_VQjjH78RK3`](https://testnet.tonviewer.com/kQDhHXS08DLghUFm_ofEI_NWM--4ICUIVv5IV_VQjjH78RK3) | single + 8-leaf inclusion proofs verified; tamper rejected; bonded dispute open+resolve |
| **JobEscrow** (per-job, Rust paid-flow) | [`0:edbeca37fcf34549e5ffba5173bc7bbebce53f8089355e621b16e5ddddd4fe23`](https://testnet.tonviewer.com/0:edbeca37fcf34549e5ffba5173bc7bbebce53f8089355e621b16e5ddddd4fe23) | open-escrow-per-job → settle; `params_version`=1 + quorum hash bound and verified on-chain |
| Deployer / owner wallet | [`kQCP7UqEfNwpaaNGDP3MihPPBb-Yd5ZYc0EU-VbXcmjpg422`](https://testnet.tonviewer.com/kQCP7UqEfNwpaaNGDP3MihPPBb-Yd5ZYc0EU-VbXcmjpg422) | holds the Duckton receipt; funds the live runs |

Reproduce the two focused live demos any time (gas-light, idempotent — each
deploys a fresh instance):

```bash
cd ton
acton script scripts/show_duckton.tolk --net testnet   # deploy vault + mint a visible Duckton
acton script scripts/show_anchor.tolk  --net testnet    # anchor roots + inclusion proofs + dispute
```

**Address stability / pinning.** A TON address is a pure function of
`(code, initial data)` — deterministic, not random. To keep ONE canonical,
stable instance across runs, pin its address (the scripts then *reuse* it
instead of deploying a new one):

```bash
VAULT_ADDR=kQD90f-cm-a4EKkUFV1q7khQgsBbxZpQL6NySZjXAbIVrrkp \
  acton script scripts/show_duckton.tolk --net testnet     # mints into the SAME Duckton master
ANCHOR_ADDR=kQDhHXS08DLghUFm_ofEI_NWM--4ICUIVv5IV_VQjjH78RK3 \
  acton script scripts/show_anchor.tolk  --net testnet     # anchors the next epoch on the SAME contract
```

The running grid pins the same way via `[economics.<net>.contracts]` (or
`p2p_contracts(...)` from SQL), so the extension/coordinator always reference
the deployed contracts. **`GlobalParams` is a singleton — deploy it once and
edit it in place via `update_params` (its address is stable); redeploying it
mints a brand-new instance with reset state.** A per-job `JobEscrow` address is
*intended* to differ per job (it deterministically commits to that job's
`expected_hash` + `params_version` + bid); a per-node `StakeVault` is one per
node owner.

> **Coverage.** Beyond the live runs above, the full scenario set is also
> validated in Acton's **local emulator** against the real compiled contracts —
> GlobalParams admin update/non-admin rejection/blocklist + monotonic version,
> stake deposit, 1:1 transfer-locked receipt + Duckton TEP-64 metadata, escrow
> settle, single- and multi-leaf anchor inclusion, and dispute (**68** emulator
> tests, including `tests/e2e_flow.test.tolk` mirroring the whole flow across all
> four contracts in one session). The harness is `bash -n`-clean and the
> `ton-live` Rust paid-flow test is a no-op without testnet env.

---

## 0. What the harness does (the live loop)

`scripts/testnet_e2e.sh`, against testnet:

1. **imports** your wallet (idempotent), **builds** the contracts (`acton build`)
   and the loadable **DuckDB extension** (`scripts/build_extension.sh`);
2. runs a **real DuckDB query through the loaded extension** and hashes the
   result — this hash becomes the escrow's HTLC lock and the anchored epoch leaf;
3. verifies the **`ton_proof` two-way wallet↔node binding** (pure-Rust Ed25519 +
   sha256, both directions) and computes an on-chain binding hash;
4. **deploys** the four contracts: `StakeVault` (+ its receipt-jetton master),
   `RecordAnchor`, `JobEscrow` (opened with the locked bid `B`, HTLC-locked on the
   query result hash), and `GlobalParams` (platform-wide economic config, admin =
   your wallet);
5. runs **`acton verify`** on each (source ↔ published bytecode);
6. **records** the deployed addresses into a generated config file;
7. runs the **live scenario** in one broadcast session:
   GlobalParams admin `update_params` (persisted, address stable) → governance
   blocklist set/clear round-trip → stake deposit → 1:1 transfer-locked
   stake-receipt jetton + Duckton TEP-64 metadata assertion → escrow settle
   (winner + platform fee + participation commissions, remainder refunded) →
   anchor epoch root → on-chain single- **and** multi-leaf (8-leaf) Merkle
   inclusion proofs → *optional* bonded dispute → GlobalParams non-admin
   `update_params` rejection (against a throwaway admin); finally a live-RPC
   wallet-bind confirmation read from `StakeVault.get_binding_hash`;
8. prints a **PASS/FAIL summary with `testnet.tonviewer.com` links**;
9. *(optional)* runs the **`ton-live`-gated Rust test** against the live RPC.

Each new scenario emits a parseable `::CHECK::<name>::PASS|FAIL` marker the
harness folds into the summary.

It is **re-runnable**: the wallet import is idempotent, `StakeVault`/`RecordAnchor`/
`GlobalParams` have deterministic addresses (re-deploy is a harmless top-up),
`GlobalParams` is mutated **in place** at that stable address (the admin update
toggles the platform-fee bps so the write is provable on every re-run), the anchor
epoch is read from chain and advanced each run (it now advances by **two** — one
single-leaf, one multi-leaf root), and each run opens a **fresh** escrow (its
address varies with the per-run deadline) so settle always targets an unsettled
escrow.

---

## 1. Prerequisites

| Tool | Why | Install |
|---|---|---|
| **Acton 1.1.0** | build/deploy/verify the Tolk contracts | `curl -LsSf https://github.com/ton-blockchain/acton/releases/latest/download/acton-installer.sh \| sh` |
| **DuckDB CLI** | run the real query through the extension | <https://duckdb.org/docs/installation> |
| **Rust toolchain** | build the extension + run the `ton_proof`/`ton-live` tests | <https://rustup.rs> |
| **curl, jq** | RPC reads + parse Acton JSON | usually preinstalled |
| **sha256sum / shasum** | hash the query result | usually preinstalled |

### macOS: the engine needs the SDK path

Acton's execution engine and the extension build both need the macOS SDK path:

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

The harness sets this for you if it's unset, but export it in your shell if you
run the underlying commands by hand.

---

## 2. Create + fund a testnet wallet

You can let Acton create one, or import an existing mnemonic.

```bash
# create a fresh testnet wallet and request faucet GRAM
acton wallet new --name deployer --local --version v5r1 --airdrop

# or import an existing 24-word mnemonic
acton wallet import --name deployer --local --version v5r1 "word1 word2 ... word24"
```

> The harness imports the mnemonic for you (under the name `deployer` by
> default), so for the turnkey path you only need the **mnemonic string** — see
> §4. The wallet **must hold testnet GRAM**.

**Fund it** (a few test-GRAM is plenty for the whole loop — defaults are tiny):

- Telegram faucet: **[@testgiver_ton_bot](https://t.me/testgiver_ton_bot)** — send your wallet address.
- or `acton wallet airdrop deployer --net testnet` (PoW faucet; may be rate-limited).

Check the balance:

```bash
acton wallet list --balance
```

Treat the mnemonic like a real secret — **never commit it to git** (testnet GRAM
has no value, but the habit matters).

## 3. Get a Toncenter testnet API key

Acton and the `ton-live` Rust test talk to Toncenter testnet. Without a key you
are throttled to ~1 RPS (slow, but works — pass `ALLOW_NO_API_KEY=1`).

- Get a **testnet** key from **[@tonapibot](https://t.me/tonapibot)** (or [@toncenter](https://t.me/toncenter)).
- The default RPC endpoint is `https://testnet.toncenter.com/api/v2`.

---

## 4. Inputs the harness reads (env vars / config file)

Set these as environment variables, or put them in a file and pass
`--config <file>` (a `scripts/testnet_e2e.env` next to the script is auto-loaded).
**Keep that file out of git** (add it to `.gitignore`).

### Required

| Variable | Meaning |
|---|---|
| `TON_TESTNET_MNEMONIC` | 24-word (or 12) wallet mnemonic, space-separated |
| `TON_TESTNET_MNEMONIC_FILE` | …or a path to a file containing the mnemonic (use instead of the above) |

### Recommended

| Variable | Meaning |
|---|---|
| `TON_TESTNET_API_KEY` | Toncenter **testnet** API key (exported to Acton as `TONCENTER_TESTNET_API_KEY`) |

### Optional (defaults shown)

| Variable | Default | Meaning |
|---|---|---|
| `TON_TESTNET_RPC` | `https://testnet.toncenter.com/api/v2` | RPC endpoint used by the live RPC reads + the `ton-live` Rust test |
| `WALLET_NAME` | `deployer` | Acton wallet name the Tolk scripts resolve (`scripts.wallet("deployer")`) |
| `WALLET_VERSION` | `v5r1` | wallet contract version for import |
| `OUT_DIR` | `ton/deployments` | where generated config/addresses/logs are written |
| `NODE_ID` | `b3:demo-node` | node id folded into the on-chain binding hash |
| `STAKE_DEPOSIT_AMOUNT` | `100000000` (0.1 TON) | nanoton to bond into `StakeVault` |
| `ESCROW_AMOUNT` | `300000000` (0.3 TON) | nanoton locked in the per-job escrow (`B`) |
| `ESCROW_WINDOW_SECS` | `3600` | refund-on-timeout window |
| `SETTLE_WINNER_AMOUNT` | `100000000` (0.1 TON) | winner base + bonus payout |
| `SETTLE_FEE` | `20000000` (0.02 TON) | platform fee to the treasury |
| `SETTLE_PARTICIPANT_AMOUNT` | `20000000` (0.02 TON) | participation commission to an agreeing non-winner |
| `E2E_RUN_DISPUTE` | `0` | set `1` to also run the bonded dispute round |
| `DISPUTE_BOND` | `200000000` (0.2 TON) | challenger bond when the dispute round runs |
| `SKIP_EXTENSION_BUILD` | `0` | set `1` to reuse an existing `dist/` extension |
| `SKIP_VERIFY` | `0` | set `1` to skip `acton verify` |
| `RUN_RUST_LIVE_TEST` | `0` | set `1` to run the `ton-live` Rust test at the end |
| `DUCKDB_QUERY_FILE` | *(built-in)* | path to your own `.sql` to run through the extension |
| `ALLOW_NO_API_KEY` | `0` | set `1` to run keyless (throttled) |

> All amounts are **nanoton** (1 TON = 1e9). Defaults are intentionally tiny, and
> the winner/treasury/participant all default to **your own wallet**, so funds
> cycle back to you.

---

## 5. Run it

```bash
# minimal (export inline)
TON_TESTNET_MNEMONIC="word1 word2 ... word24" \
TON_TESTNET_API_KEY="your-testnet-key" \
  scripts/testnet_e2e.sh
```

Or with a config file (recommended — keeps secrets out of shell history):

```bash
cat > scripts/testnet_e2e.env <<'EOF'
TON_TESTNET_MNEMONIC="word1 word2 ... word24"
TON_TESTNET_API_KEY="your-testnet-key"
# TON_TESTNET_RPC="https://testnet.toncenter.com/api/v2"
# E2E_RUN_DISPUTE=1
# RUN_RUST_LIVE_TEST=1
EOF
echo "scripts/testnet_e2e.env" >> .gitignore

scripts/testnet_e2e.sh                 # auto-loads scripts/testnet_e2e.env
# or: scripts/testnet_e2e.sh --config /path/to/other.env
```

The harness deploys and runs everything, then prints a summary like:

```text
== SUMMARY ==
Network:      testnet
Wallet:       kQ...deployer
StakeVault:   kQ...vault
                https://testnet.tonviewer.com/kQ...vault
RecordAnchor: kQ...anchor
                https://testnet.tonviewer.com/kQ...anchor
JobEscrow:    kQ...escrow
                https://testnet.tonviewer.com/kQ...escrow
GlobalParams: kQ...gparams
                https://testnet.tonviewer.com/kQ...gparams
Result hash:  0x<64-hex>
Config:       ton/deployments/testnet.env
              ton/deployments/economics.testnet.toml

Checks:
  PASS  ton_proof binding crypto
  PASS  stake deposit (staked=100000000)
  PASS  stake-receipt jetton minted 1:1 + transfer-locked (minted=100000000)
  PASS  stake-receipt jetton transfer rejected (anti-exit)
  PASS  Duckton TEP-64 metadata (name/symbol/decimals)
  PASS  escrow settle (HTLC release)
  PASS  anchor epoch root (epoch=1)
  PASS  inclusion proof verified on-chain
  PASS  multi-leaf Merkle inclusion verified on-chain
  PASS  GlobalParams admin update_params (persisted, address stable)
  PASS  GlobalParams governance blocklist round-trip
  PASS  GlobalParams non-admin update_params rejected
  PASS  wallet-bind anchored on-chain
  PASS  verify StakeVault
  PASS  verify GlobalParams
  ...
 ALL CHECKS PASSED
```

Per-step Acton transaction traces and `acton verify` output are saved under
`ton/deployments/logs/`.

---

## 6. Pointing the node config at the deployed contracts

The harness writes two files into `OUT_DIR` (default `ton/deployments/`):

- **`testnet.env`** — machine-readable addresses, sourced by re-runs and the
  `ton-live` Rust test:

  ```bash
  set -a; . ton/deployments/testnet.env; set +a
  # exports TON_TESTNET_{VAULT,ANCHOR,ESCROW,GLOBAL_PARAMS}_ADDR, TON_TESTNET_RPC,
  # TON_TESTNET_RESULT_HASH, ...
  ```

- **`economics.testnet.toml`** — a node `[economics]` snippet pointed at testnet
  with your wallet as the fee recipient. Merge it into your node config or point
  the node at it via `P2P_CONFIG`:

  ```toml
  [economics]
  enabled         = true
  settlement      = "onchain"
  network         = "testnet"
  default_payment = "paid"
  fee_recipient   = "kQ...your-wallet"

  [economics.testnet.contracts]
  stake_vault   = "kQ...vault"
  job_escrow    = "kQ...escrow"
  record_anchor = "kQ...anchor"
  global_params = "kQ...gparams"
  ```

> The generated TOML records **all four** deployed addresses under
> `[economics.<net>.contracts]` (including the platform-wide `GlobalParams`). The
> machine-readable `testnet.env` also exports them as `TON_TESTNET_*_ADDR` for
> re-runs and the `ton-live` Rust test.

---

## 7. The `ton-live` Rust integration test

A Rust test exercises the `ton` settlement impl (message ABI + the `TonRpc`
read seam) against the live RPC. It is **gated behind the `ton-live` feature**, so
it is a **no-op in normal CI** (`cargo test --workspace` never compiles it), and
even with the feature on it **skips** unless the testnet env is set.

```bash
# normal CI — testnet_live.rs is not compiled, nothing changes:
cargo test --workspace

# run the live test (after a harness run wrote testnet.env):
set -a; . ton/deployments/testnet.env; set +a
cargo test -p p2p-settlement --features ton-live --test testnet_live -- --nocapture
```

It reads `TON_TESTNET_RPC`, `TON_TESTNET_API_KEY`, and
`TON_TESTNET_{VAULT,ANCHOR,ESCROW}_ADDR`, then:

- pins the on-chain message ABI (opcodes + byte layout) — runs always;
- reads `get_vault_state` / `get_anchor_state` from the **real deployed
  contracts** over Toncenter via the `TonRpc` seam (skipped if env absent);
- documents the boundary that **broadcasting** signed wallet transactions is the
  Acton harness's job, not the Rust seam's.

The harness can run it for you at the end with `RUN_RUST_LIVE_TEST=1`.

---

## 8. Confirm on the explorer

Open the printed `https://testnet.tonviewer.com/<address>` links and confirm:

- **StakeVault** — balance reflects the bonded deposit; `get_vault_state` shows
  `staked > 0`, `eligible = true`; the receipt-jetton wallet was minted.
- **JobEscrow** — `settled = true`; outgoing messages show the winner payout, the
  platform fee to the treasury, the participation commission, and the refund of
  the unspent escrow slack to the requester.
- **RecordAnchor** — `get_anchor_state` shows the advanced epoch and the anchored
  root; `verify_inclusion` returns `true` for both the single-leaf and the
  multi-leaf (8-leaf) interior-leaf proofs.
- **GlobalParams** — `get_params` shows the admin-updated platform-fee bps and
  `get_admin` is your wallet (address unchanged across the update); `get_blocklisted`
  round-trips `true`→`false`; the non-admin update against the throwaway instance
  bounced (`exit code 260 NOT_ADMIN`).

You can also retrace any transaction locally with Acton:

```bash
acton rpc info   <ADDRESS> --net testnet
acton rpc trace  <TX_HASH> --net testnet
acton retrace    <TX_HASH> --net testnet --contract StakeVault
```

---

## 9. Running pieces by hand (optional)

The harness wraps these; run them individually for debugging. From `ton/`:

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
acton build

# deploy (each prints "Deployed <Contract> to <addr>")
STAKE_BINDING_HASH=0x<hash> acton script scripts/deploy_stake.tolk         --net testnet
ANCHOR_BOND_MIN=100000000   acton script scripts/deploy_anchor.tolk        --net testnet
ESCROW_AMOUNT=300000000 ESCROW_DEADLINE=$(( $(date +%s)+3600 )) \
  ESCROW_EXPECTED_HASH=0x<hash> acton script scripts/deploy_escrow.tolk    --net testnet
GP_FEE_RECIPIENT=<addr>     acton script scripts/deploy_global_params.tolk --net testnet

# verify
acton verify StakeVault   --address <addr> --net testnet
acton verify RecordAnchor --address <addr> --net testnet
acton verify JobEscrow    --address <addr> --net testnet
acton verify GlobalParams --address <addr> --net testnet

# the live scenario over already-deployed addresses (needs all four)
VAULT_ADDR=<addr> ESCROW_ADDR=<addr> ANCHOR_ADDR=<addr> GLOBAL_PARAMS_ADDR=<addr> \
  SETTLE_RESULT_HASH=0x<hash> ANCHOR_ROOT=0x<hash> ANCHOR_LEAF=0x<hash> \
  acton script scripts/e2e_testnet.tolk --net testnet
```

There are matching `[scripts]` aliases in `ton/Acton.toml`
(`deploy-stake-testnet`, `deploy-anchor-testnet`, `deploy-escrow-testnet`,
`deploy-global-params-testnet`, `e2e-testnet`).

---

## 10. Troubleshooting

| Symptom | Fix |
|---|---|
| `missing required tool: acton` | install Acton (see §1) and ensure `~/.acton/bin` is on `PATH`. |
| `acton build` fails on macOS with SDK errors | `export SDKROOT="$(xcrun --show-sdk-path)"`. |
| extension build fails | run `SDKROOT=$(xcrun --show-sdk-path) scripts/build_extension.sh` directly and read the error; needs the Rust toolchain + DuckDB headers. |
| `set TON_TESTNET_MNEMONIC ...` | export the mnemonic (or `TON_TESTNET_MNEMONIC_FILE`). |
| `set TON_TESTNET_API_KEY ...` | get a testnet key from [@tonapibot](https://t.me/tonapibot), or pass `ALLOW_NO_API_KEY=1` to run keyless (slow). |
| Toncenter `429 Too Many Requests` / very slow | you are keyless or over quota — set `TON_TESTNET_API_KEY`; Acton waits to respect ~1 RPS without a key. |
| `Range check error` while deploying escrow | the deadline must fit `uint32` (a real unix timestamp does; the harness computes `date +%s + window`). |
| deploy succeeds but "failed to parse … address" | the deploy log line format changed; inspect `ton/deployments/logs/deploy_*.log`. |
| `exit code 137 (NOT_ENOUGH_GAS)` on deposit | the wallet/contract is underfunded — top up via the faucet; deposit value must cover `amount + 0.05 TON` storage + gas. |
| `exit code 222 (NOT_ARBITER)` on settle | the settling wallet isn't the escrow arbiter — the harness uses your wallet for both; if running by hand, set `ESCROW_ARBITER` to the same wallet that settles. |
| `exit code 223 (HASH_MISMATCH)` on settle | `SETTLE_RESULT_HASH` must equal the escrow's `ESCROW_EXPECTED_HASH` (the harness reuses the query result hash for both). |
| `exit code 224 (ALREADY_SETTLED)` | you re-settled the same escrow — re-run the harness (it opens a fresh escrow per run via a new deadline). |
| `exit code 241 (EPOCH_REGRESSION)` / `240 (BAD_PREV_ROOT)` on anchor | the submitted epoch must be `currentEpoch + 1` chained to `lastRoot` — the harness reads chain state and advances automatically; by hand, read `get_anchor_state` first. |
| `exit code 244 (DISPUTE_BOND_TOO_LOW)` | raise `DISPUTE_BOND` above the anchor's `disputeBondMin` (the harness deploys the anchor with a low `ANCHOR_BOND_MIN` so the optional dispute is cheap). |
| `exit code 260 (NOT_ADMIN)` on GlobalParams | expected for the **non-admin rejection** check (the throwaway instance's admin is the vault, not your wallet); for a real `update_params`/`set_blocklisted`, send from the admin wallet that deployed `GlobalParams`. |
| `exit code 261 (PARAMS_OUT_OF_BOUNDS)` on GlobalParams | a proposed `update_params` violates a §12 invariant (bps ranges, slash split = 100%, min-stake monotonicity, unbonding ≥ challenge window, selection ordering). |
| `acton verify` fails or stalls | the verifier backend can be transient; the harness records it as a non-fatal check. Re-run `acton verify <C> --address <addr> --net testnet`. |
| `wallet import failed` | check the word count (12/24) and `WALLET_VERSION`; remove a stale entry with `acton wallet remove -y deployer` and re-run. |
| Rust live test "SKIP" lines | expected unless `TON_TESTNET_RPC` + `TON_TESTNET_*_ADDR` are set — source `ton/deployments/testnet.env` first. |

---

## 11. Cost & safety notes

- Testnet GRAM has no value, but the mnemonic deserves real-secret hygiene.
- The whole loop costs a fraction of a TON in gas + the (recoverable) escrow `B`
  and dispute bond; defaults keep totals well under ~1 test-GRAM.
- All payouts default to **your own wallet**, so deposits/escrow/bond largely
  return to you (minus gas).
- The contracts are strictly **non-custodial**: no platform key can seize funds;
  the harness only ever sends from your wallet.
