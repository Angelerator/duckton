import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/common"
import "@contracts/global_params_types"
import "@wrappers/GlobalParams.gen"

// A valid set of platform-wide economic parameters (mirrors the §12 defaults).
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

fun deployGlobalParams(admin: address, feeRecipient: address): GlobalParams {
    val storage: GlobalParamsStorage = {
        admin: admin,
        feeRecipient: feeRecipient,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
        paramsVersion: 1,
        codeVersion: 0,
    };
    val gp = GlobalParams.fromStorage(storage);
    val res = gp.deploy(admin, { value: ton("1") });
    expect(res).toHaveSuccessfulDeploy({ to: gp.address });
    return gp;
}

get fun `test admin update_params mutates storage in place, address unchanged`() {
    val admin = testing.treasury("admin");
    val feeRecipient = randomAddress("fee");
    val gp = deployGlobalParams(admin.address, feeRecipient);
    val addrBefore = gp.address;

    // Admin pushes a new fee + a changed parameter set.
    var p = validParams();
    p.platformFeeBps = 350;
    val newFee = randomAddress("fee2");
    val res = gp.sendUpdateParams(admin.address, 1, newFee, p, { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });

    // No redeploy: the SAME deterministic address now holds the UPDATED state.
    val same = GlobalParams.fromAddress(addrBefore);
    val view = same.getParams();
    expect(view.feeRecipient).toEqual(newFee);
    expect(same.getAdmin()).toEqual(admin.address);
    val loaded = view.params.load();
    expect(loaded.platformFeeBps).toEqual(350);
    // A redeploy at the same address would be required to change it — but we did
    // not redeploy, proving the address is stable across mutation.
    expect(same.address).toEqual(addrBefore);
}

get fun `test non-admin update_params is rejected`() {
    val admin = testing.treasury("admin");
    val attacker = testing.treasury("attacker");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    var p = validParams();
    p.platformFeeBps = 9999;
    val res = gp.sendUpdateParams(attacker.address, 2, randomAddress("evil"), p, { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });

    // State unchanged (still the original fee).
    val view = gp.getParams();
    expect(view.params.load().platformFeeBps).toEqual(200);
}

get fun `test out-of-bounds params are rejected on-chain`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // unbonding < challenge window violates the §8.4 invariant.
    var bad = validParams();
    bad.unbondingSecs = 100;
    val r1 = gp.sendUpdateParams(admin.address, 3, randomAddress("fee"), bad, { value: ton("0.5") });
    expect(r1).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });

    // slash split no longer sums to 100%.
    var bad2 = validParams();
    bad2.splitBurnBps = 5000;
    val r2 = gp.sendUpdateParams(admin.address, 4, randomAddress("fee"), bad2, { value: ton("0.5") });
    expect(r2).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });

    // selection ordering violated (n_default > n_max).
    var bad3 = validParams();
    bad3.nMax = 2;
    val r3 = gp.sendUpdateParams(admin.address, 5, randomAddress("fee"), bad3, { value: ton("0.5") });
    expect(r3).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });

    // Original params survive every rejected update.
    expect(gp.getParams().params.load().unbondingSecs).toEqual(604800);
}

get fun `test admin-gated governance blocklist (anti-abuse)`() {
    val admin = testing.treasury("admin");
    val attacker = testing.treasury("attacker");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    val subject = 0x1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0;

    // Unknown subjects are not blocked.
    expect(gp.getBlocklisted(subject)).toEqual(false);

    // A non-admin cannot add to the governance blocklist.
    val denied = gp.sendSetBlocklisted(attacker.address, 1, subject, true, { value: ton("0.1") });
    expect(denied).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });
    expect(gp.getBlocklisted(subject)).toEqual(false);

    // The admin blocks the subject (mutates storage in place).
    val ok = gp.sendSetBlocklisted(admin.address, 2, subject, true, { value: ton("0.1") });
    expect(ok).toHaveSuccessfulTx<SetBlocklisted>({ from: admin.address, to: gp.address });
    expect(gp.getBlocklisted(subject)).toEqual(true);

    // The admin can clear it again.
    val cleared = gp.sendSetBlocklisted(admin.address, 3, subject, false, { value: ton("0.1") });
    expect(cleared).toHaveSuccessfulTx<SetBlocklisted>({ from: admin.address, to: gp.address });
    expect(gp.getBlocklisted(subject)).toEqual(false);
}

