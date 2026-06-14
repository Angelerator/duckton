import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"
import "@acton/tolk-stdlib/tvm-dicts"

import "@contracts/common"
import "@contracts/stake_types"
import "@wrappers/StakeReceiptWallet.gen"
import "@wrappers/StakeVault.gen"

const T0: uint32 = 1_700_000_000
const UNBONDING_PERIOD: uint32 = 1000
const CHALLENGE_WINDOW: uint32 = 500
const KEEPER_GRACE: uint32 = 100

// Build a vault config with the given minimum stake.
fun makeConfig(
    minStake: coins,
    slasher: address,
    treasury: address,
    redundancy: address,
): Cell<VaultConfig> {
    val slash: SlashConfig = {
        treasury: treasury,
        redundancyPool: redundancy,
        splitChallengerBps: 4000, // 40%
        splitRedundancyBps: 3000, // 30%
        splitBurnBps: 2000,       // 20%
        splitTreasuryBps: 1000,   // 10%
    };
    val cfg: VaultConfig = {
        minStake: minStake,
        unbondingPeriod: UNBONDING_PERIOD,
        challengeWindow: CHALLENGE_WINDOW,
        keeperGrace: KEEPER_GRACE,
        keeperBountyBps: 200, // 2%
        slasher: slasher,
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    return cfg.toCell();
}

fun deployVault(
    owner: address,
    minStake: coins,
    slasher: address,
    treasury: address,
    redundancy: address,
): StakeVault {
    val storage: VaultStorage = {
        owner: owner,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: 0xDEADBEEF,
        config: makeConfig(minStake, slasher, treasury, redundancy),
    };
    val vault = StakeVault.fromStorage(storage);
    val res = vault.deploy(owner, { value: ton("1") });
    expect(res).toHaveSuccessfulDeploy({ to: vault.address });
    return vault;
}

fun receiptWalletOf(vault: StakeVault, owner: address): StakeReceiptWallet {
    return StakeReceiptWallet.fromAddress(vault.getReceiptWalletAddress(owner));
}

// Decode a TEP-64 on-chain metadata value the way an explorer/indexer does:
// read the snake `0x00` prefix on the `^`-ref value for `sha256(key)`, then
// return the remaining UTF-8 bytes. Spec-true (not self-referential): it parses
// the actual content cell rather than re-hashing the builder output.
fun decodeOnchainMetaValue(content: dict, keyHash: int): slice {
    val (valueRef, found) = content.uDictGetRef(256, keyHash);
    expect(found).toBeTrue();
    var value = valueRef!.beginParse();
    expect(value.loadUint(8)).toEqual(0); // snake-format prefix
    return value;
}

get fun `test deposit bonds TON and mints a 1to1 stake-receipt jetton`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val treasury = randomAddress("treasury");
    val redundancy = randomAddress("redundancy");
    val vault = deployVault(owner.address, ton("10"), slasher.address, treasury, redundancy);

    val res = vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });
    expect(res).toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });

    val state = vault.getVaultState();
    expect(state.staked).toEqual(ton("100"));
    expect(state.totalSupply).toEqual(ton("100"));
    expect(state.eligible).toBeTrue();

    // Receipt minted 1:1 to the owner's (non-transferable) receipt wallet.
    val wallet = receiptWalletOf(vault, owner.address);
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
    expect(wallet.isTransferLocked()).toBeTrue();
}

get fun `test stake-receipt jetton transfer is locked (anti-exit)`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    val wallet = receiptWalletOf(vault, owner.address);
    // The owner tries a standard TEP-74 transfer to dodge slashing -> rejected.
    val res = wallet.sendJettonTransfer(
        owner.address,
        7,
        ton("50"),
        attacker.address,
        owner.address,
        null,
        ton("0"),
        beginCell().endCell().beginParse(),
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: owner.address, to: wallet.address, exitCode: ErrCode.RECEIPT_LOCKED });
    // Balance unchanged.
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
}

