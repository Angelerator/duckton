// RecordAnchor **v2** — the in-place upgrade TARGET proving address-stable code
// upgradeability for the anchor/dispute contract (BLOCKCHAIN_ECONOMICS §12.1).
//
// Faithful successor of `RecordAnchor.tolk`: SAME storage layout (`AnchorStorage`
// — so the anchored epoch chain + open disputes are read unchanged after the
// swap) and SAME authority-gated handlers, plus ONE new getter v1 lacked —
// `get_verdict_authority()`. After an authority-gated `upgrade_code` (TVM
// SETCODE) the contract exposes this getter at the SAME address over the SAME
// anchored history: the concrete proof the new code is live without a redeploy.
//
// Keeps `upgrade_code` so v2 is itself upgradable (v2 -> v3 ...).

import "anchor_types"
import "common"

contract RecordAnchorV2 {
    storage: AnchorStorage
    incomingMessages: AllowedMessageToAnchorV2
}

type AllowedMessageToAnchorV2 =
    | AnchorSubmitRoot
    | AnchorOpenDispute
    | AnchorResolveDispute
    | AnchorAnnounceUpgrade
    | AnchorUpgradeCode
    | AnchorCancelUpgrade

// A1 bounce-safety (mirrors v1): a bounced bond refund re-opens its dispute.
fun onBouncedMessage(in: InMessageBounced) {
    var body = in.bouncedBody;
    body.skipBouncedPrefix();
    val op = body.loadUint(32);
    if (op == OP_ANCHOR_BOND_REFUND) {
        val disputeId = body.loadUint(32) as uint32;
        var storage = lazy AnchorStorage.load();
        val found = storage.disputes.get(disputeId);
        if (found.isFound) {
            var dispute = found.loadValue();
            dispute.status = 0;
            storage.disputes.set(disputeId, dispute);
            storage.save();
        }
    }
}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToAnchorV2.fromSlice(in.body);

    match (msg) {
        AnchorSubmitRoot => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();

            assert (in.senderAddress == cfg.auth.load().keeper) throw ErrCode.NOT_ANCHOR_KEEPER;
            validateAnchorConfig(cfg);
            assert (msg.root != 0) throw ErrCode.ZERO_ROOT;
            assert (msg.epoch == storage.currentEpoch + 1) throw ErrCode.EPOCH_REGRESSION;
            assert (msg.prevRoot == storage.lastRoot) throw ErrCode.BAD_PREV_ROOT;
            assert (msg.stakeWeight >= cfg.minStakeWeight) throw ErrCode.INSUFFICIENT_ANCHOR_STAKE;

            storage.currentEpoch = msg.epoch;
            storage.lastRoot = msg.root;
            storage.roots.set(msg.epoch, msg.root);
            storage.save();
        }

        AnchorOpenDispute => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (in.valueCoins >= cfg.disputeBondMin) throw ErrCode.DISPUTE_BOND_TOO_LOW;

            val rootFound = storage.roots.get(msg.epoch);
            assert (rootFound.isFound) throw ErrCode.UNKNOWN_EPOCH;
            val anchoredRoot = rootFound.loadValue();

            val recomputed = computeRootFromProof(msg.leaf, msg.proof);
            assert (recomputed == anchoredRoot) throw ErrCode.BAD_INCLUSION_PROOF;

            val id = storage.nextDisputeId;
            storage.nextDisputeId = id + 1;
            storage
                .disputes
                .set(
                    id,
                    {
                        challenger: in.senderAddress,
                        epoch: msg.epoch,
                        leaf: msg.leaf,
                        bond: in.valueCoins,
                        status: 0,
                    },
                );
            storage.save();
        }

        AnchorResolveDispute => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.verdictAuthority) throw ErrCode.NOT_VERDICT_AUTHORITY;

            val found = storage.disputes.get(msg.disputeId);
            assert (found.isFound) throw ErrCode.NO_SUCH_DISPUTE;
            var dispute = found.loadValue();
            assert (dispute.status == 0) throw ErrCode.DISPUTE_NOT_OPEN;

            if (msg.upheld) {
                dispute.status = 1;
            } else {
                dispute.status = 2;
            }
            storage.disputes.set(msg.disputeId, dispute);
            storage.save();

            if (msg.upheld) {
                refundBond(dispute.challenger, dispute.bond, msg.disputeId);
            } else {
                refundBond(cfg.auth.load().treasury, dispute.bond, msg.disputeId);
            }
        }

        // D2 timelocked upgrade (mirrors v1): announce -> wait -> apply, plus cancel.
        AnchorAnnounceUpgrade => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (
                in.senderAddress == cfg.auth.load().upgradeAuthority
            ) throw ErrCode.UPGRADE_NOT_AUTHORITY;
            storage.pendingCodeHash = msg.newCodeHash;
            storage.pendingCodeAt = blockchain.now() as uint32;
            storage.save();
        }

        AnchorUpgradeCode => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (
                in.senderAddress == cfg.auth.load().upgradeAuthority
            ) throw ErrCode.UPGRADE_NOT_AUTHORITY;
            assert (storage.pendingCodeHash != 0) throw ErrCode.NO_PENDING_UPGRADE;
            assert (
                msg.newCode.hash() == storage.pendingCodeHash
            ) throw ErrCode.UPGRADE_CODE_MISMATCH;
            val readyAt = storage.pendingCodeAt + storage.upgradeDelay;
            assert (blockchain.now() >= readyAt) throw ErrCode.UPGRADE_TIMELOCK_ACTIVE;
            contract.setCodePostponed(msg.newCode);
            storage.codeVersion += 1;
            storage.pendingCodeHash = 0;
            storage.pendingCodeAt = 0;
            storage.save();
        }

        AnchorCancelUpgrade => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (
                in.senderAddress == cfg.auth.load().upgradeAuthority
            ) throw ErrCode.UPGRADE_NOT_AUTHORITY;
            assert (storage.pendingCodeHash != 0) throw ErrCode.NO_PENDING_UPGRADE;
            storage.pendingCodeHash = 0;
            storage.pendingCodeAt = 0;
            storage.save();
        }

        else => {
            assert (in.body.isEmpty()) throw ErrCode.INVALID_OP;
        }
    }
}

