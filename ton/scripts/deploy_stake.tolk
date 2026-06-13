// Deploy a per-node StakeVault (+ its transfer-locked stake-receipt jetton).
//
// Network-agnostic: run in local emulation with
//     acton script scripts/deploy_stake.tolk
// or against testnet/mainnet with
//     acton script scripts/deploy_stake.tolk --net testnet
//
// All parameters are read from the environment (with sensible defaults), so the
// same script deploys to any network without edits. Example:
//     STAKE_MIN=100 STAKE_SLASHER=<addr> STAKE_TREASURY=<addr> \
//     STAKE_REDUNDANCY=<addr> STAKE_BINDING_HASH=0x... \
//     acton script scripts/deploy_stake.tolk --net testnet

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/stake_types"
import "@wrappers/StakeVault.gen"

fun main() {
    val owner = scripts.wallet("deployer");

    val slasher = env<address>("STAKE_SLASHER") ?? owner.address;
    val treasury = env<address>("STAKE_TREASURY") ?? owner.address;
    val redundancy = env<address>("STAKE_REDUNDANCY") ?? owner.address;
    val minStake = env<coins>("STAKE_MIN") ?? ton("100");
    val unbondingPeriod = (env<int>("STAKE_UNBONDING_SECS") ?? 604800) as uint32; // >= challenge window
    val challengeWindow = (env<int>("STAKE_CHALLENGE_SECS") ?? 86400) as uint32;
    val keeperGrace = (env<int>("STAKE_KEEPER_GRACE_SECS") ?? 86400) as uint32;
    val bindingHash = env<int>("STAKE_BINDING_HASH") ?? 0;

    val slash: SlashConfig = {
        treasury: treasury,
        redundancyPool: redundancy,
        splitChallengerBps: (env<int>("SLASH_CHALLENGER_BPS") ?? 4000) as uint16,
        splitRedundancyBps: (env<int>("SLASH_REDUNDANCY_BPS") ?? 3000) as uint16,
        splitBurnBps: (env<int>("SLASH_BURN_BPS") ?? 2000) as uint16,
        splitTreasuryBps: (env<int>("SLASH_TREASURY_BPS") ?? 1000) as uint16,
    };
    val cfg: VaultConfig = {
        minStake: minStake,
        unbondingPeriod: unbondingPeriod,
        challengeWindow: challengeWindow,
        keeperGrace: keeperGrace,
        keeperBountyBps: (env<int>("STAKE_KEEPER_BOUNTY_BPS") ?? 200) as uint16,
        slasher: slasher,
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    val storage: VaultStorage = {
        owner: owner.address,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: bindingHash,
        config: cfg.toCell(),
    };

    val vault = StakeVault.fromStorage(storage);
    val res = vault.deploy(owner.address, { value: ton("0.1") });
    res.waitForTrace();

    println("Deployed StakeVault to {}", vault.address);
    println("  owner       = {}", owner.address);
    println("  min stake   = {}", minStake);
    println("  receipt for = {}", vault.getReceiptWalletAddress(owner.address));
}
