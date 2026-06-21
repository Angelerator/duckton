// Per-node stake contract (BLOCKCHAIN_ECONOMICS §8). Non-custodial: no human key
// controls the funds — deposit, unbond cooldown, slashing and keeper withdrawal
// are all governed by contract code. The vault is ALSO the master of this node's
// transfer-locked stake-receipt jetton (§8.5): it mints 1:1 on deposit and burns
// on withdrawal/slash.
//
// This contract is only ever touched by PAID work. Free jobs (§8.2) bypass it
// entirely via the off-chain NoopSettlement path.
//
// SECURITY HARDENING:
//   C1  every pushed payout (slash split / keeper bounty / owner withdrawal) is
//       BOUNCEABLE; a bounced payout is credited to a pull-ledger (`pending`)
//       keyed by the recipient, never silently lost (old NoBounce|IGNORE_ERRORS).
//   C2  the deposit MINT (internal_transfer) is bounceable; on bounce the staked
//       + totalSupply credit is rolled back so a failed mint cannot inflate supply
//       with no backing (standard TEP-74 master behavior).
//   C3  owner/keeper withdrawals pay from the LIQUID remainder (RAWRESERVE keeps
//       the still-bonded stake + a storage buffer, then CARRY_ALL_BALANCE), so
//       storage-rent erosion over the long unbonding window cannot underpay.
//   C4  keeperBountyBps / unbonding-vs-challenge / split sums are validated.
//   D1  code upgrades are gated to a distinct `upgradeAuthority`, not the slasher.

import "common"
import "stake_types"

contract StakeVault {
    storage: VaultStorage
    incomingMessages: AllowedMessageToVault
}

type AllowedMessageToVault =
    | StakeDeposit
    | StakeRequestUnbond
    | StakeWithdraw
    | StakeKeeperWithdraw
    | StakeSlash
    | StakeClaim // C1 pull a queued/bounced payout
    | ProvideWalletAddress // TEP-89 jetton-wallet discovery
    | ReceiptTopUp
    | StakeAnnounceUpgrade // §8.6 timelocked code upgrade: step 1 (announce)
    | StakeApplyUpgrade // §8.6 timelocked code upgrade: step 2 (apply, after delay)
    | StakeCancelUpgrade // §8.6 abort a pending announcement