get fun `test admin rotation hands control to a new admin (multisig handoff)`() {
    val admin = testing.treasury("admin");
    val multisig = testing.treasury("multisig");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // Rotate admin -> multisig.
    val rot = gp.sendUpdateAdmin(admin.address, 6, multisig.address);
    expect(rot).toHaveSuccessfulTx<UpdateAdmin>({ from: admin.address, to: gp.address });
    expect(gp.getAdmin()).toEqual(multisig.address);

    // Old admin can no longer update.
    val denied = gp.sendUpdateParams(admin.address, 7, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(denied).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });

    // New admin can.
    val ok = gp.sendUpdateParams(multisig.address, 8, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(ok).toHaveSuccessfulTx<UpdateParams>({ from: multisig.address, to: gp.address });
}

// ===========================================================================
// Hardening: focused tests that kill surviving mutants on the platform-wide
// parameter contract — every §12 bounds invariant, both boundaries, and the
// admin / opcode guards.
// ===========================================================================

// Params with EVERY `<=` / `>=` invariant sitting EXACTLY on its boundary, so a
// weakened comparison (`<=`->`<`, `>=`->`>`) would wrongly reject this set.
fun boundaryValidParams(): EcoParams {
    return {
        platformFeeBps: 10000,
        surchargeBps: 10000,
        participationCommissionBps: 1000, // == κ cap
        slashWrongBps: 10000,
        slashCheatBps: 10000,
        slashDowntimeBps: 10000,
        slashEquivocationBps: 10000,
        splitChallengerBps: 4000,
        splitRedundancyBps: 3000,
        splitBurnBps: 2000,
        splitTreasuryBps: 1000,
        minStake: ton("1000"),
        minStakeInternal: ton("1000"),   // == minStake
        minStakeSensitive: ton("1000"),  // == minStakeInternal
        stakeCap: ton("1000"),           // == minStake
        unbondingSecs: 86400,
        challengeWindowSecs: 86400,       // == unbondingSecs
        nPublic: 1,
        nDefault: 1,
        nMax: 1,
        quorum: 1,
        checksumMin: 1,
        wQualityBps: 10000,
        wStakeBps: 10000,
        wPriceBps: 10000,
        slashFailedCommitmentBps: 10000, // == bps cap
        attemptDeadlineMs: 0,
        progressIntervalMs: 1,            // == min
        progressStallMult: 1,             // == min
    };
}

fun expectParamsRejected(gp: GlobalParams, admin: address, qid: uint64, p: EcoParams) {
    val res = gp.sendUpdateParams(admin, qid, randomAddress("fee"), p, { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: admin, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });
}

get fun `test every economic-parameter invariant is enforced on-chain`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // bps fractions must be within [0, 10000].
    var p = validParams(); p.platformFeeBps = 10001; expectParamsRejected(gp, admin.address, 10, p);
    p = validParams(); p.surchargeBps = 10001; expectParamsRejected(gp, admin.address, 11, p);
    p = validParams(); p.slashWrongBps = 10001; expectParamsRejected(gp, admin.address, 12, p);
    p = validParams(); p.slashCheatBps = 10001; expectParamsRejected(gp, admin.address, 13, p);
    p = validParams(); p.slashDowntimeBps = 10001; expectParamsRejected(gp, admin.address, 14, p);
    p = validParams(); p.slashEquivocationBps = 10001; expectParamsRejected(gp, admin.address, 15, p);
    p = validParams(); p.wQualityBps = 10001; expectParamsRejected(gp, admin.address, 16, p);
    p = validParams(); p.wStakeBps = 10001; expectParamsRejected(gp, admin.address, 17, p);
    p = validParams(); p.wPriceBps = 10001; expectParamsRejected(gp, admin.address, 18, p);

    // κ (participation commission) must be <= 1000 bps (10%).
    p = validParams(); p.participationCommissionBps = 1001; expectParamsRejected(gp, admin.address, 19, p);

    // Slash split must sum to exactly 100%.
    p = validParams(); p.splitBurnBps = 5000; expectParamsRejected(gp, admin.address, 20, p);

    // Per-class min-stake monotonicity + cap >= min.
    p = validParams(); p.minStakeSensitive = ton("50"); expectParamsRejected(gp, admin.address, 21, p);
    p = validParams(); p.minStake = ton("200"); expectParamsRejected(gp, admin.address, 22, p);
    p = validParams(); p.minStake = ton("100"); p.stakeCap = ton("50"); expectParamsRejected(gp, admin.address, 23, p);

    // Unbonding must outlast the challenge window.
    p = validParams(); p.unbondingSecs = 100; expectParamsRejected(gp, admin.address, 24, p);

    // Selection ordering: n_max >= n_default >= n_public >= checksum_min >= 1.
    p = validParams(); p.checksumMin = 0; expectParamsRejected(gp, admin.address, 25, p);
    p = validParams(); p.nPublic = 2; expectParamsRejected(gp, admin.address, 26, p);
    // nDefault < nPublic, with quorum lowered so ONLY the nDefault>=nPublic
    // invariant is violated (otherwise quorum<=nDefault would mask it).
    p = validParams(); p.nDefault = 2; p.quorum = 1; expectParamsRejected(gp, admin.address, 27, p);
    p = validParams(); p.nMax = 2; expectParamsRejected(gp, admin.address, 28, p);

    // Quorum must satisfy 1 <= quorum <= n_default (pins both sides of the &&).
    p = validParams(); p.quorum = 0; expectParamsRejected(gp, admin.address, 29, p);
    p = validParams(); p.quorum = 6; expectParamsRejected(gp, admin.address, 30, p);

    // Resilience gating invariants (the on-chain fairness params).
    p = validParams(); p.slashFailedCommitmentBps = 10001; expectParamsRejected(gp, admin.address, 31, p);
    p = validParams(); p.progressIntervalMs = 0; expectParamsRejected(gp, admin.address, 32, p);
    p = validParams(); p.progressStallMult = 0; expectParamsRejected(gp, admin.address, 33, p);

    // Original params survive every rejected update.
    expect(gp.getParams().params.load().platformFeeBps).toEqual(200);
}

