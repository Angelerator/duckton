// StakeVault **v2** — the in-place upgrade TARGET proving address-stable, but
// TIMELOCKED + governance-gated, code upgradeability for the bond-custody vault
// (BLOCKCHAIN_ECONOMICS §8.6, §12.1).
//
// Faithful successor of `StakeVault.tolk`: SAME storage layout (`VaultStorage`,
// so bonds / supply / cooldown / the upgrade child cell are read unchanged after
// the swap) and SAME handlers (including the timelocked announce -> apply path),
// plus ONE new getter v1 lacked — `get_slasher()`. After a timelocked
// `apply_upgrade` (TVM SETCODE), the vault exposes this getter at the SAME
// address over the SAME bonded state: the proof the new code is live without a
// redeploy (which would have stranded every staker's bond at the old address).
//
// Keeps the announce/apply/cancel handlers so v2 is itself upgradable under the
// same non-custodial, timelocked discipline (v2 -> v3 ...).

import "common"
import "stake_types"

contract StakeVaultV2 {
    storage: VaultStorage
    incomingMessages: AllowedMessageToVaultV2
}

type AllowedMessageToVaultV2 =
    | StakeDeposit
    | StakeRequestUnbond
    | StakeWithdraw
    | StakeKeeperWithdraw
    | StakeSlash
    | StakeClaim
    | ProvideWalletAddress
    | ReceiptTopUp
    | StakeAnnounceUpgrade
    | StakeApplyUpgrade
    | StakeCancelUpgrade

// C1/C2 bounce-safety (mirrors v1).
fun onBouncedMessage(in: InMessageBounced) {
    var body = in.bouncedBody;
    body.skipBouncedPrefix();
    val op = body.loadUint(32);
    var storage = lazy VaultStorage.load();

    if (op == OP_JETTON_TRANSFER_INTERNAL) {
        // C2: mint bounced — claw `amount` back out of staked then the cooldown
        // bucket and shrink totalSupply by exactly what was reclaimed, keeping
        // `totalSupply == staked + unbondingAmount` exact even if an unbond raced
        // the bounce (mirrors the v1 fix).
        body.skipBits(64);
        val amount = body.loadCoins();
        val fromStaked = min(amount, storage.staked);
        storage.staked -= fromStaked;
        val fromUnbonding = min(amount - fromStaked, storage.unbondingAmount);
        storage.unbondingAmount -= fromUnbonding;
        storage.totalSupply -= fromStaked + fromUnbonding;
        storage.save();
    } else if (op == OP_STAKE_PAYOUT) {
        val prev = storage.pending.get(in.senderAddress);
        val had: coins = prev.isFound ? prev.loadValue() : 0;
        storage.pending.set(in.senderAddress, had + in.valueCoins);
        storage.save();
    }
}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToVaultV2.fromSlice(in.body);

    match (msg) {
        StakeDeposit => {
            var storage = lazy VaultStorage.load();
            assert (in.senderAddress == storage.owner) throw ErrCode.NOT_OWNER;
            assert (
                in.valueCoins >= msg.amount + MIN_TONS_FOR_STORAGE
            ) throw ErrCode.NOT_ENOUGH_GAS;

            val cfg = storage.config.load();
            validateVaultConfig(cfg);

            storage.staked += msg.amount;
            storage.totalSupply += msg.amount;
            storage.save();

            mintReceipt(
                storage.owner,
                contract.getAddress(),
                cfg.receiptWalletCode,
                msg.amount,
                msg.queryId,
            );
        }

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
            payoutLiquidRemainder(storage.owner, storage.staked, msg.queryId);
        }

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
            reserveToncoinsOnBalance(
                storage.staked + MIN_TONS_FOR_STORAGE,
                RESERVE_MODE_EXACT_AMOUNT,
            );
            if (bounty > 0) {
                payout(in.senderAddress, bounty, msg.queryId);
            }
            payoutRemainderNoReserve(storage.owner, msg.queryId);
        }

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

        // §8.6 timelocked governance upgrade — identical discipline to v1.
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

        StakeApplyUpgrade => {
            var storage = lazy VaultStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.upgradeAuthority) throw ErrCode.UPGRADE_NOT_AUTHORITY;

            var up = storage.upgrade.load();
            assert (up.pendingCodeHash != 0) throw ErrCode.NO_PENDING_UPGRADE;
            assert (msg.newCode.hash() == up.pendingCodeHash) throw ErrCode.UPGRADE_CODE_MISMATCH;
            val readyAt = up.pendingCodeAt + cfg.unbondingPeriod;
            assert (blockchain.now() >= readyAt) throw ErrCode.UPGRADE_TIMELOCK_ACTIVE;

            contract.setCodePostponed(msg.newCode);
            up.codeVersion += 1;
            up.pendingCodeHash = 0;
            up.pendingCodeAt = 0;
            storage.upgrade = up.toCell();
            storage.save();
        }

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
// Internal helpers (duplicated from v1 — separate compilation unit)
// ---------------------------------------------------------------------------

fun mintReceipt(owner: address, master: address, walletCode: cell, amount: coins, queryId: int) {
    val dest = calcDeployedReceiptWallet(owner, master, walletCode);
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

fun payout(to: address, amount: coins, queryId: int) {
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: to,
        value: amount,
        body: StakePayout { queryId: queryId as uint64 },
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY);
}

fun payoutLiquidRemainder(to: address, keepReserved: coins, queryId: int) {
    reserveToncoinsOnBalance(keepReserved + MIN_TONS_FOR_STORAGE, RESERVE_MODE_EXACT_AMOUNT);
    payoutRemainderNoReserve(to, queryId);
}

fun payoutRemainderNoReserve(to: address, queryId: int) {
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: to,
        value: 0,
        body: StakePayout { queryId: queryId as uint64 },
    });
    m.send(SEND_MODE_CARRY_ALL_BALANCE);
}

fun applySlashSplit(sc: SlashConfig, amount: coins, challenger: address, queryId: int) {
    val sum = sc.splitChallengerBps + sc.splitRedundancyBps + sc.splitBurnBps + sc.splitTreasuryBps;
    assert (sum == BPS_DENOM) throw ErrCode.INVALID_SPLIT;

    val challengerAmt = mulDivFloor(amount, sc.splitChallengerBps, BPS_DENOM);
    val redundancyAmt = mulDivFloor(amount, sc.splitRedundancyBps, BPS_DENOM);
    val treasuryAmt = mulDivFloor(amount, sc.splitTreasuryBps, BPS_DENOM);

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
// Getters — identical to v1, PLUS the new `get_slasher` that proves the
// upgraded code is live at the unchanged address.
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

get fun get_code_version(): int {
    val storage = lazy VaultStorage.load();
    return storage.upgrade.load().codeVersion;
}

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

// NEW in v2: expose the configured slasher/governance authority, which v1 kept
// only inside the config child cell. Adding it used to require a redeploy (new
// code -> new address, stranding every bond); now it ships in place.
get fun get_slasher(): address {
    val storage = lazy VaultStorage.load();
    return storage.config.load().slasher;
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

get fun get_wallet_address(owner: address): address {
    val storage = lazy VaultStorage.load();
    val cfg = storage.config.load();
    return calcReceiptWalletAddress(owner, contract.getAddress(), cfg.receiptWalletCode);
}

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
