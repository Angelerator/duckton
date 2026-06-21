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
const ANCHOR_UPGRADE_DELAY: uint32 = 86400

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

const GP_UPGRADE_DELAY: uint32 = 86400;

fun deployGlobalParams(admin: address): GlobalParams {
    val storage: GlobalParamsStorage = {
        admin: admin,
        feeRecipient: admin,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
        paramsVersion: 1,
        codeVersion: 0,
        upgradeDelay: GP_UPGRADE_DELAY,
        pendingCodeHash: 0,
        pendingCodeAt: 0,
    };
    val gp = GlobalParams.fromStorage(storage);
    expect(gp.deploy(admin, { value: ton("1") })).toHaveSuccessfulDeploy({ to: gp.address });
    return gp;
}

get fun `test global params upgrade_code swaps code in place (address stable, state preserved, codeVersion bumped, new getter live)`() {
    testing.setNow(T0);
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address);
    val addrBefore = gp.address;

    // Pre-upgrade snapshot: original code (v0), params version 1, fee 200.
    expect(gp.getCodeVersion()).toEqual(0);
    expect(gp.getParamsVersion()).toEqual(1);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);

    val newCode = build("GlobalParamsV2");
    // D2 step 1 — ANNOUNCE the successor code hash + start the timelock clock.
    val ann = gp.sendAnnounceCode(admin.address, 1, newCode.hash(), { value: ton("0.1") });
    expect(ann).toHaveSuccessfulTx<AnnounceCode>({ from: admin.address, to: gp.address });
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(newCode.hash());
    expect(gp.getPendingUpgrade().readyAt).toEqual(T0 + GP_UPGRADE_DELAY);

    // D2: apply BEFORE the delay is rejected (timelock active).
    val early = gp.sendUpgradeCode(admin.address, 2, newCode, { value: ton("0.5") });
    expect(early).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.UPGRADE_TIMELOCK_ACTIVE });
    expect(gp.getCodeVersion()).toEqual(0);

    // Warp past the timelock, then APPLY (step 2) the in-place upgrade.
    testing.setNow(T0 + GP_UPGRADE_DELAY);
    val res = gp.sendUpgradeCode(admin.address, 3, newCode, { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<UpgradeCode>({ from: admin.address, to: gp.address });

    // SAME address, storage preserved, codeVersion bumped 0 -> 1, pending cleared.
    expect(gp.address).toEqual(addrBefore);
    expect(gp.getCodeVersion()).toEqual(1);
    expect(gp.getParamsVersion()).toEqual(1);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);
    expect(gp.getAdmin()).toEqual(admin.address);
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(0);

    // New code is LIVE: get_surcharge_bps exists only in v2, reads stored 500.
    expect(net.runGetMethod<int>(gp.address, "get_surcharge_bps")).toEqual(500);

    // v2 handlers still work in place: an admin update_params bumps the version.
    var p = validParams();
    p.platformFeeBps = 333;
    val upd = gp.sendUpdateParams(admin.address, 4, admin.address, p, { value: ton("0.5") });
    expect(upd).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getParamsVersion()).toEqual(2);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(333);
    // still v2 after a mutation: surcharge getter remains live.
    expect(net.runGetMethod<int>(gp.address, "get_surcharge_bps")).toEqual(500);

    // Exercise the v2 admin / blocklist / announce-cancel handlers in place.
    val subj = 0xABCD;
    gp.sendSetBlocklisted(admin.address, 5, subj, true, { value: ton("0.1") });
    expect(gp.getBlocklisted(subj)).toEqual(true);
    gp.sendUpdateAdmin(admin.address, 6, admin.address, { value: ton("0.1") });
    expect(gp.getAdmin()).toEqual(admin.address);
    // Announce a (v2->v3-style) upgrade then cancel it — covers v2 announce/cancel.
    expect(gp.sendAnnounceCode(admin.address, 7, build("GlobalParams").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<AnnounceCode>({ from: admin.address, to: gp.address });
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(build("GlobalParams").hash());
    expect(gp.sendCancelCode(admin.address, 8, { value: ton("0.05") }))
        .toHaveSuccessfulTx<CancelCode>({ from: admin.address, to: gp.address });
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(0);
}

get fun `test global params upgrade announce/apply from a non-admin is rejected (NOT_ADMIN), code unchanged`() {
    testing.setNow(T0);
    val admin = testing.treasury("admin");
    val attacker = testing.treasury("attacker");
    val gp = deployGlobalParams(admin.address);

    // Non-admin cannot announce.
    val ann = gp.sendAnnounceCode(attacker.address, 1, build("GlobalParamsV2").hash(), { value: ton("0.1") });
    expect(ann).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(0);

    // Even with a legit announcement, a non-admin cannot apply.
    expect(gp.sendAnnounceCode(admin.address, 2, build("GlobalParamsV2").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<AnnounceCode>({ from: admin.address, to: gp.address });
    testing.setNow(T0 + GP_UPGRADE_DELAY);
    val res = gp.sendUpgradeCode(attacker.address, 3, build("GlobalParamsV2"), { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });

    // Code NOT swapped: still v0, original params intact.
    expect(gp.getCodeVersion()).toEqual(0);
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);
}

get fun `test global params upgrade apply guards: nothing-pending, code-mismatch rejected; cancel clears`() {
    testing.setNow(T0);
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address);

    // Apply with nothing announced -> NO_PENDING_UPGRADE.
    val noPend = gp.sendUpgradeCode(admin.address, 1, build("GlobalParamsV2"), { value: ton("0.5") });
    expect(noPend).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.NO_PENDING_UPGRADE });

    // Announce the v2 hash, warp, then apply the WRONG code (v1 hashes differently).
    expect(gp.sendAnnounceCode(admin.address, 2, build("GlobalParamsV2").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<AnnounceCode>({ from: admin.address, to: gp.address });
    testing.setNow(T0 + GP_UPGRADE_DELAY);
    val mism = gp.sendUpgradeCode(admin.address, 3, build("GlobalParams"), { value: ton("0.5") });
    expect(mism).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.UPGRADE_CODE_MISMATCH });
    expect(gp.getCodeVersion()).toEqual(0);

    // Cancel clears the announcement; a second cancel is a no-pending error.
    expect(gp.sendCancelCode(admin.address, 4, { value: ton("0.05") }))
        .toHaveSuccessfulTx<CancelCode>({ from: admin.address, to: gp.address });
    expect(gp.getPendingUpgrade().pendingCodeHash).toEqual(0);
    expect(gp.sendCancelCode(admin.address, 5, { value: ton("0.05") }))
        .toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.NO_PENDING_UPGRADE });
}

