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
    | AnchorUpgradeCode

fun onBouncedMessage(_in: InMessageBounced) {}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToAnchorV2.fromSlice(in.body);

    match (msg) {
        AnchorSubmitRoot => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();

            assert (msg.epoch == storage.currentEpoch + 1) throw ErrCode.EPOCH_REGRESSION;
            assert (msg.prevRoot == storage.lastRoot) throw ErrCode.BAD_PREV_ROOT;
            assert (msg.stakeWeight >= cfg.minStakeWeight) throw ErrCode.INSUFFICIENT_ANCHOR_STAKE;

            storage.currentEpoch = msg.epoch;
            storage.lastRoot = msg.root;
            storage.save();
        }

        AnchorOpenDispute => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (in.valueCoins >= cfg.disputeBondMin) throw ErrCode.DISPUTE_BOND_TOO_LOW;

            val recomputed = computeRootFromProof(msg.leaf, msg.proof);
            assert (recomputed == msg.claimedRoot) throw ErrCode.BAD_INCLUSION_PROOF;

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

            if (msg.upheld) {
                dispute.status = 1;
                refund(dispute.challenger, dispute.bond);
            } else {
                dispute.status = 2;
                refund(cfg.treasury, dispute.bond);
            }
            storage.disputes.set(msg.disputeId, dispute);
            storage.save();
        }

        AnchorUpgradeCode => {
            var storage = lazy AnchorStorage.load();
            val cfg = storage.config.load();
            assert (in.senderAddress == cfg.verdictAuthority) throw ErrCode.NOT_VERDICT_AUTHORITY;
            contract.setCodePostponed(msg.newCode);
            storage.codeVersion += 1;
            storage.save();
        }

        else => {
            assert (in.body.isEmpty()) throw ErrCode.INVALID_OP;
        }
    }
}

fun refund(to: address, amount: coins) {
    val m = createMessage({
        bounce: BounceMode.NoBounce,
        dest: to,
        value: amount,
    });
    m.send(SEND_MODE_PAY_FEES_SEPARATELY | SEND_MODE_IGNORE_ERRORS);
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
