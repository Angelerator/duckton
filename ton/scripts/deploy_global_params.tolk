// Deploy the platform-wide GlobalParams contract (BLOCKCHAIN_ECONOMICS §12).
//
// Network-agnostic: run in local emulation with
//     acton script scripts/deploy_global_params.tolk
// or against testnet/mainnet with
//     acton script scripts/deploy_global_params.tolk --net testnet
//
// The admin (deployer wallet) can later edit the params IN PLACE via
// `update_params` (address stays stable) and hand control to a multisig via
// `update_admin`. All initial values are read from the environment with §12
// defaults.

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/global_params_types"
import "@wrappers/GlobalParams.gen"

fun main() {
    val admin = scripts.wallet("deployer");
    val feeRecipient = env<address>("GP_FEE_RECIPIENT") ?? admin.address;

    val params: EcoParams = {
        platformFeeBps: (env<int>("GP_PLATFORM_FEE_BPS") ?? 200) as uint16,
        surchargeBps: (env<int>("GP_SURCHARGE_BPS") ?? 500) as uint16,
        participationCommissionBps: (env<int>("GP_PARTICIPATION_BPS") ?? 200) as uint16,
        slashWrongBps: (env<int>("GP_SLASH_WRONG_BPS") ?? 1500) as uint16,
        slashCheatBps: (env<int>("GP_SLASH_CHEAT_BPS") ?? 10000) as uint16,
        slashDowntimeBps: (env<int>("GP_SLASH_DOWNTIME_BPS") ?? 200) as uint16,
        slashEquivocationBps: (env<int>("GP_SLASH_EQUIV_BPS") ?? 5000) as uint16,
        splitChallengerBps: (env<int>("GP_SPLIT_CHALLENGER_BPS") ?? 4000) as uint16,
        splitRedundancyBps: (env<int>("GP_SPLIT_REDUNDANCY_BPS") ?? 3000) as uint16,
        splitBurnBps: (env<int>("GP_SPLIT_BURN_BPS") ?? 2000) as uint16,
        splitTreasuryBps: (env<int>("GP_SPLIT_TREASURY_BPS") ?? 1000) as uint16,
        minStake: env<coins>("GP_MIN_STAKE") ?? ton("0"),
        minStakeInternal: env<coins>("GP_MIN_STAKE_INTERNAL") ?? ton("100"),
        minStakeSensitive: env<coins>("GP_MIN_STAKE_SENSITIVE") ?? ton("1000"),
        stakeCap: env<coins>("GP_STAKE_CAP") ?? ton("100000"),
        unbondingSecs: (env<int>("GP_UNBONDING_SECS") ?? 604800) as uint32,
        challengeWindowSecs: (env<int>("GP_CHALLENGE_SECS") ?? 86400) as uint32,
        nPublic: (env<int>("GP_N_PUBLIC") ?? 3) as uint8,
        nDefault: (env<int>("GP_N_DEFAULT") ?? 5) as uint8,
        nMax: (env<int>("GP_N_MAX") ?? 10) as uint8,
        quorum: (env<int>("GP_QUORUM") ?? 3) as uint8,
        checksumMin: (env<int>("GP_CHECKSUM_MIN") ?? 3) as uint8,
        wQualityBps: (env<int>("GP_W_QUALITY_BPS") ?? 6000) as uint16,
        wStakeBps: (env<int>("GP_W_STAKE_BPS") ?? 1500) as uint16,
        wPriceBps: (env<int>("GP_W_PRICE_BPS") ?? 2500) as uint16,
    };

    val storage: GlobalParamsStorage = {
        admin: admin.address,
        feeRecipient: feeRecipient,
        params: params.toCell(),
        blocklist: createEmptyMap(),
    };

    val gp = GlobalParams.fromStorage(storage);
    val res = gp.deploy(admin.address, { value: ton("0.1") });
    res.waitForTrace();

    println("Deployed GlobalParams to {}", gp.address);
    println("  admin         = {}", admin.address);
    println("  fee recipient = {}", feeRecipient);
}