get fun `test slashing reduces stake, burns receipt and splits funds`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val challenger = randomAddress("challenger");
    val treasury = randomAddress("treasury");
    val redundancy = randomAddress("redundancy");
    val vault = deployVault(owner.address, ton("10"), slasher.address, treasury, redundancy);
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });

    val res = vault.sendStakeSlash(
        slasher.address,
        2,
        ton("50"),
        1,
        challenger,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<StakeSlash>({ from: slasher.address, to: vault.address });

    val state = vault.getVaultState();
    expect(state.staked).toEqual(ton("50"));
    expect(state.totalSupply).toEqual(ton("50"));

    // Receipt burned to match the slashed amount.
    val wallet = receiptWalletOf(vault, owner.address);
    expect(wallet.getWalletData().balance).toEqual(ton("50"));

    // Split: 40% challenger, 30% redundancy, 10% treasury (20% burned/locked).
    expect(testing.getAccountBalance(challenger)).toEqual(ton("20"));
    expect(testing.getAccountBalance(redundancy)).toEqual(ton("15"));
    expect(testing.getAccountBalance(treasury)).toEqual(ton("5"));
}

get fun `test slash from non-slasher is rejected`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val notSlasher = testing.treasury("notSlasher");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });

    val res = vault.sendStakeSlash(
        notSlasher.address,
        2,
        ton("50"),
        1,
        randomAddress("c"),
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: notSlasher.address, to: vault.address, exitCode: ErrCode.NOT_SLASHER });
}

get fun `test unbonding cooldown blocks early withdraw and stays slashable`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val challenger = randomAddress("challenger");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });

    // Begin unbonding 40 of 100.
    val u = vault.sendStakeRequestUnbond(owner.address, 2, ton("40"));
    expect(u).toHaveSuccessfulTx<StakeRequestUnbond>({ from: owner.address, to: vault.address });
    var state = vault.getVaultState();
    expect(state.staked).toEqual(ton("60"));
    expect(state.unbondingAmount).toEqual(ton("40"));
    expect(
        state.eligible,
    ).toBeFalse();                      // unbonding in progress => not eligible for new high-tier jobs

    // Withdraw before cooldown elapses -> rejected.
    val early = vault.sendStakeWithdraw(owner.address, 3);
    expect(
        early,
    ).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.COOLDOWN_NOT_ELAPSED });

    // Slash 70 DURING cooldown: 60 from bonded + 10 from the still-slashable cooldown bucket.
    val s = vault.sendStakeSlash(
        slasher.address,
        4,
        ton("70"),
        1,
        challenger,
        { value: ton("1") },
    );
    expect(s).toHaveSuccessfulTx<StakeSlash>({ from: slasher.address, to: vault.address });
    state = vault.getVaultState();
    expect(state.staked).toEqual(ton("0"));
    expect(state.unbondingAmount).toEqual(ton("30"));
    expect(state.totalSupply).toEqual(ton("30"));

    // After the cooldown, withdraw releases the remaining cooled stake.
    testing.setNow(T0 + UNBONDING_PERIOD);
    val w = vault.sendStakeWithdraw(owner.address, 5);
    expect(w).toHaveSuccessfulTx<StakeWithdraw>({ from: owner.address, to: vault.address });
    state = vault.getVaultState();
    expect(state.unbondingAmount).toEqual(ton("0"));
    expect(state.totalSupply).toEqual(ton("0"));
}

get fun `test keeper can finalize an unresponsive owner withdraw for a bounty`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val keeper = randomAddress("keeper");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });
    vault.sendStakeRequestUnbond(owner.address, 2, ton("100"));

    // Before cooldown + grace, the keeper is too early.
    testing.setNow(T0 + UNBONDING_PERIOD);
    val early = vault.sendStakeKeeperWithdraw(keeper, 3, { value: ton("1") });
    expect(
        early,
    ).toHaveFailedTx({ from: keeper, to: vault.address, exitCode: ErrCode.KEEPER_TOO_EARLY });

    // After cooldown + grace, anyone can finalize; the keeper earns a 2% bounty.
    testing.setNow(T0 + UNBONDING_PERIOD + KEEPER_GRACE);
    val res = vault.sendStakeKeeperWithdraw(keeper, 4, { value: ton("1") });
    expect(res).toHaveSuccessfulTx<StakeKeeperWithdraw>({ from: keeper, to: vault.address });

    val state = vault.getVaultState();
    expect(state.unbondingAmount).toEqual(ton("0"));
    expect(state.totalSupply).toEqual(ton("0"));
    // 2% of 100 = 2 TON bounty to the keeper.
    expect(testing.getAccountBalance(keeper)).toBeGreaterOrEqual(ton("2"));
}

