// Activate a per-node payout wallet on-chain (NO staking) so it can RECEIVE the
// escrow's bounceable settlement payout.
//
// The JobEscrow pays winners/participants with BOUNCEABLE messages (B2/B3
// bounce-safety): a bounceable transfer to an *uninitialized* account bounces
// back into the escrow's pull-ledger instead of landing. So a payout recipient
// must be an ACTIVE account first. A wallet self-deploys on its first outgoing
// message, which needs a little gas — paid here by the funded `deployer`, NOT by
// the node, and with NO stake bonded.
//
//   NODE=<per-node-wallet-addr> acton script scripts/init_node_wallet.tolk --net testnet
//
// Idempotent: a no-op if the wallet is already active.

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"

fun main() {
    val deployer = scripts.wallet("deployer");
    val node = env<address>("NODE")!;

    if (scripts.isContractDeployed(node)) {
        println("::CHECK::node_active::{}::already=true", node);
        return;
    }

    // 1) Fund the uninitialized node account (NON-bounceable so it lands).
    val fundAmount = env<coins>("FUND_AMOUNT") ?? ton("0.08");
    net.send(deployer.address, createMessage({
        bounce: false,
        value: fundAmount,
        dest: node,
    })).waitForTrace();

    // 2) The node self-deploys its v5r1 code by sending its first message back to
    //    the deployer (acton attaches the wallet stateInit on first send).
    net.send(node, createMessage({
        bounce: false,
        value: ton("0.01"),
        dest: deployer.address,
    })).waitForTrace();

    println("::CHECK::node_active::{}::deployed={}", node, scripts.isContractDeployed(node));
}