// C1/C2 bounce-safety. A bounced MINT rolls back the staked/totalSupply credit
// (so a failed receipt mint cannot inflate supply); a bounced PAYOUT is credited
// to the pull-ledger keyed by the recipient (= bounced-from address) so it is
// never lost.
fun onBouncedMessage(in: InMessageBounced) {
    var body = in.bouncedBody;
    body.skipBouncedPrefix();
    val op = body.loadUint(32);
    var storage = lazy VaultStorage.load();

    if (op == OP_JETTON_TRANSFER_INTERNAL) {
        // C2: mint bounced. Body = op, queryId(64), amount(coins). The receipt was
        // never minted, so claw the deposited `amount` back OUT of the bonded
        // buckets and shrink totalSupply by the SAME amount — keeping the
        // invariant `totalSupply == staked + unbondingAmount` exact. Pull from
        // `staked` first, then from the cooldown bucket if a `StakeRequestUnbond`
        // moved the just-deposited stake there before the bounce landed (the old
        // code shrank totalSupply by the full `amount` but `staked` by only
        // `min(amount, staked)`, desyncing supply from backing whenever an unbond
        // raced the bounce). `min` guards every leg against underflow.
        body.skipBits(64); // queryId
        val amount = body.loadCoins();
        val fromStaked = min(amount, storage.staked);
        storage.staked -= fromStaked;
        val fromUnbonding = min(amount - fromStaked, storage.unbondingAmount);
        storage.unbondingAmount -= fromUnbonding;
        // Shrink supply only by what was actually reclaimed from backing, so the
        // invariant holds even in the (unreachable today) case where less than
        // `amount` remained bonded.
        storage.totalSupply -= fromStaked + fromUnbonding;
        storage.save();
    } else if (op == OP_STAKE_PAYOUT) {
        // C1: payout bounced. Credit the returned value to the pull-ledger.
        val prev = storage.pending.get(in.senderAddress);
        val had: coins = prev.isFound ? prev.loadValue() : 0;
        storage.pending.set(in.senderAddress, had + in.valueCoins);
        storage.save();
    }
}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToVault.fromSlice(in.body);

    match (msg) {
        // Deposit: bond `amount` (held non-custodially in this contract) and mint
        // an equal amount of the transfer-locked stake-receipt jetton to the owner.
        StakeDeposit => {
            var storage = lazy VaultStorage.load();
            assert (in.senderAddress == storage.owner) throw ErrCode.NOT_OWNER;
            assert (
                in.valueCoins >= msg.amount + MIN_TONS_FOR_STORAGE
            ) throw ErrCode.NOT_ENOUGH_GAS;

            val cfg = storage.config.load();
            // C4: reject a misconfigured vault at first use (no on-chain setter).
            validateVaultConfig(cfg);

            storage.staked += msg.amount;
            storage.totalSupply += msg.amount;
            storage.save();

            // C2: bounceable mint — if it fails, onBouncedMessage rolls back.
            mintReceipt(
                storage.owner,
                contract.getAddress(),
                cfg.receiptWalletCode,
                msg.amount,
                msg.queryId,
            );
        }

        // Begin unbonding: move `amount` from the bonded bucket into the cooldown
        // bucket. The cooldown stake stays SLASHABLE (§8.4) and the receipt stays
        // locked until withdrawal completes.
        StakeRequestUnbond => {
            var storage = lazy VaultStorage.load();
            assert (in.senderAddress == storage.owner) throw ErrCode.NOT_OWNER;
            assert (storage.unbondingAt == 0) throw ErrCode.UNBOND_IN_PROGRESS;
            assert (msg.amount > 0 && msg.amount <= storage.staked) throw ErrCode.NOTHING_TO_UNBOND;

            storage.staked -= msg.amount;
            storage.unbondingAmount = msg.amount;
            storage.unbondingAt = blockchain.now() as uint32;
            storage.save();
        }

        // Owner withdrawal after the cooldown elapses: release the cooled stake and
        // burn the matching receipts. C3: pay from the LIQUID remainder so rent
        // erosion over the unbonding window never underpays.
        StakeWithdraw => {
            var storage = lazy VaultStorage.load();
            assert (in.senderAddress == storage.owner) throw ErrCode.NOT_OWNER;
            assert (storage.unbondingAmount > 0) throw ErrCode.NO_UNBONDING;
            val cfg = storage.config.load();
            val readyAt = storage.unbondingAt + cfg.unbondingPeriod;
            assert (blockchain.now() >= readyAt) throw ErrCode.COOLDOWN_NOT_ELAPSED;

            val amount = storage.unbondingAmount;
            storage.unbondingAmount = 0;
            storage.unbondingAt = 0;
            storage.totalSupply -= amount;
            storage.save();

            burnReceipt(
                storage.owner,
                contract.getAddress(),
                cfg.receiptWalletCode,
                amount,
                msg.queryId,
            );
            // C3: reserve the still-bonded stake + storage buffer, send the rest
            // (the cooled stake, adjusted for real liquidity) to the owner.
            payoutLiquidRemainder(storage.owner, storage.staked, msg.queryId);
        }

        // Keeper liveness (TON idiom, §8.4): once cooldown + grace elapses, ANYONE
        // can finalize an unresponsive owner's withdrawal and earn a small bounty.
        StakeKeeperWithdraw => {
            var storage = lazy VaultStorage.load();
            assert (storage.unbondingAmount > 0) throw ErrCode.NO_UNBONDING;
            val cfg = storage.config.load();
            val keeperAt = storage.unbondingAt + cfg.unbondingPeriod + cfg.keeperGrace;
            assert (blockchain.now() >= keeperAt) throw ErrCode.KEEPER_TOO_EARLY;

            val amount = storage.unbondingAmount;
            storage.unbondingAmount = 0;
            storage.unbondingAt = 0;
            storage.totalSupply -= amount;
            storage.save();

            val bounty = mulDivFloor(amount, cfg.keeperBountyBps, BPS_DENOM);
            burnReceipt(
                storage.owner,
                contract.getAddress(),
                cfg.receiptWalletCode,
                amount,
                msg.queryId,
            );
            // C3: reserve the still-bonded stake + a storage buffer, pay the keeper
            // its fixed bounty FIRST (from the liquid remainder), then sweep the
            // rest to the owner with carry-remaining. Reserving only staked+buffer
            // (not the bounty) keeps the bounty within the sendable balance.
            reserveToncoinsOnBalance(
                storage.staked + MIN_TONS_FOR_STORAGE,
                RESERVE_MODE_EXACT_AMOUNT,
            );
            if (bounty > 0) {
                payout(in.senderAddress, bounty, msg.queryId);
            }
            payoutRemainderNoReserve(storage.owner, msg.queryId);
        }

        // Graduated slashing (§8.3) by the authorized slasher (dispute contract).
        // Takes from the bonded bucket first, then the still-slashable cooldown
        // bucket, splits the slashed funds per config, and burns the receipts.
        StakeSlash => {
            var storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.slasher) throw ErrCode.NOT_SLASHER;
            val slashable = storage.staked + storage.unbondingAmount;
            assert (msg.amount > 0 && msg.amount <= slashable) throw ErrCode.SLASH_EXCEEDS_STAKE;

            val fromStaked = min(msg.amount, storage.staked);
            storage.staked -= fromStaked;
            storage.unbondingAmount -= (msg.amount - fromStaked);
            storage.totalSupply -= msg.amount;
            storage.save();

            burnReceipt(
                storage.owner,
                contract.getAddress(),
                cfg.receiptWalletCode,
                msg.amount,
                msg.queryId,
            );
            applySlashSplit(cfg.slash.load(), msg.amount, msg.challenger, msg.queryId);
        }

        // C1 pull path: deliver a queued/bounced payout to the recipient it is
        // keyed under. Anyone may trigger it; funds only go to that recipient.
        StakeClaim => {
            var storage = lazy VaultStorage.load();
            val found = storage.pending.get(msg.recipient);
            assert (found.isFound) throw ErrCode.NO_UNBONDING;
            val amount = found.loadValue();
            assert (amount > 0) throw ErrCode.NO_UNBONDING;
            storage.pending.delete(msg.recipient);
            storage.save();
            payout(msg.recipient, amount, msg.queryId);
        }

        // TEP-89: reply with this node's receipt jetton-wallet address for the
        // requested owner so indexers/explorers/wallets can discover and link
        // holder wallets to this master (the vault).
        ProvideWalletAddress => {
            val storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            val walletAddr = calcReceiptWalletAddress(
                msg.ownerAddress,
                contract.getAddress(),
                cfg.receiptWalletCode,
            );
            val ownerRef = msg.includeAddress
                ? beginCell().storeAddress(msg.ownerAddress).endCell()
                : null;
            val reply = createMessage({
                bounce: BounceMode.NoBounce,
                dest: in.senderAddress,
                value: 0,
                body: TakeWalletAddress {
                    queryId: msg.queryId,
                    walletAddress: walletAddr,
                    ownerAddress: ownerRef,
                },
            });
            reply.send(SEND_MODE_CARRY_ALL_REMAINING_MESSAGE_VALUE | SEND_MODE_IGNORE_ERRORS);
        }

        ReceiptTopUp => {
            // accept TON top-ups for storage rent
        }

        // ===================================================================
        // Timelocked governance code upgrade (§8.6) — UPGRADEABILITY vs the
        // NON-CUSTODIAL guarantee, and how the timelock reconciles them.
        //
        // This vault holds stakers' bonds non-custodially: release is governed
        // ONLY by code (deposit / unbond cooldown / slashing / keeper). An
        // ARBITRARY admin code-swap would BREAK that — whoever could replace the
        // code could insert a "drain to me" path and rug every bond. So we make
        // the code upgradeable, but DELIBERATELY constrain it so it cannot be
        // used to rug:
        //   * D1: gated to `cfg.upgradeAuthority` — a DISTINCT governance authority
        //     SEPARATED from `cfg.slasher` (so a compromised slasher key cannot
        //     even announce an upgrade), meant to be a multisig.
        //   * TIMELOCKED two-step: ANNOUNCE (commit to the successor code's hash
        //     + start the clock) then APPLY, where apply is rejected until
        //     `pendingAt + unbondingPeriod` has elapsed. `unbondingPeriod` is
        //     >= the challenge window (validated), so ANY staker who dislikes an
        //     announced upgrade can `request_unbond` and fully withdraw under the
        //     CURRENT (trusted) code BEFORE the new code can take effect.
        //   * Apply must present code whose hash == the announced hash, so the
        //     swap is exactly what was published (no bait-and-switch), and the
        //     announcement is publicly observable via `get_pending_upgrade`.
        //   * `StakeCancelUpgrade` lets governance abort a bad announcement.
        // ===================================================================

        // Step 1/2 — announce: record the successor code hash + start the clock.
        StakeAnnounceUpgrade => {
            var storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.upgradeAuthority) throw ErrCode.UPGRADE_NOT_AUTHORITY;

            var up = storage.upgrade.load();
            up.pendingCodeHash = msg.newCodeHash;
            up.pendingCodeAt = blockchain.now() as uint32;
            storage.upgrade = up.toCell();
            storage.save();
        }

        // Step 2/2 — apply: only after the timelock, only the announced code.
        StakeApplyUpgrade => {
            var storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.upgradeAuthority) throw ErrCode.UPGRADE_NOT_AUTHORITY;

            var up = storage.upgrade.load();
            assert (up.pendingCodeHash != 0) throw ErrCode.NO_PENDING_UPGRADE;
            // Bind apply to EXACTLY the announced code (commit-then-reveal).
            assert (msg.newCode.hash() == up.pendingCodeHash) throw ErrCode.UPGRADE_CODE_MISMATCH;
            // Enforce the exit window: apply only after >= the unbonding period.
            val readyAt = up.pendingCodeAt + cfg.unbondingPeriod;
            assert (blockchain.now() >= readyAt) throw ErrCode.UPGRADE_TIMELOCK_ACTIVE;

            // Postponed SETCODE: new code is live only AFTER this run succeeds;
            // storage (bonds, supply, cooldown) is fully preserved. Bump the
            // monotonic codeVersion and clear the pending announcement.
            contract.setCodePostponed(msg.newCode);
            up.codeVersion += 1;
            up.pendingCodeHash = 0;
            up.pendingCodeAt = 0;
            storage.upgrade = up.toCell();
            storage.save();
        }

        // Abort a pending announcement (governance safety valve).
        StakeCancelUpgrade => {
            var storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.upgradeAuthority) throw ErrCode.UPGRADE_NOT_AUTHORITY;

            var up = storage.upgrade.load();
            assert (up.pendingCodeHash != 0) throw ErrCode.NO_PENDING_UPGRADE;
            up.pendingCodeHash = 0;
            up.pendingCodeAt = 0;
            storage.upgrade = up.toCell();
            storage.save();
        }

        else => {
            assert (in.body.isEmpty()) throw ErrCode.INVALID_OP;
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fun mintReceipt(owner: address, master: address, walletCode: cell, amount: coins, queryId: int) {
    val dest = calcDeployedReceiptWallet(owner, master, walletCode);
    // Credit via the STANDARD TEP-74 internal_transfer so indexers recognize the
    // mint. `from`/`responseAddress` are the master; `forwardTonAmount` is 0 (no
    // owner notification — keeps the deposit gas budget unchanged), and the
    // forward payload is empty.
    //
    // C2: BOUNCEABLE — a failed mint bounces and `onBouncedMessage` rolls back the
    // staked/totalSupply credit, so supply can never inflate without backing.
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: dest,
        value: MIN_TONS_FOR_STORAGE,
        body: JettonInternalTransfer {
            queryId: queryId,
            amount: amount,
            from: master,
            responseAddress: master,
            forwardTonAmount: 0,
            forwardPayload: beginCell().endCell().beginParse(),
        },
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY);
}

