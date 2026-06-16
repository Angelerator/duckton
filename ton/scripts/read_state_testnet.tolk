// Read-only on-chain getter dump for documentation/verification (gas-free).
// Prints the authoritative GlobalParams economic getters and, if VAULT is set,
// the StakeVault state + its receipt-jetton wallet balance for OWNER.
//
//   GP=<addr> [VAULT=<addr>] [OWNER=<addr>] acton script scripts/read_state_testnet.tolk --net testnet

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/global_params_types"
import "@contracts/stake_types"
import "@wrappers/GlobalParams.gen"
import "@wrappers/StakeVault.gen"
import "@wrappers/StakeReceiptWallet.gen"

fun main() {
    val gpAddr = env<address>("GP");
    if (gpAddr != null) {
        val gp = GlobalParams.fromAddress(gpAddr!);
        println("::GP::address={}", gpAddr!);
        println("::GP::admin={}", gp.getAdmin());
        println("::GP::fee_recipient={}", gp.getFeeRecipient());
        println("::GP::platform_fee_bps={}", gp.getPlatformFeeBps());
        println("::GP::participation_commission_bps={}", gp.getParticipationCommissionBps());
        println("::GP::params_version={}", gp.getParamsVersion());
    }

    val vaultAddr = env<address>("VAULT");
    if (vaultAddr != null) {
        val vault = StakeVault.fromAddress(vaultAddr!);
        val vs = vault.getVaultState();
        println("::VAULT::address={}", vaultAddr!);
        println("::VAULT::staked={}", vs.staked);
        println("::VAULT::unbondingAmount={}", vs.unbondingAmount);
        println("::VAULT::unbondingAt={}", vs.unbondingAt);
        println("::VAULT::totalSupply={}", vs.totalSupply);
        println("::VAULT::eligible={}", vs.eligible);
        println("::VAULT::code_version={}", vault.getCodeVersion());

        val owner = env<address>("OWNER");
        if (owner != null) {
            val receiptAddr = vault.getReceiptWalletAddress(owner!);
            println("::VAULT::receipt_wallet={}", receiptAddr);
            if (scripts.isContractDeployed(receiptAddr)) {
                val rw = StakeReceiptWallet.fromAddress(receiptAddr);
                val wd = rw.getWalletData();
                println("::RECEIPT::balance={}", wd.balance);
                println("::RECEIPT::locked={}", rw.isTransferLocked());
            } else {
                println("::RECEIPT::balance=0::not_yet_deployed");
            }
        }
    }
}