get fun `test stake-receipt jetton exposes Duckton TEP-64 metadata`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    // TEP-74 master data: mintable, supply matches the bond, master == vault,
    // and the receipt-wallet code is the deployed StakeReceiptWallet code.
    val data = vault.getJettonData();
    expect(data.mintable).toBeTrue();
    expect(data.totalSupply).toEqual(ton("100"));
    expect(data.adminAddress).toEqual(vault.address);
    expect(data.jettonWalletCode.hash()).toEqual(build("StakeReceiptWallet").hash());

    // SPEC-TRUE TEP-64 decode (not self-referential): parse the actual content
    // cell exactly as an explorer would — read the `0x00` on-chain root tag, load
    // the dict, look up `sha256("name"|"symbol"|"decimals")`, follow the `^` ref,
    // read the `0x00` snake prefix, and assert the decoded UTF-8 bytes. This
    // catches any format regression that would stop wallets/explorers rendering it.
    var content = data.jettonContent.beginParse();
    expect(content.loadUint(8)).toEqual(0); // on-chain content root tag
    val meta = content.loadDict();
    expect(decodeOnchainMetaValue(meta, "name".sha256()).bitsEqual("Duckton".literalSlice())).toBeTrue();
    expect(decodeOnchainMetaValue(meta, "symbol".sha256()).bitsEqual("DUCKTON".literalSlice())).toBeTrue();
    expect(decodeOnchainMetaValue(meta, "decimals".sha256()).bitsEqual("9".literalSlice())).toBeTrue();
}

get fun `test receipt wallet exposes standard TEP-74 get_wallet_data shape`() {
    // Indexers recognize a jetton wallet by `get_wallet_data` returning, in order,
    // (balance, owner, jetton_master, jetton_wallet_code). Assert that exact shape
    // and that the master back-reference matches the vault.
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    val wallet = receiptWalletOf(vault, owner.address);
    val wd = wallet.getWalletData();
    expect(wd.balance).toEqual(ton("100"));
    expect(wd.owner).toEqual(owner.address);
    expect(wd.minter).toEqual(vault.address); // jetton master == the vault
    expect(wd.code.hash()).toEqual(build("StakeReceiptWallet").hash());
}

get fun `test master implements TEP-89 wallet discovery (get_wallet_address + provide)`() {
    // TEP-89 is the back-reference indexers use to link holder wallets to a master.
    // `get_wallet_address(owner)` must equal the real receipt-wallet address, and a
    // `provide_wallet_address` must be answered with `take_wallet_address`.
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );

    // get-method back-reference matches the deterministic wallet address.
    val expected = vault.getReceiptWalletAddress(owner.address);
    expect(vault.getWalletAddress(owner.address)).toEqual(expected);

    // provide_wallet_address -> the master must answer with take_wallet_address
    // (0xd1735400) back to the requester. That reply is what an indexer uses to
    // confirm the wallet belongs to this master.
    val res = vault.sendProvideWalletAddress(
        owner.address,
        7,
        owner.address,
        true,
        { value: ton("0.1") },
    );
    expect(res).toHaveSuccessfulTx<ProvideWalletAddress>({ from: owner.address, to: vault.address });
    val reply = res.findTransaction<TakeWalletAddress>({ from: vault.address });
    expect(reply).toBeNotNull();
}

