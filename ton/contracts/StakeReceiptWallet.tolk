// Stake-receipt jetton wallet — TEP-74 shaped but NON-TRANSFERABLE.
//
// BLOCKCHAIN_ECONOMICS §8.5: the stake receipt is a *slashable accountability
// bond*, not a liquid-staking position. A freely transferable receipt would let
// a host sell it and exit to dodge slashing, so the outgoing-transfer path is
// LOCKED here for the whole lifetime of the bond (mint -> burn). The receipt is
// minted 1:1 on deposit and burned on withdrawal/slash by the master vault.

import "common"
import "stake_types"

contract StakeReceiptWallet {
    storage: ReceiptWalletStorage
    incomingMessages: AllowedMessageToReceiptWallet
}

type AllowedMessageToReceiptWallet =
    | JettonInternalTransfer // standard TEP-74 credit (mint), sent by the master vault
    | JettonTransfer // ALWAYS rejected (non-transferable)
    | ReceiptBurn // master-driven burn
    | ReceiptTopUp

fun onBouncedMessage(_in: InMessageBounced) {
    // Mint is the only bounceable message the wallet emits indirectly; on bounce
    // the master reconciles supply. Nothing to restore locally.
}

fun onInternalMessage(in: InMessage) {
    val msg = lazy AllowedMessageToReceiptWallet.fromSlice(in.body);

    match (msg) {
        // Mint path: the master credits this wallet via the STANDARD TEP-74
        // internal_transfer op so indexers recognize the credit as a jetton mint.
        // Because the receipt is non-transferable, the ONLY legitimate sender is
        // the master vault (no sibling-wallet transfers ever happen — the transfer
        // op is locked below), so we still gate this on the master.
        JettonInternalTransfer => {
            var storage = lazy ReceiptWalletStorage.load();
            assert (in.senderAddress == storage.master) throw ErrCode.NOT_FROM_MASTER;
            storage.balance += msg.amount;
            storage.save();

            // Standard behaviour: notify the owner when a forward amount is
            // attached, so wallet apps surface the incoming receipt. Does NOT
            // unlock transfers — purely a recognition/UX signal.
            if (msg.forwardTonAmount > 0) {
                val note = createMessage({
                    bounce: BounceMode.NoBounce,
                    dest: storage.owner,
                    value: msg.forwardTonAmount,
                    body: JettonTransferNotification {
                        queryId: msg.queryId,
                        amount: msg.amount,
                        sender: storage.master,
                        forwardPayload: msg.forwardPayload,
                    },
                });
                note.send(SEND_MODE_PAY_FEES_SEPARATELY);
            }
        }

        // Transfer path: hard-locked. This is the core anti-exit guarantee.
        JettonTransfer => {
            throw ErrCode.RECEIPT_LOCKED;
        }

        // Burn path: only the master may reduce the receipt balance.
        ReceiptBurn => {
            var storage = lazy ReceiptWalletStorage.load();
            assert (in.senderAddress == storage.master) throw ErrCode.NOT_FROM_MASTER;
            storage.balance -= msg.amount;
            assert (storage.balance >= 0) throw ErrCode.BALANCE_NEGATIVE;
            storage.save();
        }

        ReceiptTopUp => {
            // accept TON top-ups for storage rent
        }

        else => {
            assert (in.body.isEmpty()) throw ErrCode.INVALID_OP;
        }
    }
}

// ---------------------------------------------------------------------------
// Getters (TEP-74)
// ---------------------------------------------------------------------------

get fun get_wallet_data(): ReceiptWalletData {
    val storage = lazy ReceiptWalletStorage.load();
    return {
        balance: storage.balance,
        owner: storage.owner,
        minter: storage.master,
        code: contract.getCode(),
    };
}

get fun get_status(): int {
    val storage = lazy ReceiptWalletStorage.load();
    return storage.status;
}

get fun get_owner(): address {
    val storage = lazy ReceiptWalletStorage.load();
    return storage.owner;
}

// The receipt is transfer-locked iff bit 0 of status is set (always true here).
get fun is_transfer_locked(): bool {
    val storage = lazy ReceiptWalletStorage.load();
    return (storage.status & RECEIPT_STATUS_LOCKED) == RECEIPT_STATUS_LOCKED;
}
