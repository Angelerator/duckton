// Deploy a public-tier StakeVault and MINT a real "Duckton" stake-receipt on
// testnet, so the (non-transferable) receipt jetton is actually VISIBLE in
// explorers / wallets — then read its TEP-64 metadata + balance back.
//
//   acton script scripts/show_duckton.tolk --net testnet
//
// Why this exists: the receipt jetton is only minted when a node BONDS stake
// (StakeDeposit). Deploying the vault alone leaves totalSupply == 0, so nothing
// shows up.
//
// ADDRESS STABILITY: a TON address is a pure function of (code, initial data),
// so a vault's address is DETERMINISTIC, not random. To keep ONE canonical,
// stable Duckton master across runs, pin its address:
//     VAULT_ADDR=kQ... acton script scripts/show_duckton.tolk --net testnet
// With VAULT_ADDR set, this reuses that exact master and just mints more (the
// address never changes). Without it, it deploys a fresh public-tier vault
// (minStake 0) owned by the `deployer` wallet and bonds DUCKTON_STAKE (0.2 TON)
// to mint Duckton 1:1. Either way it prints the master + owner + receipt-wallet
// addresses and the on-chain metadata.

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/stake_types"
import "@wrappers/StakeVault.gen"
import "@wrappers/StakeReceiptWallet.gen"

// Read the full jetton state back so the output is a self-contained proof.
fun reportDuckton(vault: StakeVault, ownerAddr: address) {
    val vs = vault.getVaultState();
    val receiptAddr = vault.getReceiptWalletAddress(ownerAddr);
    val receipt = StakeReceiptWallet.fromAddress(receiptAddr);
    val wd = receipt.getWalletData();
    val locked = receipt.isTransferLocked();
    val jd = vault.getJettonData();
    val metaOk = jd.jettonContent.hash() == buildReceiptJettonContent().hash()
        && jd.mintable
        && jd.adminAddress == vault.address
        && jd.jettonWalletCode.hash() == build("StakeReceiptWallet").hash();

    println("::DUCKTON::master={}", vault.address);
    println("::DUCKTON::owner={}", ownerAddr);
    println("::DUCKTON::receipt_wallet={}", receiptAddr);
    println("::DUCKTON::staked={}::minted_balance={}::locked={}", vs.staked, wd.balance, locked);
    println(
        "::DUCKTON::metadata_ok={}::name=Duckton::symbol=DUCKTON::decimals={}::total_supply={}",
        metaOk,
        RECEIPT_JETTON_DECIMALS,
        jd.totalSupply,
    );
}

fun main() {
    val owner = scripts.wallet("deployer");
    val depositAmount = env<coins>("DUCKTON_STAKE") ?? ton("0.2");
    val pinned = env<address>("VAULT_ADDR");

    if (pinned != null) {
        // Reuse the pinned canonical vault (STABLE address) — just mint more.
        val vault = StakeVault.fromAddress(pinned!);
        println("Reusing pinned StakeVault (Duckton master) at {}", vault.address);
        vault.sendStakeDeposit(
            owner.address,
            1,
            depositAmount,
            { value: depositAmount + ton("0.2") },
        ).waitForTrace();
        reportDuckton(vault, owner.address);
        return;
    }

    // No pin -> deploy a fresh public-tier vault (minStake 0 == any stake mints).
    val slash: SlashConfig = {
        treasury: env<address>("STAKE_TREASURY") ?? owner.address,
        redundancyPool: env<address>("STAKE_REDUNDANCY") ?? owner.address,
        splitChallengerBps: (env<int>("SLASH_CHALLENGER_BPS") ?? 4000) as uint16,
        splitRedundancyBps: (env<int>("SLASH_REDUNDANCY_BPS") ?? 3000) as uint16,
        splitBurnBps: (env<int>("SLASH_BURN_BPS") ?? 2000) as uint16,
        splitTreasuryBps: (env<int>("SLASH_TREASURY_BPS") ?? 1000) as uint16,
    };
    val cfg: VaultConfig = {
        minStake: env<coins>("STAKE_MIN") ?? ton("0"),
        unbondingPeriod: (env<int>("STAKE_UNBONDING_SECS") ?? 604800) as uint32,
        challengeWindow: (env<int>("STAKE_CHALLENGE_SECS") ?? 86400) as uint32,
        keeperGrace: (env<int>("STAKE_KEEPER_GRACE_SECS") ?? 86400) as uint32,
        keeperBountyBps: (env<int>("STAKE_KEEPER_BOUNTY_BPS") ?? 200) as uint16,
        slasher: env<address>("STAKE_SLASHER") ?? owner.address,
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    val storage: VaultStorage = {
        owner: owner.address,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: env<int>("STAKE_BINDING_HASH") ?? 0,
        config: cfg.toCell(),
        upgrade: freshVaultUpgrade(),
    };

    val vault = StakeVault.fromStorage(storage);
    vault.deploy(owner.address, { value: ton("0.1") }).waitForTrace();
    println("Deployed StakeVault (Duckton master) to {}", vault.address);

    // Bond stake -> mint the Duckton receipt 1:1 to the owner.
    vault.sendStakeDeposit(
        owner.address,
        1,
        depositAmount,
        { value: depositAmount + ton("0.2") },
    ).waitForTrace();

    reportDuckton(vault, owner.address);
}
