import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

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
    ).toHaveFailedTx({ from: owner.address, to: wallet.address, exitCode: ERROR_RECEIPT_LOCKED });
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
    ).toHaveFailedTx({ from: notSlasher.address, to: vault.address, exitCode: ERROR_NOT_SLASHER });
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
    ).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ERROR_COOLDOWN_NOT_ELAPSED });

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
    ).toHaveFailedTx({ from: keeper, to: vault.address, exitCode: ERROR_KEEPER_TOO_EARLY });

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
