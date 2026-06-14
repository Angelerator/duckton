// In-place code upgradeability tests (BLOCKCHAIN_ECONOMICS §8.6, §12.1).
//
// Proves, in the Acton emulator, that each upgradable contract can have its CODE
// replaced via TVM `SETCODE` (`contract.setCodePostponed`) WITHOUT changing the
// contract address or losing storage, gated to the right authority, and (for the
// bond-custody StakeVault) only after a timelock so stakers can exit first:
//
//   * GlobalParams  — admin-gated `upgrade_code`        (the primary win)
//   * RecordAnchor  — authority-gated `upgrade_code`
//   * StakeVault    — governance-gated + TIMELOCKED announce -> apply
//
// "New code is live" is proven by calling a getter that exists ONLY in the v2
// target (get_surcharge_bps / get_verdict_authority / get_slasher), at the SAME
// address, over the SAME (preserved) storage, with codeVersion bumped 0 -> 1.

import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/common"
import "@contracts/global_params_types"
import "@contracts/anchor_types"
import "@contracts/stake_types"
import "@wrappers/GlobalParams.gen"
import "@wrappers/RecordAnchor.gen"
import "@wrappers/StakeVault.gen"

const T0: uint32 = 1_700_000_000
const VAULT_UNBONDING: uint32 = 1000
const VAULT_CHALLENGE: uint32 = 500

// ---------------------------------------------------------------------------
// GlobalParams (admin-gated in-place upgrade) — the primary win.
// ---------------------------------------------------------------------------

fun validParams(): EcoParams {
    return {
        platformFeeBps: 200,
        surchargeBps: 500,
        participationCommissionBps: 200,
        slashWrongBps: 1500,
        slashCheatBps: 10000,
        slashDowntimeBps: 200,
        slashEquivocationBps: 5000,
        splitChallengerBps: 4000,
        splitRedundancyBps: 3000,
        splitBurnBps: 2000,
        splitTreasuryBps: 1000,
        minStake: ton("0"),
        minStakeInternal: ton("100"),
        minStakeSensitive: ton("1000"),
        stakeCap: ton("100000"),
        unbondingSecs: 604800,
        challengeWindowSecs: 86400,
        nPublic: 3,
        nDefault: 5,
        nMax: 10,
        quorum: 3,
        checksumMin: 3,
        wQualityBps: 6000,
        wStakeBps: 1500,
        wPriceBps: 2500,
        slashFailedCommitmentBps: 1000,
        attemptDeadlineMs: 60000,
        progressIntervalMs: 2000,
        progressStallMult: 5,
    };
}

fun deployGlobalParams(admin: address): GlobalParams {
    val storage: GlobalParamsStorage = {
        admin: admin,
        feeRecipient: admin,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
        paramsVersion: 1,
        codeVersion: 0,
    };
    val gp = GlobalParams.fromStorage(storage);
    expect(gp.deploy(admin, { value: ton("1") })).toHaveSuccessfulDeploy({ to: gp.address });
    return gp;
}

get fun `test global params upgrade_code swaps code in place (address stable, state preserved, codeVersion bumped, new getter live)`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address);
    val addrBefore = gp.address;

    // Pre-upgrade snapshot: original code (v0), params version 1, fee 200.
    expect(gp.getCodeVersion()).toEqual(0);
    expect(gp.getParamsVersion()).toEqual(1);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);

    // Admin upgrades the CODE in place to GlobalParamsV2 (adds get_surcharge_bps).
    val res = gp.sendUpgradeCode(admin.address, 1, build("GlobalParamsV2"), { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<UpgradeCode>({ from: admin.address, to: gp.address });

    // SAME address, storage preserved, codeVersion bumped 0 -> 1.
    expect(gp.address).toEqual(addrBefore);
    expect(gp.getCodeVersion()).toEqual(1);
    expect(gp.getParamsVersion()).toEqual(1);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);
    expect(gp.getAdmin()).toEqual(admin.address);

    // New code is LIVE: get_surcharge_bps exists only in v2, reads stored 500.
    expect(net.runGetMethod<int>(gp.address, "get_surcharge_bps")).toEqual(500);

    // v2 handlers still work in place: an admin update_params bumps the version.
    var p = validParams();
    p.platformFeeBps = 333;
    val upd = gp.sendUpdateParams(admin.address, 2, admin.address, p, { value: ton("0.5") });
    expect(upd).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getParamsVersion()).toEqual(2);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(333);
    // still v2 after a mutation: surcharge getter remains live.
    expect(net.runGetMethod<int>(gp.address, "get_surcharge_bps")).toEqual(500);
}

get fun `test global params upgrade_code from a non-admin is rejected (NOT_ADMIN), code unchanged`() {
    val admin = testing.treasury("admin");
    val attacker = testing.treasury("attacker");
    val gp = deployGlobalParams(admin.address);

    val res = gp.sendUpgradeCode(attacker.address, 1, build("GlobalParamsV2"), { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });

    // Code NOT swapped: still v0, original params intact.
    expect(gp.getCodeVersion()).toEqual(0);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);
}

