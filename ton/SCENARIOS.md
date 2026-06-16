# On-chain scenario catalog (TON testnet)

This is the **code-derived** scenario catalog for the duckdb-p2p settlement
contracts in [`ton/contracts`](contracts). Every row below is taken directly
from the Tolk sources (the opcode it exercises, the guard/error it asserts, the
getter it reads back) — not invented. It is the specification the on-chain
runner and the Acton emulator suite are measured against.

Sources of truth:

- Contracts: `StakeVault(.tolk)` + `StakeVaultV2`, `StakeReceiptWallet`,
  `JobEscrow`, `RecordAnchor` + `RecordAnchorV2`, `GlobalParams` +
  `GlobalParamsV2`; shared `common.tolk` (frozen error codes + opcodes).
- Live testnet runners: [`scripts/testnet_e2e.sh`](../scripts/testnet_e2e.sh)
  (the 14-check turnkey flow, driving [`scripts/e2e_testnet.tolk`](scripts/e2e_testnet.tolk))
  and [`scripts/scenarios_testnet.sh`](../scripts/scenarios_testnet.sh) (the
  comprehensive runner, driving [`scripts/scenarios_testnet.tolk`](scripts/scenarios_testnet.tolk)).
- Emulator suite: [`tests/`](tests) (`acton test`, 90 tests across 9 files).

## Classification legend

Each scenario is classified by **where it is proven**, which is dictated by the
contract semantics — not by convenience:

| Class | Symbol | Where | Why |
|---|---|---|---|
| **testnet-broadcast** | `T` | live testnet: broadcast → `acton rpc trace <hash>` → read getters → assert | a **positive** transition from clean state; leaves only successful txs on-chain |
| **emulator-only** | `E` | `acton test` (Acton emulator) | a **negative** / expected-to-fail case. Broadcasting it would leave a scary *aborted* tx in the explorer and burn gas for **no extra signal** — the exit code is asserted deterministically in the emulator instead |
| **time-gated emulator** | `G` | `acton test` via `testing.setNow(...)` | a **positive** transition that depends on a real **cooldown / timelock** (unbonding ≥ 7 days, keeper grace, upgrade timelock, escrow deadline) that is impractical to wait out on a live wall clock; proven by warping emulator time |

Rationale for not broadcasting negatives is baked into the contracts/scripts
themselves (see the note block in `scripts/e2e_testnet.tolk` and
`tests/stake.test.tolk`): a funded testnet run should leave **no failed
transactions** and waste no TON, so every `assert ... throw` rejection is an
emulator assertion, while every happy-path state change is broadcast live.

The on-chain runner emits a parseable `::CHECK::<name>::PASS|FAIL` marker per
broadcast scenario; the bash orchestrator folds these into a PASS/FAIL summary
with `https://testnet.tonviewer.com/<addr>` links and `acton rpc trace` of
representative tx hashes, then runs `acton test` so the negatives + time-gated
positives are covered in the same report. Nothing is left untested.

---

## 1. StakeVault (+ StakeVaultV2) — per-node bond custody / receipt-jetton master

`contracts/StakeVault.tolk`, `contracts/stake_types.tolk`. Storage:
`VaultStorage` (owner, staked, unbondingAmount/At, totalSupply, bindingHash,
config child cell, upgrade child cell, `pending` pull-ledger).

Incoming opcodes: `StakeDeposit` (0x534b4b01), `StakeRequestUnbond` (…02),
`StakeWithdraw` (…03), `StakeKeeperWithdraw` (…04), `StakeSlash` (…05),
`StakeClaim` (…0a), `ProvideWalletAddress` (TEP-89 0x2c76b973),
`ReceiptTopUp` (…00), `StakeAnnounceUpgrade` (…07), `StakeApplyUpgrade` (…08),
`StakeCancelUpgrade` (…09); plus `onBouncedMessage` (mint rollback / payout →
pull-ledger). Getters: `get_vault_state`, `get_owner`, `get_code_version`,
`get_pending_upgrade`, `get_binding_hash`, `get_pending`,
`get_receipt_wallet_address`, `get_wallet_address` (TEP-89), `get_jetton_data`
(TEP-64/74), `is_eligible`.