fun refundBond(to: address, amount: coins, disputeId: int) {
    val m = createMessage({
        bounce: BounceMode.Only256BitsOfBody,
        dest: to,
        value: amount,
        body: AnchorBondRefund { disputeId: disputeId as uint32 },
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY);
}

// ---------------------------------------------------------------------------
// Getters — identical to v1, PLUS the new `get_verdict_authority` that proves
// the upgraded code is live at the unchanged address.
// ---------------------------------------------------------------------------

get fun get_anchor_state(): AnchorState {
    val storage = lazy AnchorStorage.load();
    return {
        currentEpoch: storage.currentEpoch,
        lastRoot: storage.lastRoot,
        nextDisputeId: storage.nextDisputeId,
    };
}

get fun get_code_version(): int {
    val storage = lazy AnchorStorage.load();
    return storage.codeVersion;
}

get fun get_pending_upgrade(): AnchorUpgradeView {
    val storage = lazy AnchorStorage.load();
    val readyAt = storage.pendingCodeAt == 0 ? 0 : storage.pendingCodeAt + storage.upgradeDelay;
    return {
        codeVersion: storage.codeVersion,
        pendingCodeHash: storage.pendingCodeHash,
        pendingAt: storage.pendingCodeAt,
        readyAt: readyAt as uint32,
    };
}

// NEW in v2: expose the configured dispute verdict authority (governance), which
// v1 kept only inside the config child cell. Adding it used to require a redeploy
// (new code -> new address, orphaning the epoch chain); now it ships in place.
get fun get_verdict_authority(): address {
    val storage = lazy AnchorStorage.load();
    return storage.config.load().verdictAuthority;
}

get fun get_dispute(disputeId: int): DisputeInfo? {
    val storage = lazy AnchorStorage.load();
    val found = storage.disputes.get(disputeId as uint32);
    if (!found.isFound) {
        return null;
    }
    return found.loadValue();
}

get fun verify_inclusion(leaf: int, root: int, proof: Cell<MerkleStep>?): bool {
    return computeRootFromProof(leaf, proof) == root;
}

get fun compute_root(leaf: int, proof: Cell<MerkleStep>?): int {
    return computeRootFromProof(leaf, proof);
}
