// GlobalParams **v2** — the in-place upgrade TARGET used to prove address-stable
// code upgradeability (BLOCKCHAIN_ECONOMICS §12.1).
//
// This is a faithful successor of `GlobalParams.tolk`: SAME storage layout
// (`GlobalParamsStorage`, so the live storage is read unchanged after the swap)
// and SAME admin-gated handlers, plus ONE genuinely new getter that v1 lacked —
// `get_surcharge_bps()`. That extra getter is the concrete demonstration of the
// win: a deployed `GlobalParams` is upgraded to this code via TVM `SETCODE`
// (`upgrade_code`) and AFTERWARDS exposes `get_surcharge_bps` at the SAME
// address with the SAME params/admin/version state — no redeploy, no new
// address. Before this change, exposing a new getter/field forced a redeploy to
// a fresh address; now it does not.
//
// It keeps the `upgrade_code` handler so v2 is itself upgradable (v2 -> v3 ...).

import "common"
import "global_params_types"

contract GlobalParamsV2 {
    storage: GlobalParamsStorage
    incomingMessages: AllowedMessageToGlobalParamsV2
}

type AllowedMessageToGlobalParamsV2 =
    | UpdateParams
    | UpdateAdmin
    | SetBlocklisted
    | UpgradeCode

fun onBouncedMessage(_in: InMessageBounced) {}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToGlobalParamsV2.fromSlice(in.body);

    match (msg) {
        UpdateParams => {
            var storage = lazy GlobalParamsStorage.load();
            assert (in.senderAddress == storage.admin) throw ErrCode.NOT_ADMIN;

            val newParams = msg.params.load();
            validateEcoParams(newParams);

            storage.feeRecipient = msg.feeRecipient;
            storage.params = msg.params;
            storage.paramsVersion += 1;
            storage.save();

            val ev = createExternalLogMessage({
                dest: ExtOutLogBucket { topic: EVENT_PARAMS_UPDATED },
                body: ParamsUpdated {
                    queryId: msg.queryId,
                    version: storage.paramsVersion,
                    platformFeeBps: newParams.platformFeeBps,
                    minStake: newParams.minStake,
                },
            });
            ev.send(SEND_MODE_REGULAR);
        }

        UpdateAdmin => {
            var storage = lazy GlobalParamsStorage.load();
            assert (in.senderAddress == storage.admin) throw ErrCode.NOT_ADMIN;
            storage.admin = msg.newAdmin;
            storage.save();
        }

        SetBlocklisted => {
            var storage = lazy GlobalParamsStorage.load();
            assert (in.senderAddress == storage.admin) throw ErrCode.NOT_ADMIN;
            storage.blocklist.set(msg.subject, (msg.blocked ? 1 : 0) as uint1);
            storage.save();
        }

        UpgradeCode => {
            var storage = lazy GlobalParamsStorage.load();
            assert (in.senderAddress == storage.admin) throw ErrCode.NOT_ADMIN;
            contract.setCodePostponed(msg.newCode);
            storage.codeVersion += 1;
            storage.save();

            val ev = createExternalLogMessage({
                dest: ExtOutLogBucket { topic: EVENT_CODE_UPGRADED },
                body: CodeUpgraded {
                    queryId: msg.queryId,
                    codeVersion: storage.codeVersion,
                    codeHash: msg.newCode.hash(),
                },
            });
            ev.send(SEND_MODE_REGULAR);
        }

        else => {
            assert (in.body.isEmpty()) throw ErrCode.INVALID_OP;
        }
    }
}

// ---------------------------------------------------------------------------
// Getters — identical to v1, PLUS the new `get_surcharge_bps` that proves the
// upgraded code is live at the unchanged address.
// ---------------------------------------------------------------------------

get fun get_params(): GlobalParamsView {
    val storage = lazy GlobalParamsStorage.load();
    return {
        admin: storage.admin,
        feeRecipient: storage.feeRecipient,
        params: storage.params,
        paramsVersion: storage.paramsVersion,
    };
}

get fun get_params_version(): int {
    val storage = lazy GlobalParamsStorage.load();
    return storage.paramsVersion;
}

get fun get_code_version(): int {
    val storage = lazy GlobalParamsStorage.load();
    return storage.codeVersion;
}

// NEW in v2: surcharge bps was stored all along but never exposed as a scalar
// getter. Adding it used to require a redeploy (new code -> new address); with
// in-place `upgrade_code` it ships at the SAME address. Its presence after the
// swap is the proof that the new code is live.
get fun get_surcharge_bps(): int {
    return GlobalParamsStorage.load().params.load().surchargeBps;
}

get fun get_platform_fee_bps(): int {
    return GlobalParamsStorage.load().params.load().platformFeeBps;
}

get fun get_participation_commission_bps(): int {
    return GlobalParamsStorage.load().params.load().participationCommissionBps;
}

get fun get_admin(): address {
    val storage = lazy GlobalParamsStorage.load();
    return storage.admin;
}

get fun get_fee_recipient(): address {
    val storage = lazy GlobalParamsStorage.load();
    return storage.feeRecipient;
}

get fun get_blocklisted(subject: int): bool {
    val storage = lazy GlobalParamsStorage.load();
    val found = storage.blocklist.get(subject as uint256);
    if (!found.isFound) {
        return false;
    }
    return found.loadValue() == 1;
}