### Positive (testnet-broadcast, `T`)

| # | Scenario | Op / getter | Preconditions | Expected state + getter readback | On-chain assertion |
|---|---|---|---|---|---|
| SV-1 | Deposit bonds TON + mints 1:1 transfer-locked receipt | `StakeDeposit` → mint `JettonInternalTransfer` | sender == owner; value ≥ amount + 0.05 storage | `staked += amount`, `totalSupply += amount`; receipt wallet `balance == amount` | `get_vault_state.staked`, `.totalSupply`; receipt `get_wallet_data.balance == staked`; trace shows `StakeDeposit → JettonInternalTransfer → StakeReceiptWallet` |
| SV-2 | Receipt minted via **standard** TEP-74 internal_transfer (indexer-recognized) | mint path | after SV-1 | receipt wallet exists, `is_transfer_locked == true` | `is_transfer_locked()` true; `get_status() & 1 == 1` |
| SV-3 | Duckton **TEP-64** on-chain metadata | `get_jetton_data` | vault deployed (current code) | `jettonContent` hashes to `buildReceiptJettonContent()`; name=Duckton symbol=DUCKTON decimals=9; `mintable==true`; `adminAddress==vault` | content-cell hash equality + `jettonWalletCode.hash()==build("StakeReceiptWallet").hash()` |
| SV-4 | **TEP-74** `get_wallet_data` shape | receipt `get_wallet_data` | after SV-1 | `{balance, owner, minter==vault, code}` | minter == vault; balance == staked |
| SV-5 | **TEP-89** wallet discovery | `get_wallet_address(owner)` + `ProvideWalletAddress` | vault deployed | getter returns the deterministic receipt-wallet addr; provide replies `TakeWalletAddress` | `get_wallet_address(owner) == get_receipt_wallet_address(owner)`; trace shows the reply |
| SV-6 | Eligibility boundary `staked == min_stake` | `is_eligible` / `get_vault_state.eligible` | free-tier (min_stake 0) or staked at the threshold, no unbonding | `eligible == true` | `is_eligible()` true; `get_vault_state.eligible` true |
| SV-7 | Free-tier vault (min_stake 0) eligible with zero stake | `is_eligible` | fresh vault, min_stake 0 | `eligible == true` even with `staked==0` | `is_eligible()` true |
| SV-8 | `request_unbond` moves staked → cooldown bucket | `StakeRequestUnbond` | sender == owner; `unbondingAt==0`; 0 < amount ≤ staked | `staked -= amount`; `unbondingAmount = amount`; `unbondingAt = now`; cooldown stake still slashable | `get_vault_state.unbondingAmount == amount`, `.staked` reduced, `.unbondingAt != 0` |
| SV-9 | Graduated slash reduces stake, burns receipt, splits funds | `StakeSlash` | sender == cfg.slasher; 0 < amount ≤ staked+unbonding; split sums 10000 | `staked`/`totalSupply` reduced; receipt burned; challenger/redundancy/treasury paid pro-rata, burn share locked | `get_vault_state.staked` reduced by `min(amount,staked)`; trace shows split payouts; slashed-sum conservation |
| SV-10 | `ReceiptTopUp` accepted (storage rent) | `ReceiptTopUp` | any sender | balance only (no state field change); tx succeeds, no bounce | trace success, no abort |
| SV-11 | Pull-ledger `claim` re-delivers a bounced payout | `StakeClaim` | a `pending[recipient] > 0` exists (e.g. a slash payout to an uninit recipient bounced) | `pending[recipient]` cleared; funds re-sent to recipient | `get_pending(recipient) > 0` before, `== 0` after; trace shows payout |

> SV-11 is the bounce-safety happy path (C1): drive a slash with an
> uninitialized challenger so its share bounces into `pending`, then `StakeClaim`
> drains it — a positive, broadcastable proof of the recoverable push-then-pull
> design.

### Negative (emulator-only, `E`) — exit code asserted in `acton test`