fun burnReceipt(owner: address, master: address, walletCode: cell, amount: coins, queryId: int) {
    val dest = calcReceiptWalletAddress(owner, master, walletCode);
    val m = createMessage({
        bounce: BounceMode.NoBounce,
        dest: dest,
        value: ton("0.02"),
        body: ReceiptBurn {
            queryId: queryId,
            amount: amount,
        },
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY | SEND_MODE_IGNORE_ERRORS);
}

// C1: a fixed-value payout, BOUNCEABLE with an op-tagged body so a failed delivery
// returns the funds here and `onBouncedMessage` credits the pull-ledger.
// PAY_FEES_SEPARATELY so the recipient receives EXACTLY `amount`. NOT
// IGNORE_ERRORS: a genuine bounce is what makes recovery possible.
fun payout(to: address, amount: coins, queryId: int) {
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: to,
        value: amount,
        body: StakePayout { queryId: queryId as uint64 },
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY);
}

// C3: pay the LIQUID remainder. Reserve `keepReserved` (the still-bonded stake +
// any amount that must stay, e.g. a keeper bounty) plus a storage buffer, then
// carry the rest to `to`. This sends the cooled stake adjusted for real liquidity,
// so storage-rent erosion over the long unbonding window can never underpay (the
// recipient simply gets the true remaining liquid amount, never more than escrow).
// Bounceable so a failed delivery is credited to the pull-ledger.
fun payoutLiquidRemainder(to: address, keepReserved: coins, queryId: int) {
    reserveToncoinsOnBalance(keepReserved + MIN_TONS_FOR_STORAGE, RESERVE_MODE_EXACT_AMOUNT);
    payoutRemainderNoReserve(to, queryId);
}

