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
    };
}

fun deployGlobalParams(admin: address, feeRecipient: address): GlobalParams {
    val storage: GlobalParamsStorage = {
        admin: admin,
        feeRecipient: feeRecipient,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
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
    expect(res).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ERROR_NOT_ADMIN });

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
    expect(r1).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ERROR_PARAMS_OUT_OF_BOUNDS });

    // slash split no longer sums to 100%.
    var bad2 = validParams();
    bad2.splitBurnBps = 5000;
    val r2 = gp.sendUpdateParams(admin.address, 4, randomAddress("fee"), bad2, { value: ton("0.5") });
    expect(r2).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ERROR_PARAMS_OUT_OF_BOUNDS });

    // selection ordering violated (n_default > n_max).
    var bad3 = validParams();
    bad3.nMax = 2;
    val r3 = gp.sendUpdateParams(admin.address, 5, randomAddress("fee"), bad3, { value: ton("0.5") });
    expect(r3).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ERROR_PARAMS_OUT_OF_BOUNDS });

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
    expect(denied).toHaveFailedTx({ from: attacker.address, to: gp.address, exitCode: ERROR_NOT_ADMIN });
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
    expect(denied).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ERROR_NOT_ADMIN });

    // New admin can.
    val ok = gp.sendUpdateParams(multisig.address, 8, randomAddress("fee"), validParams(), { value: ton("0.5") });
    expect(ok).toHaveSuccessfulTx<UpdateParams>({ from: multisig.address, to: gp.address });
}
