# How Duckton works

Duckton is a peer-to-peer **distributed DuckDB** compute grid settled on **TON**.
This doc explains the parts you actually touch: the **economic model** (free vs
paid), **what is enforced on-chain** (with real, captured testnet proof), how to
**set up a wallet** through the extension, the **four smart contracts** in plain
English, and an honest **security/threat model**.

For the full system design see [`ARCHITECTURE.md`](ARCHITECTURE.md); for the
economic layer's rationale see [`BLOCKCHAIN_ECONOMICS.md`](BLOCKCHAIN_ECONOMICS.md);
for the deploy/run-it-yourself runbook see [`TESTNET.md`](TESTNET.md).

---

## 1. The economic model: free vs paid

Duckton has two execution modes. **Free is the default** — nothing below applies
until you turn economics on.

### Free request → free nodes

A free query needs **no TON, no wallet, no contracts, no fees**. It runs on the
in-process, locked-down DuckDB engine (no network egress, no local filesystem,
`lock_configuration=true`), either locally or fanned out to other free hosts on
the grid. This is what you get out of the box:

```sql
LOAD duckton;
SELECT * FROM p2p_query('SELECT 42 AS x');   -- free, walletless
```

### Paid request

When economics is enabled (`settlement => 'ton'`) and a query is `paid`, the job
is backed by an on-chain **per-job escrow** that funds every party from a single
locked amount `B`. The split is enforced **by the escrow contract**, not by any
node's local config:

| Party | Gets | Enforced |
| --- | --- | --- |
| **Platform treasury** | **always 15%** of the quoted base | on-chain (`platform_fee_bps = 1500`) |
| **Verifier nodes** (non-winner checksummers that actually ran the query) | **5% each** of the base | on-chain (`participation_commission_bps = 500`) |
| **Winner** | the **base** | on-chain (`winnerAmount ≤ base`) |
| **Requester** | the **remainder** of `B`, refunded | on-chain |

The escrow must **cover all parties up front**. Before opening an escrow the
requester runs a **preflight** check (`need X TON to cover winner + 15% platform
fee + 5% commission × runners`); the contract independently re-checks that
`winner + fee + Σcommissions ≤ B` at settle, or it aborts.

### Free (walletless) nodes inside paid jobs

A free, walletless node **may participate in a paid job and may even win**. If a
free node wins:

- its **winner payout is 0** (it has no wallet to pay), and the winner's **base
  is refunded to the requester**;
- the platform **still collects its 15%**, and verifiers still get their 5%.

So turning on payments never lets a free participant silently divert the platform
fee — the fee is a property of the *job*, not of *who won*.

### The fee recipient is chain-authoritative

The treasury address is **not** taken from local config. It is sourced from the
on-chain `GlobalParams.fee_recipient` for the pinned `params_version`. An honest
requester/coordinator **refuses to open or settle** any escrow whose bound
treasury disagrees with that on-chain value
(`SettleError::TreasuryMismatch { bound, expected, params_version }`), so the
admin treasury can never be silently replaced by a local-config override.

---

## 2. Real on-chain proof (TON testnet)

Everything below was **captured live on the TON testnet** (workchain 0) and is
recorded in [`ton/deployments/upgrade_proof.testnet.env`](../ton/deployments/upgrade_proof.testnet.env).
These are not illustrative numbers — they are the actual deployed addresses and
settle transactions. Explorer base: `https://testnet.tonviewer.com/<address>`.

### Deployed contracts