// ---------------------------------------------------------------------------
// RecordAnchor (authority-gated in-place upgrade).
// ---------------------------------------------------------------------------

// `authority` here is BOTH the verdict authority and (for these tests) the
// keeper + upgrade authority, so it can submit a root and drive the upgrade.
fun deployAnchor(authority: address): RecordAnchor {
    val auth: AnchorAuthorities = {
        treasury: authority,
        keeper: authority,
        upgradeAuthority: authority,
    };
    val cfg: AnchorConfig = {
        minStakeWeight: ton("100"),
        disputeBondMin: ton("1"),
        verdictAuthority: authority,
        auth: auth.toCell(),
    };
    val storage: AnchorStorage = {
        currentEpoch: 0,
        lastRoot: 0,
        nextDisputeId: 0,
        disputes: createEmptyMap(),
        roots: createEmptyMap(),
        config: cfg.toCell(),
        codeVersion: 0,
        pendingCodeHash: 0,
        pendingCodeAt: 0,
        upgradeDelay: ANCHOR_UPGRADE_DELAY,
    };
    val anchor = RecordAnchor.fromStorage(storage);
    expect(anchor.deploy(authority, { value: ton("1") })).toHaveSuccessfulDeploy({ to: anchor.address });
    return anchor;
}

get fun `test record anchor timelocked upgrade: announce, apply-before-delay rejected, apply-after-delay succeeds, epoch chain preserved`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val anchor = deployAnchor(authority.address);
    val addrBefore = anchor.address;

    // Anchor a root so the upgrade has real state to preserve.
    val sub = anchor.sendAnchorSubmitRoot(authority.address, 1, 1, 0xAAAA, 0, ton("100"), { value: ton("0.1") });
    expect(sub).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: authority.address, to: anchor.address });
    val st = anchor.getAnchorState();
    expect(anchor.getCodeVersion()).toEqual(0);

    val newCode = build("RecordAnchorV2");
    // D2 step 1 — ANNOUNCE the successor code hash + start the timelock clock.
    val ann = anchor.sendAnchorAnnounceUpgrade(authority.address, 2, newCode.hash(), { value: ton("0.1") });
    expect(ann).toHaveSuccessfulTx<AnchorAnnounceUpgrade>({ from: authority.address, to: anchor.address });
    expect(anchor.getPendingUpgrade().pendingCodeHash).toEqual(newCode.hash());
    expect(anchor.getPendingUpgrade().readyAt).toEqual(T0 + ANCHOR_UPGRADE_DELAY);

    // D2: apply BEFORE the delay is rejected (timelock active).
    val early = anchor.sendAnchorUpgradeCode(authority.address, 3, newCode, { value: ton("0.5") });
    expect(early).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.UPGRADE_TIMELOCK_ACTIVE });
    expect(anchor.getCodeVersion()).toEqual(0);

    // Warp past the timelock, then APPLY (step 2) the in-place upgrade.
    testing.setNow(T0 + ANCHOR_UPGRADE_DELAY);
    val res = anchor.sendAnchorUpgradeCode(authority.address, 4, newCode, { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<AnchorUpgradeCode>({ from: authority.address, to: anchor.address });

    // SAME address, anchored epoch chain preserved, codeVersion bumped, pending cleared.
    expect(anchor.address).toEqual(addrBefore);
    expect(anchor.getCodeVersion()).toEqual(1);
    expect(anchor.getPendingUpgrade().pendingCodeHash).toEqual(0);
    val st2 = anchor.getAnchorState();
    expect(st2.currentEpoch).toEqual(st.currentEpoch);
    expect(st2.lastRoot).toEqual(st.lastRoot);

    // New code LIVE: get_verdict_authority exists only in v2.
    expect(net.runGetMethod<address>(anchor.address, "get_verdict_authority")).toEqual(authority.address);

    // The UPGRADED (v2) handlers still execute: anchor a NEW epoch (chained to the
    // preserved epoch-1 root) via v2, then open + resolve a bonded dispute against
    // it — exercising the v2 submit / open-dispute / resolve / bond-refund paths.
    // A single-leaf tree's root is hashLeaf(leaf) (MERKLE v2).
    val leaf2 = 0x1234;
    val root2 = beginCell().storeUint(leaf2, 256).endCell().hash(); // == hashLeaf(leaf2)
    val sub2 = anchor.sendAnchorSubmitRoot(authority.address, 30, 2, root2, 0xAAAA, ton("100"), { value: ton("0.1") });
    expect(sub2).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: authority.address, to: anchor.address });

    val challenger = testing.treasury("v2chal");
    val open2 = anchor.sendAnchorOpenDispute(challenger.address, 31, 2, leaf2, null, { value: ton("2") });
    expect(open2).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger.address, to: anchor.address });
    val resolve2 = anchor.sendAnchorResolveDispute(authority.address, 32, 0, true, { value: ton("1") });
    expect(resolve2).toHaveSuccessfulTx<AnchorResolveDispute>({ from: authority.address, to: anchor.address });
    expect(anchor.getDispute(0)!.status).toEqual(1);
}