get fun `test stake deposit mints receipt 1to1 and a transfer attempt is rejected`() {
    // Explicit combined check (BLOCKCHAIN_ECONOMICS §8.5 / item 5): after a stake
    // deposit the StakeReceiptWallet is minted 1:1, and any transfer is rejected.
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );

    // 1:1 mint on deposit.
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });
    val wallet = receiptWalletOf(vault, owner.address);
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
    expect(wallet.isTransferLocked()).toBeTrue();

    // Transfer attempt is rejected and the balance is untouched (anti-exit).
    val res = wallet.sendJettonTransfer(
        owner.address,
        9,
        ton("100"),
        attacker.address,
        owner.address,
        null,
        ton("0"),
        beginCell().endCell().beginParse(),
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: owner.address, to: wallet.address, exitCode: ErrCode.RECEIPT_LOCKED });
    expect(wallet.getWalletData().balance).toEqual(ton("100"));
}

get fun `test free-tier vault (min_stake = 0) is eligible with zero stake`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    // A free-tier host needs no bond at all; it can still register a vault.
    val vault = deployVault(
        owner.address,
        ton("0"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
    );
    expect(vault.isEligible()).toBeTrue();

    // A paid-tier vault (min_stake = 10) is NOT eligible until it bonds enough.
    val paid = deployVault(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t2"),
        randomAddress("r2"),
    );
    expect(paid.isEligible()).toBeFalse();
}

// ===========================================================================
// Hardening: focused negative / boundary tests that kill surviving mutants
// (assert removal, comparison-boundary, logical-operator, slash-split guard).
// Each test pins a money-rule guard so a removed/weakened check fails CI.
// ===========================================================================

// Build a vault config with an explicit slash split + keeper bounty (so we can
// exercise the split-sum guard and the zero-stake eligibility boundary).
fun makeConfigSplit(
    minStake: coins,
    slasher: address,
    treasury: address,
    redundancy: address,
    splitCh: uint16,
    splitRe: uint16,
    splitBu: uint16,
    splitTr: uint16,
): Cell<VaultConfig> {
    val slash: SlashConfig = {
        treasury: treasury,
        redundancyPool: redundancy,
        splitChallengerBps: splitCh,
        splitRedundancyBps: splitRe,
        splitBurnBps: splitBu,
        splitTreasuryBps: splitTr,
    };
    val cfg: VaultConfig = {
        minStake: minStake,
        unbondingPeriod: UNBONDING_PERIOD,
        challengeWindow: CHALLENGE_WINDOW,
        keeperGrace: KEEPER_GRACE,
        keeperBountyBps: 200,
        slasher: slasher,
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    return cfg.toCell();
}

fun deployVaultSplit(
    owner: address,
    minStake: coins,
    slasher: address,
    treasury: address,
    redundancy: address,
    splitCh: uint16,
    splitRe: uint16,
    splitBu: uint16,
    splitTr: uint16,
): StakeVault {
    val storage: VaultStorage = {
        owner: owner,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: 0xDEADBEEF,
        config: makeConfigSplit(minStake, slasher, treasury, redundancy, splitCh, splitRe, splitBu, splitTr),
    };
    val vault = StakeVault.fromStorage(storage);
    expect(vault.deploy(owner, { value: ton("1") })).toHaveSuccessfulDeploy({ to: vault.address });
    return vault;
}

get fun `test deposit from a non-owner is rejected`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));

    val res = vault.sendStakeDeposit(attacker.address, 1, ton("100"), { value: ton("103") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.NOT_OWNER });
    expect(vault.getVaultState().staked).toEqual(ton("0"));
}

get fun `test deposit gas floor: under-funded rejected, exact floor accepted`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));

    // value == amount (no room for MIN_TONS_FOR_STORAGE) must be rejected. This
    // also pins the `+` in `amount + MIN_TONS_FOR_STORAGE` (a `-` would accept).
    val under = vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("100") });
    expect(under).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.NOT_ENOUGH_GAS });
    expect(vault.getVaultState().staked).toEqual(ton("0"));

    // value == amount + MIN_TONS_FOR_STORAGE EXACTLY must be accepted (pins
    // `>=`: a strict `>` would reject the exact floor).
    val exact = vault.sendStakeDeposit(owner.address, 2, ton("100"), { value: ton("100") + MIN_TONS_FOR_STORAGE });
    expect(exact).toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });
    expect(vault.getVaultState().staked).toEqual(ton("100"));
}

