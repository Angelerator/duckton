// StakeVault lifecycle op driver (deposit / unbond / withdraw) for the on-chain
// staking proof (BLOCKCHAIN_ECONOMICS §8). The vault owner = the funded deployer.
//
//   VAULT=<addr> ACTION=deposit  AMOUNT=<nanoton> acton script scripts/stake_op_testnet.tolk --net testnet
//   VAULT=<addr> ACTION=unbond   AMOUNT=<nanoton> acton script scripts/stake_op_testnet.tolk --net testnet
//   VAULT=<addr> ACTION=withdraw                  acton script scripts/stake_op_testnet.tolk --net testnet
//
// `withdraw` BEFORE the unbonding cooldown elapses is rejected on-chain with
// COOLDOWN_NOT_ELAPSED (203); the script still completes and prints the post-state
// (unchanged) so the rejection is observable.

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/stake_types"
import "@wrappers/StakeVault.gen"

fun main() {
    val owner = scripts.wallet("deployer");
    val vault = StakeVault.fromAddress(env<address>("VAULT")!);
    val action = env<int>("ACTION_CODE") ?? 0; // 1=deposit 2=unbond 3=withdraw

    if (action == 1) {
        val amount = env<coins>("AMOUNT")!;
        // value must cover amount + MIN_TONS_FOR_STORAGE (0.05) + gas.
        vault.sendStakeDeposit(owner.address, 1, amount, { value: amount + ton("0.08") }).waitForTrace();
    } else if (action == 2) {
        val amount = env<coins>("AMOUNT")!;
        vault.sendStakeRequestUnbond(owner.address, 2, amount, { value: ton("0.05") }).waitForTrace();
    } else if (action == 3) {
        vault.sendStakeWithdraw(owner.address, 3, { value: ton("0.05") }).waitForTrace();
    }

    val vs = vault.getVaultState();
    println("::VAULT::staked={}::unbondingAmount={}::unbondingAt={}::totalSupply={}::eligible={}",
        vs.staked, vs.unbondingAmount, vs.unbondingAt, vs.totalSupply, vs.eligible);
    println("::CHECK::done");
}