| Scenario | Op | Exit code | Emulator test (`tests/`) |
|---|---|---|---|
| Deposit from non-owner | `StakeDeposit` | `NOT_OWNER` (132) | stake.test: "deposit from a non-owner is rejected" |
| Deposit under gas floor | `StakeDeposit` | `NOT_ENOUGH_GAS` (137) | stake.test: "deposit gas floor: under-funded rejected, exact floor accepted" |
| Receipt **transfer** attempted (anti-exit) | `JettonTransfer` | `RECEIPT_LOCKED` (138) | stake.test: "stake-receipt jetton transfer is locked (anti-exit)"; receipt_wallet.test |
| Direct mint from non-master | `JettonInternalTransfer` | `NOT_FROM_MASTER` (133) | receipt_wallet.test: "direct mint … from a non-master is rejected" |
| Direct burn from non-master | `ReceiptBurn` | `NOT_FROM_MASTER` (133) | receipt_wallet.test: "direct burn from a non-master is rejected" |
| Burn over balance | `ReceiptBurn` | `BALANCE_NEGATIVE` (136) | receipt_wallet.test: "burn over balance rejected; … zero accepted (boundary)" |
| Unbond guards (non-owner / zero / over-staked / double) | `StakeRequestUnbond` | `NOT_OWNER` (132) / `NOTHING_TO_UNBOND` (201) / `UNBOND_IN_PROGRESS` (202) | stake.test: "unbond guards: non-owner, zero, over-staked, and double unbond" |
| Withdraw guards (non-owner / nothing-unbonding) | `StakeWithdraw` | `NOT_OWNER` (132) / `NO_UNBONDING` (204) | stake.test: "withdraw guards: non-owner and nothing-unbonding rejected" |
| Keeper-withdraw nothing-unbonding | `StakeKeeperWithdraw` | `NO_UNBONDING` (204) | stake.test: "keeper withdraw with nothing unbonding is rejected" |
| Slash guards (zero / over-slashable) | `StakeSlash` | `SLASH_EXCEEDS_STAKE` (206) | stake.test: "slash guards: zero, over-slashable rejected; full slash accepted" |
| Slash from non-slasher | `StakeSlash` | `NOT_SLASHER` (200) | stake.test: "slash from non-slasher is rejected" |
| Config split ≠ 100% (first use) | any (validateVaultConfig) | `INVALID_SPLIT` (207) | stake.test: "config with a split that does not sum to 100% is rejected at first use" |
| Config keeperBountyBps > 100% | any (validateVaultConfig) | `BAD_BOUNTY_BPS` (284) | stake.test: "config with keeperBountyBps over 100% is rejected" |
| Unknown opcode (non-empty body) | else-branch | `INVALID_OP` (0xffff) | stake.test / receipt_wallet.test: "unknown opcode … INVALID_OP" |
| Upgrade announce/apply from non-authority | `StakeAnnounceUpgrade`/`StakeApplyUpgrade` | `UPGRADE_NOT_AUTHORITY` (283) | upgrade.test: "… from non-governance is rejected" |
| Apply with nothing pending / code mismatch | `StakeApplyUpgrade` | `NO_PENDING_UPGRADE` (208) / `UPGRADE_CODE_MISMATCH` (210) | upgrade.test: "apply guards: nothing-pending, code-mismatch …; cancel clears" |

### Time-gated positive (`G`) — proven via `testing.setNow`

| Scenario | Op | Time dependency | Emulator test |
|---|---|---|---|
| Owner **withdraw** after the unbonding cooldown | `StakeWithdraw` | `now ≥ unbondingAt + unbondingPeriod` (≥ challenge window; default 7 days). Early → `COOLDOWN_NOT_ELAPSED` (203) | stake.test: "unbonding cooldown blocks early withdraw and stays slashable"; upgrade.test v2 withdraw |
| **Keeper** withdraw after cooldown + grace (bounty math) | `StakeKeeperWithdraw` | `now ≥ unbondingAt + unbondingPeriod + keeperGrace`. Early → `KEEPER_TOO_EARLY` (205); bounty = `amount·keeperBountyBps/10000` | stake.test: "keeper can finalize an unresponsive owner withdraw for a bounty"; "keeper bounty math"; upgrade.test v2 keeper |
| **Timelocked SETCODE** apply → V2 (`get_slasher` live) | `StakeAnnounceUpgrade` → wait → `StakeApplyUpgrade` | apply only after `pendingAt + unbondingPeriod` (the staker exit window). Early → `UPGRADE_TIMELOCK_ACTIVE` (209) | upgrade.test: "stake vault timelocked upgrade: announce, apply-before-delay rejected, apply-after-delay succeeds (address stable, bonds preserved, new getter live)" |