get fun `test eligibility boundary: staked == min_stake is eligible`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));

    // Bond EXACTLY the minimum: get_vault_state.eligible must be true (pins the
    // `>=` in `staked >= minStake`; a `>` would report not-eligible at the floor).
    vault.sendStakeDeposit(owner.address, 1, ton("10"), { value: ton("10.1") });
    val state = vault.getVaultState();
    expect(state.staked).toEqual(ton("10"));
    expect(state.eligible).toBeTrue();
}

get fun `test unbond guards: non-owner, zero, over-staked, and double unbond`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    // Non-owner cannot unbond.
    val notOwner = vault.sendStakeRequestUnbond(attacker.address, 2, ton("10"));
    expect(notOwner).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.NOT_OWNER });

    // amount == 0 rejected (pins `amount > 0`: `>=` or `&&`->`||` would accept).
    val zero = vault.sendStakeRequestUnbond(owner.address, 3, ton("0"));
    expect(zero).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.NOTHING_TO_UNBOND });

    // amount > staked rejected (pins `amount <= staked`).
    val over = vault.sendStakeRequestUnbond(owner.address, 4, ton("101"));
    expect(over).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.NOTHING_TO_UNBOND });

    // A second unbond while one is in progress is rejected (pins unbondingAt==0).
    val first = vault.sendStakeRequestUnbond(owner.address, 5, ton("40"));
    expect(first).toHaveSuccessfulTx<StakeRequestUnbond>({ from: owner.address, to: vault.address });
    val second = vault.sendStakeRequestUnbond(owner.address, 6, ton("10"));
    expect(second).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.UNBOND_IN_PROGRESS });
}

get fun `test withdraw guards: non-owner and nothing-unbonding rejected`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    // Withdraw with nothing unbonding is rejected (pins `unbondingAmount > 0`;
    // a `>=` would pass with 0).
    val nothing = vault.sendStakeWithdraw(owner.address, 2);
    expect(nothing).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.NO_UNBONDING });

    // With an unbond in progress, a non-owner withdraw is still rejected (owner
    // gate is checked first).
    vault.sendStakeRequestUnbond(owner.address, 3, ton("40"));
    val notOwner = vault.sendStakeWithdraw(attacker.address, 4);
    expect(notOwner).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.NOT_OWNER });
}

get fun `test keeper withdraw with nothing unbonding is rejected`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val keeper = randomAddress("keeper");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("103") });

    // No unbonding bucket: keeper is rejected with NO_UNBONDING (checked before
    // the time gate), pinning `unbondingAmount > 0`.
    testing.setNow(T0 + UNBONDING_PERIOD + KEEPER_GRACE + 1);
    val res = vault.sendStakeKeeperWithdraw(keeper, 2, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: keeper, to: vault.address, exitCode: ErrCode.NO_UNBONDING });
}

get fun `test slash guards: zero, over-slashable rejected; full slash accepted`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val challenger = randomAddress("challenger");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });

    // amount == 0 rejected (pins `amount > 0` and the `&&`).
    val zero = vault.sendStakeSlash(slasher.address, 2, ton("0"), 1, challenger, { value: ton("1") });
    expect(zero).toHaveFailedTx({ from: slasher.address, to: vault.address, exitCode: ErrCode.SLASH_EXCEEDS_STAKE });

    // amount > slashable rejected (pins `amount <= slashable`).
    val over = vault.sendStakeSlash(slasher.address, 3, ton("101"), 1, challenger, { value: ton("1") });
    expect(over).toHaveFailedTx({ from: slasher.address, to: vault.address, exitCode: ErrCode.SLASH_EXCEEDS_STAKE });

    // amount == slashable EXACTLY is accepted (pins `<=`: a strict `<` would
    // reject slashing the whole stake).
    val full = vault.sendStakeSlash(slasher.address, 4, ton("100"), 1, challenger, { value: ton("1") });
    expect(full).toHaveSuccessfulTx<StakeSlash>({ from: slasher.address, to: vault.address });
    val state = vault.getVaultState();
    expect(state.staked).toEqual(ton("0"));
    expect(state.totalSupply).toEqual(ton("0"));
}