// ---------------------------------------------------------------------------
// RecordAnchor (authority-gated in-place upgrade).
// ---------------------------------------------------------------------------

fun deployAnchor(authority: address): RecordAnchor {
    val cfg: AnchorConfig = {
        minStakeWeight: ton("100"),
        disputeBondMin: ton("1"),
        verdictAuthority: authority,
        treasury: authority,
    };
    val storage: AnchorStorage = {
        currentEpoch: 0,
        lastRoot: 0,
        nextDisputeId: 0,
        disputes: createEmptyMap(),
        config: cfg.toCell(),
        codeVersion: 0,
    };
    val anchor = RecordAnchor.fromStorage(storage);
    expect(anchor.deploy(authority, { value: ton("1") })).toHaveSuccessfulDeploy({ to: anchor.address });
    return anchor;
}

get fun `test record anchor upgrade_code (authority-gated) swaps code in place, epoch chain preserved`() {
    val authority = testing.treasury("authority");
    val anchor = deployAnchor(authority.address);
    val addrBefore = anchor.address;

    // Anchor a root so the upgrade has real state to preserve.
    val sub = anchor.sendAnchorSubmitRoot(authority.address, 1, 1, 0xAAAA, 0, ton("100"), { value: ton("0.1") });
    expect(sub).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: authority.address, to: anchor.address });
    val st = anchor.getAnchorState();
    expect(anchor.getCodeVersion()).toEqual(0);

    // Authority upgrades the CODE in place to RecordAnchorV2.
    val res = anchor.sendAnchorUpgradeCode(authority.address, 2, build("RecordAnchorV2"), { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<AnchorUpgradeCode>({ from: authority.address, to: anchor.address });

    // SAME address, anchored epoch chain preserved, codeVersion bumped.
    expect(anchor.address).toEqual(addrBefore);
    expect(anchor.getCodeVersion()).toEqual(1);
    val st2 = anchor.getAnchorState();
    expect(st2.currentEpoch).toEqual(st.currentEpoch);
    expect(st2.lastRoot).toEqual(st.lastRoot);

    // New code LIVE: get_verdict_authority exists only in v2.
    expect(net.runGetMethod<address>(anchor.address, "get_verdict_authority")).toEqual(authority.address);
}

get fun `test record anchor upgrade_code from a non-authority is rejected (NOT_VERDICT_AUTHORITY)`() {
    val authority = testing.treasury("authority");
    val attacker = testing.treasury("attacker");
    val anchor = deployAnchor(authority.address);

    val res = anchor.sendAnchorUpgradeCode(attacker.address, 9, build("RecordAnchorV2"), { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: anchor.address, exitCode: ErrCode.NOT_VERDICT_AUTHORITY });
    expect(anchor.getCodeVersion()).toEqual(0);
}

// ---------------------------------------------------------------------------
// StakeVault (governance-gated + TIMELOCKED announce -> apply) — the delicate
// one: an arbitrary code-swap would break the non-custodial guarantee, so apply
// is only allowed after a delay >= the unbonding window (exit window for stakers).
// ---------------------------------------------------------------------------

fun deployVault(owner: address, governance: address): StakeVault {
    val slash: SlashConfig = {
        treasury: owner,
        redundancyPool: owner,
        splitChallengerBps: 4000,
        splitRedundancyBps: 3000,
        splitBurnBps: 2000,
        splitTreasuryBps: 1000,
    };
    val cfg: VaultConfig = {
        minStake: ton("0"),
        unbondingPeriod: VAULT_UNBONDING,
        challengeWindow: VAULT_CHALLENGE,
        keeperGrace: 100,
        keeperBountyBps: 200,
        slasher: governance, // the slasher == governance authority gating upgrades
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    val storage: VaultStorage = {
        owner: owner,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: 0xDEADBEEF,
        config: cfg.toCell(),
        upgrade: freshVaultUpgrade(),
    };
    val vault = StakeVault.fromStorage(storage);
    expect(vault.deploy(owner, { value: ton("1") })).toHaveSuccessfulDeploy({ to: vault.address });
    return vault;
}

get fun `test stake vault timelocked upgrade: announce, apply-before-delay rejected, apply-after-delay succeeds (address stable, bonds preserved, new getter live)`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val vault = deployVault(owner.address, gov.address);
    val addrBefore = vault.address;

    // Bond some stake so the upgrade has real custody state to preserve.
    val dep = vault.sendStakeDeposit(owner.address, 1, ton("5"), { value: ton("5.5") });
    expect(dep).toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });
    val stakedBefore = vault.getVaultState().staked;
    expect(stakedBefore).toEqual(ton("5"));
    expect(vault.getCodeVersion()).toEqual(0);

    val newCode = build("StakeVaultV2");
    val newHash = newCode.hash();

    // Step 1 — ANNOUNCE (governance-gated): records the successor hash + clock.
    val ann = vault.sendStakeAnnounceUpgrade(gov.address, 10, newHash, { value: ton("0.1") });
    expect(ann).toHaveSuccessfulTx<StakeAnnounceUpgrade>({ from: gov.address, to: vault.address });
    val pend = vault.getPendingUpgrade();
    expect(pend.pendingCodeHash).toEqual(newHash);
    expect(pend.pendingAt).toEqual(T0);
    // Exit window: apply not allowed until announce + unbondingPeriod.
    expect(pend.readyAt).toEqual(T0 + VAULT_UNBONDING);
    expect(vault.getCodeVersion()).toEqual(0);

    // Step 2 too early — APPLY before the timelock elapses is REJECTED.
    val early = vault.sendStakeApplyUpgrade(gov.address, 11, newCode, { value: ton("0.5") });
    expect(early).toHaveFailedTx({ from: gov.address, to: vault.address, exitCode: ErrCode.UPGRADE_TIMELOCK_ACTIVE });
    expect(vault.getCodeVersion()).toEqual(0);

    // Warp past the timelock (stakers have had >= the unbonding window to exit).
    testing.setNow(T0 + VAULT_UNBONDING);

    // Step 2 — APPLY succeeds now.
    val applied = vault.sendStakeApplyUpgrade(gov.address, 12, newCode, { value: ton("0.5") });
    expect(applied).toHaveSuccessfulTx<StakeApplyUpgrade>({ from: gov.address, to: vault.address });

    // SAME address, bonds preserved, codeVersion bumped, pending cleared.
    expect(vault.address).toEqual(addrBefore);
    expect(vault.getCodeVersion()).toEqual(1);
    expect(vault.getVaultState().staked).toEqual(stakedBefore);
    expect(vault.getPendingUpgrade().pendingCodeHash).toEqual(0);

    // New code LIVE: get_slasher exists only in v2.
    expect(net.runGetMethod<address>(vault.address, "get_slasher")).toEqual(gov.address);
}