> **Why StakeVault upgrade is `G`, not `T`:** the timelock delay equals the
> unbonding period (≥ the challenge window) *by design* — it is the staker exit
> window that preserves the non-custodial guarantee. Broadcasting a real apply
> would require waiting ≥ 7 days. The emulator warps time; the live runner does
> **not** degrade the delay to 0 (that would defeat the very property under test).

---

## 2. StakeReceiptWallet — 1:1 transfer-locked stake-receipt jetton (TEP-74)

`contracts/StakeReceiptWallet.tolk`. Incoming: `JettonInternalTransfer` (mint,
master-only), `JettonTransfer` (ALWAYS rejected), `ReceiptBurn` (master-only),
`ReceiptTopUp`. Getters: `get_wallet_data`, `get_status`, `get_owner`,
`is_transfer_locked`.

Its positive paths are driven **through the master vault** (mint on SV-1, burn
on SV-9/withdraw), so they are covered as `T` under §1 (SV-1..SV-4). Its
direct-message guards are all negatives (`E`):

| Scenario | Class | Op | Exit code | Where |
|---|---|---|---|---|
| Mint emits transfer-notification when forward amount > 0 | `T`/emulator | `JettonInternalTransfer` (fwd>0) | — | stake.test "mint with a forward amount emits a transfer notification" (vault mint uses fwd=0, so this exact path is emulator) |
| Transfer rejected (anti-exit) | `E` | `JettonTransfer` | `RECEIPT_LOCKED` (138) | receipt_wallet / stake.test |
| Direct mint from non-master | `E` | `JettonInternalTransfer` | `NOT_FROM_MASTER` (133) | receipt_wallet.test |
| Direct burn from non-master | `E` | `ReceiptBurn` | `NOT_FROM_MASTER` (133) | receipt_wallet.test |
| Burn over balance | `E` | `ReceiptBurn` | `BALANCE_NEGATIVE` (136) | receipt_wallet.test |
| Unknown opcode | `E` | else | `INVALID_OP` | receipt_wallet.test |
| `get_wallet_data` / `get_status` / top-up read paths | `T` | getters | — | covered live on SV-2/SV-4 |

---

## 3. JobEscrow — per-job non-custodial HTLC escrow

`contracts/JobEscrow.tolk`, `contracts/escrow_types.tolk`. Storage:
`EscrowStorage` (requester, arbiter, escrowAmount B, deadline, settled, terms
child cell {treasury, expectedHash, candidatesHash, paramsVersion}, `pending`
pull-ledger). Incoming: `EscrowSettle` (0x45534302), `EscrowRefund` (…03),
`EscrowTopUp` (…00), `EscrowClaim` (…04); `onBouncedMessage` → pull-ledger.
Getters: `get_escrow_state`, `get_params_version`, `get_requester`,
`get_expected_hash`, `get_candidates_hash`, `get_pending`.

### Positive (`T`)