get fun `test params version bumps monotonically on every successful update`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // Deployed at version 1 (see deployGlobalParams).
    expect(gp.getParamsVersion()).toEqual(1);
    expect(gp.getParams().paramsVersion).toEqual(1);

    // Each accepted update_params bumps the monotonic seqno by exactly 1.
    val r1 = gp.sendUpdateParams(admin.address, 50, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(r1).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getParamsVersion()).toEqual(2);

    val r2 = gp.sendUpdateParams(admin.address, 51, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(r2).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getParamsVersion()).toEqual(3);

    // A REJECTED update (non-admin) does NOT bump the version.
    val attacker = testing.treasury("attacker");
    val denied = gp.sendUpdateParams(attacker.address, 52, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(denied).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });
    expect(gp.getParamsVersion()).toEqual(3);

    // A REJECTED update (out-of-bounds) does NOT bump the version either.
    var bad = validParams(); bad.progressStallMult = 0;
    val r3 = gp.sendUpdateParams(admin.address, 53, randomAddress("fee"), bad, { value: ton("0.5") });
    expect(r3).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });
    expect(gp.getParamsVersion()).toEqual(3);
}

get fun `test resilience + new policy scalar getters read back the stored params`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // The single-int policy getters (the off-chain RPC read path) return the
    // ecosystem-wide values a paid job reads as authoritative.
    expect(gp.getPlatformFeeBps()).toEqual(200);
    expect(gp.getParticipationCommissionBps()).toEqual(200);
    expect(gp.getSlashFailedCommitmentBps()).toEqual(1000);
    expect(gp.getAttemptDeadlineMs()).toEqual(60000);
    expect(gp.getProgressIntervalMs()).toEqual(2000);
    expect(gp.getProgressStallMult()).toEqual(5);

    // After an admin edit they reflect the new values.
    var p = validParams();
    p.slashFailedCommitmentBps = 2500;
    p.attemptDeadlineMs = 45000;
    val res = gp.sendUpdateParams(admin.address, 60, randomAddress("fee"), p, { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getSlashFailedCommitmentBps()).toEqual(2500);
    expect(gp.getAttemptDeadlineMs()).toEqual(45000);
}

get fun `test params sitting exactly on every boundary are accepted`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    // All invariants at their exact edge must be ACCEPTED (pins every `<=`/`>=`
    // boundary: a strict comparison would wrongly reject the edge).
    val res = gp.sendUpdateParams(admin.address, 40, randomAddress("fee"), boundaryValidParams(), { value: ton("0.5") });
    expect(res).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
    expect(gp.getParams().params.load().participationCommissionBps).toEqual(1000);
}

get fun `test update_admin from a non-admin is rejected`() {
    val admin = testing.treasury("admin");
    val attacker = testing.treasury("attacker");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    val res = gp.sendUpdateAdmin(attacker.address, 1, attacker.address);
    expect(res).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ErrCode.NOT_ADMIN });
    // Admin unchanged.
    expect(gp.getAdmin()).toEqual(admin.address);
}

get fun `test global params rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address, randomAddress("fee"));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = gp.sendAny(admin.address, body, { value: ton("0.5") });
    expect(res).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test fee-recipient getter covers the read path`() {
    val admin = testing.treasury("admin");
    val feeRecipient = randomAddress("fee");
    val gp = deployGlobalParams(admin.address, feeRecipient);
    expect(gp.getFeeRecipient()).toEqual(feeRecipient);
}