get fun `test record anchor upgrade_code from a non-authority is rejected (UPGRADE_NOT_AUTHORITY)`() {
    val authority = testing.treasury("authority");
    val attacker = testing.treasury("attacker");
    val anchor = deployAnchor(authority.address);

    // D2: upgrades are gated to the upgrade authority (distinct from the verdict
    // authority); a non-authority is rejected with UPGRADE_NOT_AUTHORITY.
    val res = anchor.sendAnchorUpgradeCode(attacker.address, 9, build("RecordAnchorV2"), { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: anchor.address, exitCode: ErrCode.UPGRADE_NOT_AUTHORITY });
    expect(anchor.getCodeVersion()).toEqual(0);
}

get fun `test record anchor upgrade apply guards: nothing-pending, code-mismatch rejected; cancel clears`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val anchor = deployAnchor(authority.address);

    // Apply with nothing announced -> NO_PENDING_UPGRADE.
    val noPend = anchor.sendAnchorUpgradeCode(authority.address, 1, build("RecordAnchorV2"), { value: ton("0.5") });
    expect(noPend).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.NO_PENDING_UPGRADE });

    // Announce the v2 hash, warp, then apply the WRONG code (v1 hashes differently).
    expect(anchor.sendAnchorAnnounceUpgrade(authority.address, 2, build("RecordAnchorV2").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<AnchorAnnounceUpgrade>({ from: authority.address, to: anchor.address });
    testing.setNow(T0 + ANCHOR_UPGRADE_DELAY);
    val mism = anchor.sendAnchorUpgradeCode(authority.address, 3, build("RecordAnchor"), { value: ton("0.5") });
    expect(mism).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.UPGRADE_CODE_MISMATCH });
    expect(anchor.getCodeVersion()).toEqual(0);

    // Cancel clears the announcement; a second cancel is a no-pending error.
    expect(anchor.sendAnchorCancelUpgrade(authority.address, 4, { value: ton("0.05") }))
        .toHaveSuccessfulTx<AnchorCancelUpgrade>({ from: authority.address, to: anchor.address });
    expect(anchor.getPendingUpgrade().pendingCodeHash).toEqual(0);
    expect(anchor.sendAnchorCancelUpgrade(authority.address, 5, { value: ton("0.05") }))
        .toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.NO_PENDING_UPGRADE });
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
        slasher: governance,
        // D1: a DISTINCT upgrade authority gates code upgrades (here = governance).
        upgradeAuthority: governance,
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
        pending: createEmptyMap(),
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

    // The UPGRADED (v2) handlers still execute correctly over the preserved
    // bonds: a fresh deposit credits 1:1, and a governance slash burns + splits
    // and reduces the bond — exercising the v2 deposit/slash/payout paths.
    val depV2 = vault.sendStakeDeposit(owner.address, 20, ton("10"), { value: ton("11") });
    expect(depV2).toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });
    expect(vault.getVaultState().staked).toEqual(ton("15")); // 5 preserved + 10 new

    val chal = testing.treasury("v2chal");
    val slashV2 = vault.sendStakeSlash(gov.address, 21, ton("6"), 1, chal.address, { value: ton("1") });
    expect(slashV2).toHaveSuccessfulTx<StakeSlash>({ from: gov.address, to: vault.address });
    expect(vault.getVaultState().staked).toEqual(ton("9"));

    // A v2 unbond → keeper-withdraw round trip exercises the v2 reserve+carry path.
    val unbV2 = vault.sendStakeRequestUnbond(owner.address, 22, ton("9"));
    expect(unbV2).toHaveSuccessfulTx<StakeRequestUnbond>({ from: owner.address, to: vault.address });
    testing.setNow(T0 + VAULT_UNBONDING + VAULT_UNBONDING);
    val wV2 = vault.sendStakeWithdraw(owner.address, 23);
    expect(wV2).toHaveSuccessfulTx<StakeWithdraw>({ from: owner.address, to: vault.address });
    expect(vault.getVaultState().unbondingAmount).toEqual(ton("0"));
}