| # | Scenario | Op / getter | Preconditions | Expected state + getter readback | On-chain assertion |
|---|---|---|---|---|---|
| JE-1 | **HTLC settle** releases on the agreed quorum hash | `EscrowSettle` | sender == arbiter; `!settled`; `resultHash == terms.expectedHash`; `candidatesCommitment(candidates) == terms.candidatesHash`; `winner != arbiter`; winner & each participant ∈ candidates; `winnerAmount + fee + Σparticipants ≤ B` | `settled = true`; winner paid base+bonus, treasury paid fee, each agreeing non-winner paid its commission, remainder refunded to requester | `get_escrow_state.settled == true`; trace shows `EscrowSettle → EscrowPayout×N + refund`; winner balance ↑ or `get_pending(winner) > 0` |
| JE-2 | Payout split bounded by escrowed **B** (boundary `total == B` accepted) | `EscrowSettle` | total payouts exactly == B | settled; full B distributed, ~0 refund | `get_escrow_state.settled`; fuzz/boundary emulator pins arithmetic |
| JE-3 | `EscrowTopUp` accepted | `EscrowTopUp` | any | balance only; success | trace success |
| JE-4 | Pull-ledger `claim` re-delivers a bounced payout | `EscrowClaim` | `pending[recipient] > 0` (a winner/participant payout bounced) | `pending[recipient]` cleared, re-sent | `get_pending(recipient) > 0` → `0`; trace shows payout |
| JE-5 | `params_version` bound into terms | `get_params_version` | escrow opened under a GlobalParams version | returns the pinned version | `get_params_version()` == the bound version |

### Negative (`E`)

| Scenario | Op | Exit code | Emulator test (`tests/escrow.test.tolk`) |
|---|---|---|---|
| Settle with wrong result hash | `EscrowSettle` | `HASH_MISMATCH` (223) | "settle with wrong result hash is rejected (HTLC lock)" |
| Settle from non-arbiter | `EscrowSettle` | `NOT_ARBITER` (222) | "settle from non-arbiter is rejected" |
| Arbiter == winner / redirect to non-candidate | `EscrowSettle` | `NOT_OWNER_SIG` (280) / `SETTLE_POLICY_MISMATCH` (281) | "arbiter cannot redirect payout to itself or a non-candidate (B1)" |
| Non-candidate participant | `EscrowSettle` | `SETTLE_POLICY_MISMATCH` (281) | "settle with a non-candidate participant is rejected (B1)" |
| Candidate set hash mismatch | `EscrowSettle` | `SETTLE_POLICY_MISMATCH` (281) | (B1; the gap fixed in this work — see runner notes) |
| Payout exceeds B / total over B | `EscrowSettle` | `PAYOUT_EXCEEDS_ESCROW` (226) | "payout exceeding escrow is rejected"; "payout total over escrow … is rejected" |
| Too many participants (> 32) | `EscrowSettle` | `TOO_MANY_PARTICIPANTS` (282) | (B4 bound; emulator) |
| Re-settle / refund after settle | `EscrowSettle`/`EscrowRefund` | `ALREADY_SETTLED` (224) | "refund after a successful settle is rejected (already settled)" |
| Unknown opcode | else | `INVALID_OP` | "escrow rejects an unknown opcode … INVALID_OP" |

### Time-gated positive (`G`)

| Scenario | Op | Time dependency | Emulator test |
|---|---|---|---|
| **Refund-on-timeout** to the requester | `EscrowRefund` | `now ≥ deadline` (permissionless after deadline). Early → `TIMEOUT_NOT_REACHED` (225) | "refund before deadline is rejected, after deadline succeeds"; "settle refunds the unspent slack to the requester" |

### Dead / unreachable error codes (reserved ABI slots, never thrown)

- `ESCROW_NOT_FUNDED` (220): the escrow is funded with **B** at deploy; there is
  no runtime path that throws it.
- `NOT_REQUESTER` (221): settle is arbiter-gated (222) and refund is
  permissionless-after-deadline (funds always go to the requester), so no path
  throws "not requester".

These are kept (never renumbered) so the frozen on-chain ABI stays stable.

---

## 4. RecordAnchor (+ RecordAnchorV2) — per-epoch Merkle anchor + dispute

`contracts/RecordAnchor.tolk`, `contracts/anchor_types.tolk`. Storage:
`AnchorStorage` (currentEpoch, lastRoot, nextDisputeId, `disputes`, `roots`,
config child cell, codeVersion). Incoming: `AnchorSubmitRoot` (0x414e4301),
`AnchorOpenDispute` (…02), `AnchorResolveDispute` (…03), `AnchorUpgradeCode`
(…04); `onBouncedMessage` re-opens a dispute on a bounced bond refund. Getters:
`get_anchor_state`, `get_code_version`, `get_dispute`, `verify_inclusion`,
`compute_root`. Merkle: leaf = `hashLeaf(x) = sha256(00 40 ‖ x)`, node =
`hashPair(l,r) = sha256(00 80 ‖ l ‖ r)`; single-leaf root == `hashLeaf(leaf)`.