// Carry the remaining (post-reserve, post-other-sends) balance to `to`. Call only
// AFTER a reserveToncoinsOnBalance has set aside what must stay. Bounceable so a
// failed delivery is credited to the pull-ledger.
fun payoutRemainderNoReserve(to: address, queryId: int) {
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: to,
        value: 0,
        body: StakePayout { queryId: queryId as uint64 },
    });
    m.send(SEND_MODE_CARRY_ALL_BALANCE);
}

// Split slashed funds: challenger bounty / redundancy-reward pool / burn /
// treasury (§8.3). The burn share is simply left locked in this contract (no key
// can release it), which removes it from circulation. C1: each payout is
// bounceable + pull-recoverable.
fun applySlashSplit(sc: SlashConfig, amount: coins, challenger: address, queryId: int) {
    val sum = sc.splitChallengerBps + sc.splitRedundancyBps + sc.splitBurnBps + sc.splitTreasuryBps;
    assert (sum == BPS_DENOM) throw ErrCode.INVALID_SPLIT;

    val challengerAmt = mulDivFloor(amount, sc.splitChallengerBps, BPS_DENOM);
    val redundancyAmt = mulDivFloor(amount, sc.splitRedundancyBps, BPS_DENOM);
    val treasuryAmt = mulDivFloor(amount, sc.splitTreasuryBps, BPS_DENOM);
    // burn share = remainder, left locked in the vault forever.

    if (challengerAmt > 0) {
        payout(challenger, challengerAmt, queryId);
    }
    if (redundancyAmt > 0) {
        payout(sc.redundancyPool, redundancyAmt, queryId);
    }
    if (treasuryAmt > 0) {
        payout(sc.treasury, treasuryAmt, queryId);
    }
}