get fun `test stake vault upgrade announce/apply from non-governance is rejected (UPGRADE_NOT_AUTHORITY)`() {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val attacker = testing.treasury("attacker");
    val vault = deployVault(owner.address, gov.address);

    val newHash = build("StakeVaultV2").hash();
    // D1: upgrades are gated to the upgrade authority, separated from the slasher.
    val ann = vault.sendStakeAnnounceUpgrade(attacker.address, 1, newHash, { value: ton("0.1") });
    expect(ann).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.UPGRADE_NOT_AUTHORITY });
    expect(vault.getPendingUpgrade().pendingCodeHash).toEqual(0);

    val app = vault.sendStakeApplyUpgrade(attacker.address, 2, build("StakeVaultV2"), { value: ton("0.5") });
    expect(app).toHaveFailedTx({ from: attacker.address, to: vault.address, exitCode: ErrCode.UPGRADE_NOT_AUTHORITY });
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

// Drive the remaining v2 fund-custody handlers after an in-place upgrade: keeper
// withdraw, the bounce → pull-ledger → claim path, provide-wallet, and top-up.
fun upgradeVaultToV2(owner: address, gov: address): StakeVault {
    testing.setNow(T0);
    val vault = deployVault(owner, gov);
    // Fund the vault with rent headroom up front (production vaults are funded),
    // so the post-upgrade v2 mint/payout storage phases are comfortable.
    expect(vault.sendReceiptTopUp(owner, 1, { value: ton("2") }))
        .toHaveSuccessfulTx<ReceiptTopUp>({ from: owner, to: vault.address });
    expect(vault.sendStakeAnnounceUpgrade(gov, 2, build("StakeVaultV2").hash(), { value: ton("0.1") }))
        .toHaveSuccessfulTx<StakeAnnounceUpgrade>({ from: gov, to: vault.address });
    testing.setNow(T0 + VAULT_UNBONDING);
    expect(vault.sendStakeApplyUpgrade(gov, 3, build("StakeVaultV2"), { value: ton("0.5") }))
        .toHaveSuccessfulTx<StakeApplyUpgrade>({ from: gov, to: vault.address });
    expect(vault.getCodeVersion()).toEqual(1);
    return vault;
}

