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
    | ReceiptMint // mint, sent by the master vault
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
        // Mint path: the master credits this wallet. Because the receipt is
        // non-transferable, the only legitimate minter is the master vault.
        ReceiptMint => {
            var storage = lazy ReceiptWalletStorage.load();
            assert (in.senderAddress == storage.master) throw ERROR_NOT_FROM_MASTER;
            storage.balance += msg.amount;
            storage.save();
        }

        // Transfer path: hard-locked. This is the core anti-exit guarantee.
        JettonTransfer => {
            throw ERROR_RECEIPT_LOCKED;
        }

        // Burn path: only the master may reduce the receipt balance.
        ReceiptBurn => {
            var storage = lazy ReceiptWalletStorage.load();
            assert (in.senderAddress == storage.master) throw ERROR_NOT_FROM_MASTER;
            storage.balance -= msg.amount;
            assert (storage.balance >= 0) throw ERROR_BALANCE_NEGATIVE;
            storage.save();
        }

        ReceiptTopUp => {
            // accept TON top-ups for storage rent
        }

        else => {
            assert (in.body.isEmpty()) throw ERROR_INVALID_OP;
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