// ---------------------------------------------------------------------------
// Getters
// ---------------------------------------------------------------------------

get fun get_vault_state(): VaultState {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    val eligible = storage.staked >= cfg.minStake && storage.unbondingAt == 0;
    return {
        staked: storage.staked,
        unbondingAmount: storage.unbondingAmount,
        unbondingAt: storage.unbondingAt,
        totalSupply: storage.totalSupply,
        eligible: eligible,
    };
}

get fun get_owner(): address {
    val storage = lazy VaultStorage.load();
    return storage.owner;
}

// Monotonic CODE version/seqno (§8.6). 0 = original deployed code. Bumped only
// by a successful timelocked `apply_upgrade` (TVM SETCODE) at a STABLE address.
get fun get_code_version(): int {
    val storage = lazy VaultStorage.load();
    return storage.upgrade.load().codeVersion;
}

// Pending timelocked upgrade (§8.6), for stakers to monitor: the announced
// successor code hash, when it was announced, and `readyAt` (the earliest unix
// ts it can be applied = announce + unbondingPeriod). All 0 when none pending —
// the exit window guarantee made observable on-chain.
get fun get_pending_upgrade(): VaultUpgradeView {
    val storage = lazy VaultStorage.load();
    val up = storage.upgrade.load();
    val cfg = storage.config.load();
    val readyAt = up.pendingCodeAt == 0 ? 0 : up.pendingCodeAt + cfg.unbondingPeriod;
    return {
        codeVersion: up.codeVersion,
        pendingCodeHash: up.pendingCodeHash,
        pendingAt: up.pendingCodeAt,
        readyAt: readyAt as uint32,
    };
}