get fun `test v2 vault: bounced-payout claim, provide-wallet and top-up`() {
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val vault = upgradeVaultToV2(owner.address, gov.address);

    // Slash to an UNINIT recipient on v2 → bounces → pull-ledger → claim drains it.
    expect(vault.sendStakeDeposit(owner.address, 13, ton("10"), { value: ton("11") }))
        .toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });
    val ghost = randomAddress("ghost_challenger");
    expect(vault.sendStakeSlash(gov.address, 14, ton("10"), 1, ghost, { value: ton("1") }))
        .toHaveSuccessfulTx<StakeSlash>({ from: gov.address, to: vault.address });
    // 40% challenger share to the uninit ghost bounced into the pull-ledger.
    expect(vault.getPending(ghost)).toBeGreaterOrEqual(ton("3.9"));
    expect(vault.sendStakeClaim(owner.address, 15, ghost, { value: ton("1") }))
        .toHaveSuccessfulTx<StakeClaim>({ from: owner.address, to: vault.address });

    // v2 provide-wallet (TEP-89) + top-up read/accept paths.
    expect(vault.sendProvideWalletAddress(owner.address, 16, owner.address, true, { value: ton("0.1") }))
        .toHaveSuccessfulTx<ProvideWalletAddress>({ from: owner.address, to: vault.address });
    expect(vault.sendReceiptTopUp(owner.address, 17, { value: ton("0.1") }))
        .toHaveSuccessfulTx<ReceiptTopUp>({ from: owner.address, to: vault.address });
}

get fun `test v2 vault: keeper withdraw after cooldown + grace`() {
    val owner = testing.treasury("owner");
    val gov = testing.treasury("gov");
    val keeper = testing.treasury("keeper");
    val vault = upgradeVaultToV2(owner.address, gov.address);
    val base = T0 + VAULT_UNBONDING;

    expect(vault.sendStakeDeposit(owner.address, 10, ton("100"), { value: ton("101") }))
        .toHaveSuccessfulTx<StakeDeposit>({ from: owner.address, to: vault.address });
    expect(vault.sendStakeRequestUnbond(owner.address, 11, ton("100")))
        .toHaveSuccessfulTx<StakeRequestUnbond>({ from: owner.address, to: vault.address });
    testing.setNow(base + VAULT_UNBONDING + 100); // cooldown + keeperGrace(100)
    // Before grace, too early.
    expect(vault.getVaultState().unbondingAmount).toEqual(ton("100"));
    // After cooldown + grace, the keeper can finalize on v2 (exercises the v2
    // keeper-withdraw reserve+carry + bounty payout path); state is cleared and
    // the receipt fully burned.
    expect(vault.sendStakeKeeperWithdraw(keeper.address, 12, { value: ton("1") }))
        .toHaveSuccessfulTx<StakeKeeperWithdraw>({ from: keeper.address, to: vault.address });
    expect(vault.getVaultState().unbondingAmount).toEqual(ton("0"));
    expect(vault.getVaultState().totalSupply).toEqual(ton("0"));
    // The keeper's bounty was delivered (not stuck in the pull-ledger).
    expect(vault.getPending(keeper.address)).toEqual(0);
}
