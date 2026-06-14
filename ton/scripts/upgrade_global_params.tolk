// In-place CODE upgrade of a DEPLOYED GlobalParams (BLOCKCHAIN_ECONOMICS §12.1).
//
// This is the concrete proof that a code change no longer moves the contract
// address: it takes a DEPLOYED GlobalParams, swaps its CODE to GlobalParamsV2 via
// the admin-gated `upgrade_code` (TVM SETCODE), and asserts the ADDRESS is
// UNCHANGED, the stored params/admin/version are PRESERVED, the monotonic
// `codeVersion` BUMPED, and a getter that exists ONLY in v2 (`get_surcharge_bps`)
// is now live at the same address.
//
// Network-agnostic:
//   * Local emulation:   acton script scripts/upgrade_global_params.tolk
//   * Testnet (live):    acton script scripts/upgrade_global_params.tolk --net testnet
//
// If GLOBAL_PARAMS_ADDR is set, it upgrades that already-deployed contract
// (must be admin-signed by the `deployer` wallet). Otherwise it first DEPLOYS a
// fresh GlobalParams (admin = deployer) and then upgrades it in place — the
// self-contained live proof. Emits `::CHECK::...` tokens (same convention as
// scripts/e2e_testnet.tolk) so a harness can parse PASS/FAIL.

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/common"
import "@contracts/global_params_types"
import "@wrappers/GlobalParams.gen"

// §12 default economic params (surchargeBps = 500 so the new v2 getter has a
// known, provable value to read back).
fun defaultParams(): EcoParams {
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

fun main() {
    val admin = scripts.wallet("deployer");

    // Resolve the target: an already-deployed address, or a fresh deploy.
    val pinned = env<address>("GLOBAL_PARAMS_ADDR");
    var gp: GlobalParams;
    if (pinned != null) {
        gp = GlobalParams.fromAddress(pinned!);
        println("::CHECK::target::deployed={}", gp.address);
    } else {
        val storage: GlobalParamsStorage = {
            admin: admin.address,
            feeRecipient: admin.address,
            params: defaultParams().toCell(),
            blocklist: createEmptyMap(),
            paramsVersion: 1,
            codeVersion: 0,
        };
        gp = GlobalParams.fromStorage(storage);
        gp.deploy(admin.address, { value: ton("0.1") }).waitForTrace();
        println("::CHECK::target::fresh_deploy={}", gp.address);
    }

    // --- BEFORE snapshot -----------------------------------------------------
    val addrBefore = gp.address;
    val cvBefore = gp.getCodeVersion();
    val pvBefore = gp.getParamsVersion();
    val feeBefore = gp.getParams().params.load().platformFeeBps;
    val adminBefore = gp.getAdmin();
    println(
        "::CHECK::before::addr={}::code_version={}::params_version={}::fee_bps={}",
        addrBefore,
        cvBefore,
        pvBefore,
        feeBefore,
    );

    // --- IN-PLACE CODE UPGRADE (admin-signed) --------------------------------
    // Swap the CODE to GlobalParamsV2 via SETCODE. No redeploy → address stable.
    val res = gp.sendUpgradeCode(admin.address, 1, build("GlobalParamsV2"), { value: ton("0.2") });
    res.waitForTrace();

    // --- AFTER snapshot (read back from the SAME address) --------------------
    val same = GlobalParams.fromAddress(addrBefore);
    val cvAfter = same.getCodeVersion();
    val pvAfter = same.getParamsVersion();
    val feeAfter = same.getParams().params.load().platformFeeBps;
    val adminAfter = same.getAdmin();
    // New code is live iff the v2-only getter resolves (v1 lacks it entirely).
    val surchargeAfter = net.runGetMethod<int>(same.address, "get_surcharge_bps");

    val addrStable = same.address == addrBefore;
    val codeBumped = cvAfter == cvBefore + 1;
    val statePreserved = pvAfter == pvBefore && feeAfter == feeBefore && adminAfter == adminBefore;
    val pass = addrStable && codeBumped && statePreserved;

    println(
        "::CHECK::upgrade::{}::addr_stable={}::code_version={}->{}",
        pass ? "PASS" : "FAIL",
        addrStable,
        cvBefore,
        cvAfter,
    );
    println(
        "::CHECK::after::params_version={}::fee_bps={}::surcharge_bps={}::state_preserved={}",
        pvAfter,
        feeAfter,
        surchargeAfter,
        statePreserved,
    );
    println("::CHECK::done");
}