| Contract | Testnet address | Key on-chain state |
| --- | --- | --- |
| **GlobalParams** | [`kQC_cuafJQo9cycuivJfPHE5XGMrvZUP-1sN1Kq0jtZg3dna`](https://testnet.tonviewer.com/kQC_cuafJQo9cycuivJfPHE5XGMrvZUP-1sN1Kq0jtZg3dna) | `get_fee_recipient` = treasury; `platform_fee_bps = 1500` (15%); `participation_commission_bps = 500` (5%) |
| **StakeVault** | [`kQAYPc8qAo5YUKpgcTANIAi1umHrEhrp2nG_Lnkycvo0G29q`](https://testnet.tonviewer.com/kQAYPc8qAo5YUKpgcTANIAi1umHrEhrp2nG_Lnkycvo0G29q) | unbonding = `604800s` (7d); `minStake = 0.1` TON |
| **Receipt-jetton wallet** | [`kQC2uYnSZcZW2EXJkUXE5QESw8DUwITtam5qt-E0q7ZoM4Wl`](https://testnet.tonviewer.com/kQC2uYnSZcZW2EXJkUXE5QESw8DUwITtam5qt-E0q7ZoM4Wl) | deployer's Duckton receipt holder (minted 1:1, transfer-locked) |

Supporting wallets used by the scenarios:

| Role | Address |
| --- | --- |
| Requester (deployer) | [`kQBfRkK8mJMD87yeozXp98PclFUPysbLiX6z7eJC1-JXAarQ`](https://testnet.tonviewer.com/kQBfRkK8mJMD87yeozXp98PclFUPysbLiX6z7eJC1-JXAarQ) |
| Treasury / fee recipient | [`kQAl4XDL1zstQxysQeLPHJMlxSKvI_VcS2pBNzcPhqdZWXtT`](https://testnet.tonviewer.com/kQAl4XDL1zstQxysQeLPHJMlxSKvI_VcS2pBNzcPhqdZWXtT) |
| Winner `frost-owl` (B1) | [`kQAw1XK6-Cdoh5W7BU8YpP0DNEa0wlYm-wlDTWZLOr-SX-1q`](https://testnet.tonviewer.com/kQAw1XK6-Cdoh5W7BU8YpP0DNEa0wlYm-wlDTWZLOr-SX-1q) |
| Verifier `harbor-vole` | [`kQCkcSNzW6BHYivOVBiYTogG1zU1xsD51s9Svs0N-u_alr-g`](https://testnet.tonviewer.com/kQCkcSNzW6BHYivOVBiYTogG1zU1xsD51s9Svs0N-u_alr-g) |
| Free winner `tidal-fox` (B2) | [`kQA6Azm9JTzvtpFF2ME6r9i6MrX_5ZXkSdgdMCehbsSsXm1e`](https://testnet.tonviewer.com/kQA6Azm9JTzvtpFF2ME6r9i6MrX_5ZXkSdgdMCehbsSsXm1e) |

### B1 — wallet winner, happy path

A paid job with `base = 0.04`, escrow `B = 0.05`. The single settle transaction
paid every party in the exact split, with **zero bounces**.

- **Escrow:** [`kQDHgryO_RSzQ_fNbmGKBYWzd6KBMa2JD-cM3YxxLHH0xKwB`](https://testnet.tonviewer.com/kQDHgryO_RSzQ_fNbmGKBYWzd6KBMa2JD-cM3YxxLHH0xKwB)
- **Settle tx:** `17HGqsNqVI8AoU95l0UmGBCMR4iG7OGmqA6elH+utBQ=`

| Recipient | Amount (TON) | Share |
| --- | --- | --- |
| Winner `frost-owl` | **+0.04** | base |
| Treasury | **+0.006** | 15% of base |
| Verifier `harbor-vole` | **+0.002** | 5% of base |
| Requester | remainder of `B` | refund |

### B2 — free (walletless) winner

Same shape, but the winner is a **free node** (`winnerAmount = 0`). The platform
fee is **still** collected and the base is folded back to the requester.

- **Escrow:** [`kQDmytj0PleYijDRFs45HuVycKrBXY48xndHUsl6P9Si80zl`](https://testnet.tonviewer.com/kQDmytj0PleYijDRFs45HuVycKrBXY48xndHUsl6P9Si80zl)
- **Settle tx:** `uQo6FLoFJMIPb2bCnYAvKcPqHAslAObWIAcNpn0Mqec=`

| Recipient | Amount (TON) | Share |
| --- | --- | --- |
| Winner (free) | **+0** | base earns nothing |
| Treasury | **+0.006** | **15% STILL collected** |
| Verifier | **+0.002** | 5% of base |
| Requester | remainder of `B` | refund (incl. the unspent `0.04` base) |

### B3 — negatives (all correctly rejected)

| Case | Outcome | Escrow |
| --- | --- | --- |
| **Wrong platform fee** | on-chain abort `exit_code = 285 FEE_MISMATCH` | [`kQDb_fDXEsgFfLk-0QBo8sGqHw-EAwReLaRpjy77pn6X7uT8`](https://testnet.tonviewer.com/kQDb_fDXEsgFfLk-0QBo8sGqHw-EAwReLaRpjy77pn6X7uT8) |
| **Under-funded escrow** | on-chain abort `exit_code = 226 PAYOUT_EXCEEDS_ESCROW`, plus the off-chain preflight refuses first (`insufficient escrow: need X TON …`) | [`kQDA7lKFnIeKZ1pjIqJUXfWmK8-8fcao3U51cNJ3h7kUeO4v`](https://testnet.tonviewer.com/kQDA7lKFnIeKZ1pjIqJUXfWmK8-8fcao3U51cNJ3h7kUeO4v) |
| **Mismatched treasury** | escrow's `get_fee_recipient` ≠ `GlobalParams.fee_recipient`, so the honest coordinator refuses (`SettleError::TreasuryMismatch`, `params_version 1`) before sending | [`kQDKP5cBaX8HaB0DwTvE5EhcUQ0zAs2oCV4ekg6Jp6gu6q4n`](https://testnet.tonviewer.com/kQDKP5cBaX8HaB0DwTvE5EhcUQ0zAs2oCV4ekg6Jp6gu6q4n) |

### C — staking lifecycle

Deposit → receipt → unbond → early-withdraw rejection, all on-chain:

1. **Deposit 0.1 TON** into the StakeVault → a **receipt jetton is minted 1:1**
   (locked, non-transferable), and `eligible = true`.
2. **Request unbond** → the unbonding deadline is set to **exactly +604800s
   (7 days)**: `unbondingAt = 1781629981`, `readyAt = 1782234781`.
3. **Immediate withdraw** (before the cooldown) → on-chain abort
   `exit_code = 203 COOLDOWN_NOT_ELAPSED`; the stake stays locked.

### Settlement accounting notes

- Each per-job escrow keeps a **`MIN_TONS_FOR_STORAGE = 0.05` TON** storage
  reserve (separate from the payout math, so it can never starve the contract).
- Payout legs are **exact**: they send with `PAY_FEES_SEPARATELY`, so each
  recipient receives precisely its amount and the tiny **~0.000052 TON/leg**
  storage-rent is paid on top by the escrow, not deducted from the payout.

---

## 3. Wallet setup walkthrough (via the extension)

Everything is driven from SQL through the `duckton` extension; **secrets are
referenced by file path, never pasted**. Free usage needs none of this.

**1 — enable economics + pick the rail and network:**

```sql
CALL p2p_economics(enabled => true, settlement => 'ton', network => 'testnet');
```

**2 — point at a wallet** (mnemonic + RPC + API key are read from files outside
the repo; raw seeds are never persisted to config):

```sql
CALL p2p_wallet(
  mnemonic_file => '/path/outside/repo/deployer.mnemonic',
  rpc           => 'https://testnet.toncenter.com/api/v2',
  api_key_file  => '/path/outside/repo/toncenter.key'
);
```

**3 — register the deployed contracts** for the active network:

```sql
CALL p2p_contracts(
  global_params => 'kQC_cuafJQo9cycuivJfPHE5XGMrvZUP-1sN1Kq0jtZg3dna',
  stake_vault   => 'kQAYPc8qAo5YUKpgcTANIAi1umHrEhrp2nG_Lnkycvo0G29q',
  job_escrow    => 'kQ...per-job-template',
  record_anchor => 'kQ...anchor'
);
```

**4 — verify** (secrets come back **redacted**):

```sql
SELECT * FROM p2p_status();   -- node/wallet/network/economics summary
SELECT * FROM p2p_config();   -- effective settings, grouped, secrets redacted
```

Providers who want to stake then use `CALL p2p_stake(amount => N)` /
`CALL p2p_unstake(amount => N)`; the admin pushes economic parameters to
`GlobalParams` with `CALL p2p_admin_params()`. (Stake/admin broadcasts require a
`--features ton-live` build; otherwise they report "prepared".)

---

## 4. The four contracts in plain English

| Contract | Think of it as… | What it does |
| --- | --- | --- |
| **GlobalParams** | the admin rulebook | Holds platform-wide config: the **fee recipient**, the **15% / 5%** rates, min stake, quorum, and a blocklist. Only the admin can change it; it is a singleton, edited in place (its address is stable), and supports in-place code upgrades. |
| **StakeVault** | a collateral box | A provider stakes TON → receives a **transfer-locked receipt jetton** (1:1) + a reputation score, becomes eligible to serve paid jobs. Stake is **slashable** for misbehavior and requires a **7-day unbonding** wait before it can be withdrawn. |
| **JobEscrow** | a per-job cash register | Holds the job's funds `B` and, at settle, pays the **winner + verifiers + 15% fee** and **refunds the rest** to the requester. It **won't release** unless the split is correct (right fee, payout within `B`, matching result hash). One escrow per job. |
| **RecordAnchor** | a tamper-proof logbook | Anchors per-epoch Merkle **roots of result hashes** on-chain and tracks **disputes**, so any party can later prove what a job produced (inclusion proofs verified on-chain). |

---

## 5. Security / threat model (honest)

The core guarantee: **changing your own local config only affects your own
node.** A tamperer can't move money or forge results by editing their files —
they only isolate themselves. Peers independently verify everything:

- **Signed receipts** for every execution (Ed25519, identity-bound).
- **Quorum** of **byte-identical canonical result hashes** (BLAKE3,
  order-independent) — a lone wrong answer loses.
- **On-chain stake and escrow** — funds release only on the contract-enforced
  split; a wrong fee/under-funded/mismatched-treasury settle is **rejected**
  (see §2's B3 negatives, captured live).
- **Attestation + request scoping** proofs gate which hosts may even see a job.

So a node that lies about its config, fee recipient, or results gets deselected,
refused, or aborted — it cannot drag the rest of the network along.

### Residual gaps (we don't oversell this)

1. **Escrow can't synchronously read GlobalParams.** TON contracts can't do a
   sync cross-contract read, so fee-recipient enforcement relies on a
   combination of: the **requester's synchronous read** of `GlobalParams` when
   opening the escrow, **honest-party rejection** at settle
   (`TreasuryMismatch`), and **deterministic addressing**. A *fully* trustless
   version would need a `GlobalParams`-signed proof carried into the escrow, or
   settle routed through `GlobalParams`.
2. **A free winner's `base` is arbiter-supplied at settle.** For a walletless
   winner there is no on-chain payout to cross-check the quoted base against, so
   a **colluding arbiter + requester could under-declare it**. (The platform's
   15% is still computed from whatever base is declared.)
3. **Some defenses are opt-in.** A live stake registry, cross-node reputation
   sharing, and the stricter request-scoping tiers are not all on by default —
   they are opt-in and configured per deployment.

These are real limitations of the current implementation, not hypotheticals.
The on-chain split, fee enforcement, and staking lifecycle in §2 are proven; the
trust-minimization items above are the honest frontier.