### Positive (`T`)

| # | Scenario | Op / getter | Preconditions | Expected state + getter readback | On-chain assertion |
|---|---|---|---|---|---|
| RA-1 | Submit chained, stake-weighted root | `AnchorSubmitRoot` | sender == keeper; root ≠ 0; `epoch == currentEpoch+1`; `prevRoot == lastRoot`; `stakeWeight ≥ minStakeWeight` | `currentEpoch = epoch`; `lastRoot = root`; `roots[epoch] = root` | `get_anchor_state.currentEpoch`/`.lastRoot` advanced |
| RA-2 | Single-leaf inclusion (`root == hashLeaf(leaf)`, null proof) | `verify_inclusion` | after RA-1 with `root = hashLeaf(leaf)` | `verify_inclusion(leaf, root, null) == true` | getter true |
| RA-3 | Multi-leaf (8-leaf, 3-level) inclusion + tamper reject | `AnchorSubmitRoot` + `verify_inclusion` | submit `tree8Root()` | interior leaf M5 proof folds to root; tampered leaf does **not** | `verify_inclusion(M5,…)==true` AND `verify_inclusion(M5+1,…)==false` |
| RA-4 | Dispute open + resolve **uphold** | `AnchorOpenDispute` + `AnchorResolveDispute(upheld=true)` | bond ≥ disputeBondMin; epoch anchored; proof folds to stored root; resolver == verdictAuthority; dispute open | dispute `status = 1`; bond returned to challenger | `get_dispute(id).status == 1`; trace shows bond refund |
| RA-5 | Dispute open + resolve **reject** (frivolous) | `AnchorResolveDispute(upheld=false)` | as RA-4 | dispute `status = 2`; bond forfeited to treasury | `get_dispute(id).status == 2`; trace shows treasury refund |
| RA-6 | **Authority-gated in-place SETCODE** → V2 (`get_verdict_authority` live) | `AnchorUpgradeCode` | sender == cfg.upgradeAuthority (immediate; **no** timelock) | code swapped at SAME address; `codeVersion += 1`; epoch chain preserved | `get_code_version` 0→1; address stable; `get_verdict_authority()` (v2-only) resolves; `get_anchor_state` preserved |

> **RA-6 is `T` (not `G`):** RecordAnchor's `upgrade_code` is authority-gated but
> **not** timelocked, so a single broadcast swaps the code and the v2-only getter
> goes live immediately at the unchanged address — the concrete "new code live
> over preserved state" proof.

### Negative (`E`) — `tests/anchor.test.tolk`

| Scenario | Op | Exit code |
|---|---|---|
| Submit from non-keeper | `AnchorSubmitRoot` | `NOT_ANCHOR_KEEPER` (250) |
| All-zero root | `AnchorSubmitRoot` | `ZERO_ROOT` (251) |
| Bad prev_root | `AnchorSubmitRoot` | `BAD_PREV_ROOT` (240) |
| Epoch regression / non-monotonic | `AnchorSubmitRoot` | `EPOCH_REGRESSION` (241) |
| Stake-weight below threshold | `AnchorSubmitRoot` | `INSUFFICIENT_ANCHOR_STAKE` (242) |
| Dispute bond below minimum | `AnchorOpenDispute` | `DISPUTE_BOND_TOO_LOW` (244) |
| Dispute against unknown epoch | `AnchorOpenDispute` | `UNKNOWN_EPOCH` (248) |
| Forged-root dispute (proof folds to wrong root) | `AnchorOpenDispute` | `BAD_INCLUSION_PROOF` (243) |
| Proof exceeds max depth (32) | `AnchorOpenDispute` | `PROOF_TOO_DEEP` (249) |
| Resolve unknown dispute id | `AnchorResolveDispute` | `NO_SUCH_DISPUTE` (245) |
| Resolve from non-verdict-authority | `AnchorResolveDispute` | `NOT_VERDICT_AUTHORITY` (246) |
| Double-resolve a dispute | `AnchorResolveDispute` | `DISPUTE_NOT_OPEN` (247) |
| Config sub-floor dispute bond (first use) | validateAnchorConfig | `BOND_FLOOR_TOO_LOW` (252) |
| Upgrade from non-authority | `AnchorUpgradeCode` | `UPGRADE_NOT_AUTHORITY` (283) |
| Unknown opcode | else | `INVALID_OP` |

