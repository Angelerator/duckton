// Direct tests for the non-transferable stake-receipt jetton wallet
// (BLOCKCHAIN_ECONOMICS §8.5). The vault-driven mint/burn path is covered in
// stake.test.tolk; these exercise the wallet IN ISOLATION (sending straight to
// it) to pin the master-only mint/burn gates, the balance-floor guard, the
// transfer lock and the unknown-opcode policy — guards a malicious sibling or a
// stale indexer could otherwise probe.

import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/common"
import "@contracts/stake_types"
import "@wrappers/StakeReceiptWallet.gen"

const T0: uint32 = 1_700_000_000

fun deployReceipt(master: address, owner: address, balance: coins): StakeReceiptWallet {
    val storage: ReceiptWalletStorage = {
        status: RECEIPT_STATUS_LOCKED,
        balance: balance,
        owner: owner,
        master: master,
    };
    val wallet = StakeReceiptWallet.fromStorage(storage);
    expect(wallet.deploy(master, { value: ton("1") })).toHaveSuccessfulDeploy({ to: wallet.address });
    return wallet;
}

fun emptyPayload(): RemainingBitsAndRefs {
    return beginCell().endCell().beginParse();
}

get fun `test direct mint (internal_transfer) from a non-master is rejected`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val attacker = testing.treasury("attacker");
    val wallet = deployReceipt(master.address, owner.address, ton("0"));

    // A non-master tries to credit the receipt -> rejected (only the vault mints).
    val res = wallet.sendJettonInternalTransfer(
        attacker.address, 1, ton("100"), attacker.address, attacker.address, ton("0"), emptyPayload(), { value: ton("1") },
    );
    expect(res).toHaveFailedTx({ from: attacker.address, to: wallet.address, exitCode: ErrCode.NOT_FROM_MASTER });
    expect(wallet.getWalletData().balance).toEqual(ton("0"));
}

get fun `test direct burn from a non-master is rejected`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val attacker = testing.treasury("attacker");
    val wallet = deployReceipt(master.address, owner.address, ton("100"));

    val res = wallet.sendReceiptBurn(attacker.address, 1, ton("50"), { value: ton("1") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: wallet.address, exitCode: ErrCode.NOT_FROM_MASTER });
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
}

get fun `test burn over balance rejected; burn exactly to zero accepted (boundary)`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val wallet = deployReceipt(master.address, owner.address, ton("100"));

    // Burning more than the balance underflows -> BALANCE_NEGATIVE.
    val over = wallet.sendReceiptBurn(master.address, 1, ton("101"), { value: ton("1") });
    expect(over).toHaveFailedTx({ from: master.address, to: wallet.address, exitCode: ErrCode.BALANCE_NEGATIVE });
    expect(wallet.getWalletData().balance).toEqual(ton("100"));

    // Burning EXACTLY the balance leaves 0 and must succeed (pins `balance >= 0`:
    // a strict `>` would reject a legitimate burn-to-zero).
    val exact = wallet.sendReceiptBurn(master.address, 2, ton("100"), { value: ton("1") });
    expect(exact).toHaveSuccessfulTx<ReceiptBurn>({ from: master.address, to: wallet.address });
    expect(wallet.getWalletData().balance).toEqual(ton("0"));
}

get fun `test mint with a forward amount emits a transfer notification to the owner`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val wallet = deployReceipt(master.address, owner.address, ton("0"));

    // A master mint carrying a forward amount must surface a TEP-74
    // transfer_notification to the owner (pins the `forwardTonAmount > 0` branch:
    // forcing it off would silently drop the wallet-app notification).
    val res = wallet.sendJettonInternalTransfer(
        master.address, 1, ton("100"), master.address, master.address, ton("0.2"), emptyPayload(), { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<JettonInternalTransfer>({ from: master.address, to: wallet.address });
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
    val note = res.findTransaction<JettonTransferNotification>({ from: wallet.address, to: owner.address });
    expect(note).toBeNotNull();
}

get fun `test receipt wallet rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val wallet = deployReceipt(master.address, owner.address, ton("0"));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = wallet.sendAny(master.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: master.address, to: wallet.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test receipt getters and top-up cover read paths`() {
    testing.setNow(T0);
    val master = testing.treasury("master");
    val owner = testing.treasury("owner");
    val wallet = deployReceipt(master.address, owner.address, ton("100"));

    expect(wallet.getOwner()).toEqual(owner.address);
    expect(wallet.getStatus()).toEqual(RECEIPT_STATUS_LOCKED);

    // A plain top-up for storage rent is accepted; balance unchanged.
    val top = wallet.sendReceiptTopUp(master.address, 1, { value: ton("0.1") });
    expect(top).toHaveSuccessfulTx<ReceiptTopUp>({ from: master.address, to: wallet.address });
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
}