get fun `test slash with a split that does not sum to 100% is rejected`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val challenger = randomAddress("challenger");
    // Split sums to 9000 bps, not 10000 -> the slash-split guard must reject.
    val vault = deployVaultSplit(
        owner.address,
        ton("10"),
        slasher.address,
        randomAddress("t"),
        randomAddress("r"),
        4000,
        3000,
        1000,
        1000,
    );
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });

    val res = vault.sendStakeSlash(slasher.address, 2, ton("50"), 1, challenger, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: slasher.address, to: vault.address, exitCode: ErrCode.INVALID_SPLIT });
    // Slash rolled back: stake untouched.
    expect(vault.getVaultState().staked).toEqual(ton("100"));
}

get fun `test keeper bounty math: owner receives amount minus bounty`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val keeper = randomAddress("keeper");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));
    vault.sendStakeDeposit(owner.address, 1, ton("100"), { value: ton("110") });
    vault.sendStakeRequestUnbond(owner.address, 2, ton("100"));

    testing.setNow(T0 + UNBONDING_PERIOD + KEEPER_GRACE);
    val ownerBefore = testing.getAccountBalance(owner.address);
    val res = vault.sendStakeKeeperWithdraw(keeper, 3, { value: ton("1") });
    expect(res).toHaveSuccessfulTx<StakeKeeperWithdraw>({ from: keeper, to: vault.address });

    // bounty = 2% of 100 = 2 TON; owner must receive amount - bounty = 98 TON
    // (minus negligible message fees). This pins the `-` in `amount - bounty`:
    // a `+` would pay ~102 (> 99) and a removed bounty deduction would pay ~100.
    val ownerDelta = testing.getAccountBalance(owner.address) - ownerBefore;
    expect(ownerDelta).toBeGreaterOrEqual(ton("97.5"));
    expect(ownerDelta).toBeLessOrEqual(ton("99"));
    expect(testing.getAccountBalance(keeper)).toBeGreaterOrEqual(ton("2"));
}

get fun `test unknown opcode (non-empty body) is rejected with INVALID_OP`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));

    // A non-empty body with an unrecognized 32-bit opcode hits the `else` arm,
    // which asserts the body is empty -> ErrCode.INVALID_OP (pins that guard).
    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = vault.sendAny(owner.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test vault getters, top-up and provide-without-include cover read paths`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVault(owner.address, ton("10"), slasher.address, randomAddress("t"), randomAddress("r"));

    // Read-only getters (owner, binding hash, deterministic receipt address).
    expect(vault.getOwner()).toEqual(owner.address);
    expect(vault.getBindingHash()).toEqual(0xDEADBEEF);
    expect(vault.getReceiptWalletAddress(owner.address)).toEqual(vault.getWalletAddress(owner.address));

    // A plain top-up for storage rent is accepted.
    val top = vault.sendReceiptTopUp(owner.address, 1, { value: ton("0.1") });
    expect(top).toHaveSuccessfulTx<ReceiptTopUp>({ from: owner.address, to: vault.address });

    // provide_wallet_address WITHOUT include_address still answers (covers the
    // `ownerRef = null` branch of the TEP-89 reply).
    val res = vault.sendProvideWalletAddress(owner.address, 2, owner.address, false, { value: ton("0.1") });
    expect(res).toHaveSuccessfulTx<ProvideWalletAddress>({ from: owner.address, to: vault.address });
    expect(res.findTransaction<TakeWalletAddress>({ from: vault.address })).toBeNotNull();
}