Boundary positives (bond exactly at min; reference Merkle hash vectors a–d for
Rust pinning) are in `anchor.test.tolk` and `fuzz.test.tolk`.

---

## 5. GlobalParams (+ GlobalParamsV2) — platform-wide economic config (singleton)

`contracts/GlobalParams.tolk`, `contracts/global_params_types.tolk`. Storage:
`GlobalParamsStorage` (admin, feeRecipient, params child cell, `blocklist`,
paramsVersion, codeVersion, upgradeDelay, pendingCodeHash/At). Incoming:
`UpdateParams` (0x47504101), `UpdateAdmin` (…02), `SetBlocklisted` (…03),
`UpgradeCode` (…04, **timelocked** apply), `AnnounceCode` (…05), `CancelCode`
(…06). Getters: `get_params`, `get_params_version`, `get_code_version`,
`get_pending_upgrade`, scalar getters (`get_platform_fee_bps`, …,
`get_progress_stall_mult`), `get_admin`, `get_fee_recipient`, `get_blocklisted`.

### Positive (`T`)

| # | Scenario | Op / getter | Preconditions | Expected state + getter readback | On-chain assertion |
|---|---|---|---|---|---|
| GP-1 | `update_params` mutates in place, address stable, **paramsVersion monotonic** | `UpdateParams` | sender == admin; params pass `validateEcoParams` | `feeRecipient`/`params` replaced; `paramsVersion += 1`; emits `ParamsUpdated`; address unchanged | `get_params_version` += 1; `get_params.params…platformFeeBps` == new; same address |
| GP-2 | Params **exactly on every boundary** accepted | `UpdateParams` | params sitting on each invariant edge | accepted; version bumped | `get_params_version` advanced |
| GP-3 | Governance **blocklist** set / clear round-trip | `SetBlocklisted` | sender == admin | `blocklist[subject]` 1 then 0 | `get_blocklisted(subject)` true → false |
| GP-4 | **Admin rotation** (multisig handoff) | `UpdateAdmin` | sender == admin | `admin = newAdmin` | `get_admin()` == newAdmin |
| GP-5 | Scalar policy getters read back stored params | scalar getters | deployed | each getter returns its stored field | `get_platform_fee_bps` etc. equal stored |
| GP-6 | Announce → **cancel** a code upgrade | `AnnounceCode` + `CancelCode` | sender == admin | pending set, then cleared | `get_pending_upgrade.pendingCodeHash` set → 0 |
| GP-7 | **Timelocked SETCODE** apply → V2 (`get_surcharge_bps` live) | `AnnounceCode` → wait `upgradeDelay` → `UpgradeCode` | sender == admin; applied code hash == announced; `now ≥ pendingAt + upgradeDelay`; applied code hash-committed | code swapped at SAME address; `codeVersion += 1`; params/admin/version preserved; emits `CodeUpgraded` | `get_code_version` 0→1; address stable; `get_surcharge_bps()` (v2-only) resolves; state preserved |

> **GP-7 classification.** The production singleton uses a 24h `upgradeDelay`, so
> a real apply on the live singleton is `G`. The live runner proves GP-7 as `T`
> on a **dedicated, throwaway** GlobalParams instance deployed with
> `GP_UPGRADE_DELAY_SECS=0`, which lets announce + apply run in one session
> (`readyAt == pendingAt`). This proves the SETCODE / address-stability / v2-getter
> mechanics on-chain **without** weakening the production singleton's timelock.
> The timelock guard itself (apply-before-delay → `UPGRADE_TIMELOCK_ACTIVE`) is an
> `E`/`G` emulator assertion (`tests/upgrade.test.tolk`).