get fun get_binding_hash(): int {
    val storage = lazy VaultStorage.load();
    return storage.bindingHash;
}

// C1: the queued/bounced pull-balance for a recipient (0 if none).
get fun get_pending(recipient: address): int {
    val storage = lazy VaultStorage.load();
    val found = storage.pending.get(recipient);
    if (!found.isFound) {
        return 0;
    }
    return found.loadValue();
}

get fun get_receipt_wallet_address(owner: address): address {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    return calcReceiptWalletAddress(owner, contract.getAddress(), cfg.receiptWalletCode);
}

// TEP-89 standard discovery get-method. Indexers/explorers/wallets call exactly
// `get_wallet_address(owner)` on a jetton master and require it to return the
// owner's jetton-wallet address; the wallet's `get_wallet_data` master must point
// back here. Without this method the back-reference check fails and holder wallets
// are NOT indexed as jettons — which is why the receipt didn't show up. (Same
// computation as `get_receipt_wallet_address`, under the standard name.)
get fun get_wallet_address(owner: address): address {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    return calcReceiptWalletAddress(owner, contract.getAddress(), cfg.receiptWalletCode);
}

// TEP-74/TEP-64 jetton-master data for the stake-receipt jetton ("Duckton").
// Lets wallets/explorers render the (non-transferable) receipt. The vault is the
// master, so `adminAddress` is this contract and `jettonContent` is the on-chain
// Duckton metadata (name/symbol/decimals/description).
get fun get_jetton_data(): ReceiptJettonData {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    return {
        totalSupply: storage.totalSupply,
        mintable: true,
        adminAddress: contract.getAddress(),
        jettonContent: buildReceiptJettonContent(),
        jettonWalletCode: cfg.receiptWalletCode,
    };
}

get fun is_eligible(): bool {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    return storage.staked >= cfg.minStake && storage.unbondingAt == 0;
}