get fun `test stake vault upgrade announce/apply from non-governance is rejected (NOT_SLASHER)`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(owner.address, gov.address);

    val newHash = build("StakeVaultV2").hash();
    val ann = vault.sendStakeAnnounceUpgrade(attacker.address, 1, newHash, { value: ton("0.1") });
    expect(ann).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.NOT_SLASHER });
    expect(vault.getPendingUpgrade().pendingCodeHash).toEqual(0);

    val app = vault.sendStakeApplyUpgrade(attacker.address, 2, build("StakeVaultV2"), { value: ton("0.5") });
    expect(app).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.NOT_SLASHER });
    expect(vault.getCodeVersion()).toEqual(0);
}

get fun `test stake vault apply guards: nothing-pending, code-mismatch rejected; cancel clears the announcement`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val vault = deployVault(owner.address, gov.address);

    // Apply with nothing announced -> NO_PENDING_UPGRADE.
    val noPend = vault.sendStakeApplyUpgrade(gov.address, 1, build("StakeVaultV2"), { value: ton("0.5") });
    expect(noPend).toHaveFailedTx({ from: gov.address, to: vault.address, exitCode: ErrCode.NO_PENDING_UPGRADE });

    // Announce the v2 hash, warp past the delay, then apply the WRONG code.
    expect(vault.sendStakeAnnounceUpgrade(gov.address, 2, build("StakeVaultV2").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<StakeAnnounceUpgrade>({ from: gov.address, to: vault.address });
    testing.setNow(T0 + VAULT_UNBONDING);
    // build("StakeVault") (v1) hashes differently than the announced v2 hash.
    val mism = vault.sendStakeApplyUpgrade(gov.address, 3, build("StakeVault"), { value: ton("0.5") });
    expect(mism).toHaveFailedTx({ from: gov.address, to: vault.address, exitCode: ErrCode.UPGRADE_CODE_MISMATCH });
    // Pending still set (apply failed), code unchanged.
    expect(vault.getPendingUpgrade().pendingCodeHash).toEqual(build("StakeVaultV2").hash());
    expect(vault.getCodeVersion()).toEqual(0);

    // Cancel clears the announcement (governance safety valve).
    val canc = vault.sendStakeCancelUpgrade(gov.address, 4, { value: ton("0.05") });
    expect(canc).toHaveSuccessfulTx<StakeCancelUpgrade>({ from: gov.address, to: vault.address });
    expect(vault.getPendingUpgrade().pendingCodeHash).toEqual(0);

    // Cancel again with nothing pending -> NO_PENDING_UPGRADE.
    val canc2 = vault.sendStakeCancelUpgrade(gov.address, 5, { value: ton("0.05") });
    expect(canc2).toHaveFailedTx({ from: gov.address, to: vault.address, exitCode: ErrCode.NO_PENDING_UPGRADE });
}