### Negative (`E`) — `tests/global_params.test.tolk`, `tests/upgrade.test.tolk`

| Scenario | Op | Exit code |
|---|---|---|
| Non-admin `update_params` | `UpdateParams` | `NOT_ADMIN` (260) |
| Out-of-bounds params (every §12 invariant) | `UpdateParams` | `PARAMS_OUT_OF_BOUNDS` (261) |
| Non-admin `update_admin` / `set_blocklisted` / `announce` / `apply` | resp. | `NOT_ADMIN` (260) |
| Apply with nothing pending | `UpgradeCode` | `NO_PENDING_UPGRADE` (208) |
| Apply wrong (non-announced) code | `UpgradeCode` | `UPGRADE_CODE_MISMATCH` (210) |
| Apply before timelock elapses | `UpgradeCode` | `UPGRADE_TIMELOCK_ACTIVE` (209) |
| Unknown opcode | else | `INVALID_OP` |

The §12 invariants enforced on-chain by `validateEcoParams` (all bps ≤ 10000;
κ ≤ 1000; progress interval/mult ≥ 1; slash split == 100%; min-stake
monotonicity + cap ≥ min; unbonding ≥ challenge window; selection ordering
`nMax ≥ nDefault ≥ nPublic ≥ checksumMin ≥ 1`; `1 ≤ quorum ≤ nDefault`) are each
covered by "every economic-parameter invariant is enforced on-chain".

---

## 6. V2 contracts (StakeVaultV2 / RecordAnchorV2 / GlobalParamsV2)

Each V2 is a **faithful successor**: SAME storage layout and SAME handlers as v1
(so **every v1 scenario above re-applies verbatim** after the swap), plus ONE
new getter v1 lacked. The V2-specific, broadcastable value is the **post-SETCODE
getter resolving at the unchanged address over preserved state**:

| Target | Reached via | New getter (proof of live v2 code) | Class |
|---|---|---|---|
| `RecordAnchorV2` | RA-6 `AnchorUpgradeCode` (immediate, authority-gated) | `get_verdict_authority()` | `T` |
| `GlobalParamsV2` | GP-7 `UpgradeCode` (announce+apply, delay-0 test instance) | `get_surcharge_bps()` | `T` |
| `StakeVaultV2` | timelocked `StakeApplyUpgrade` (delay == unbonding window) | `get_slasher()` | `G` (emulator; would need ≥ 7-day wait live) |

Post-upgrade, the v2 handlers are re-exercised in `tests/upgrade.test.tolk`
(deposit/slash/keeper/claim/provide on v2 vault; submit/dispute on v2 anchor;
update/blocklist/admin/announce-cancel on v2 GlobalParams) to prove the swapped
code still drives the full flow over preserved storage.

---

## 7. How to run

```bash
export PATH="$HOME/.acton/bin:$PATH"

# (a) The 14-check turnkey live flow (StakeVault/RecordAnchor/JobEscrow/GlobalParams):
TON_TESTNET_MNEMONIC="…" ALLOW_NO_API_KEY=1 scripts/testnet_e2e.sh

# (b) The comprehensive on-chain runner (all testnet-broadcastable scenarios,
#     real broadcast→trace→getter assertions) + the emulator suite:
ALLOW_NO_API_KEY=1 scripts/scenarios_testnet.sh

# (c) Emulator suite alone (all negatives + time-gated positives):
cd ton && acton test
```

A `TON_TESTNET_API_KEY` (Toncenter testnet key from @tonapibot, exported as
`TONCENTER_TESTNET_API_KEY`) is optional but **strongly speeds up** live runs:
keyless Toncenter self-throttles to ~1 RPS, so the many-broadcast comprehensive
run is slow (but completes). The testnet faucet allows only **2 airdrops / 24h /
IP**, so keep run amounts small (the runners default to tiny nanoton values).

**Testnet only. No mainnet. No git commits.**
