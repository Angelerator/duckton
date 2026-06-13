// Live end-to-end scenario over the THREE already-deployed contracts, run in a
// single broadcast session:
//
//   1. stake deposit         -> StakeVault.StakeDeposit   (bond + mint receipt)
//   2. open-escrow settle     -> JobEscrow.EscrowSettle     (HTLC release on the
//                                  agreed result hash: winner + platform fee +
//                                  participation commissions, remainder refunds)
//   3. anchor epoch root      -> RecordAnchor.AnchorSubmitRoot (chained, weighted)
//   4. inclusion proof        -> RecordAnchor.verify_inclusion (get-method)
//   5. optional dispute       -> AnchorOpenDispute + AnchorResolveDispute
//
// The escrow is "opened" (deployed with the locked B) by scripts/deploy_escrow.tolk
// before this runs; the per-node StakeVault and per-epoch RecordAnchor are
// deployed by scripts/deploy_stake.tolk and scripts/deploy_anchor.tolk. This
// script only drives the post-deploy flow and reads state back, printing
// `::CHECK::...` lines the bash harness parses into a PASS/FAIL summary.
//
// All inputs come from the environment (addresses + amounts + hashes); see
// scripts/testnet_e2e.sh and docs/TESTNET.md. Run:
//     acton script scripts/e2e_testnet.tolk --net testnet
//
// Re-runnability: the anchor epoch is read from chain and advanced by one each
// run (prevRoot chains to the stored lastRoot), and the escrow/anchor addresses
// vary per run via the deadline / fresh deploys, so the scenario can be replayed.

import "@acton/env"
import "@acton/fmt"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@acton/testing/assert"
import "@contracts/anchor_types"
import "@contracts/escrow_types"
import "@contracts/stake_types"
import "@wrappers/JobEscrow.gen"
import "@wrappers/RecordAnchor.gen"
import "@wrappers/StakeVault.gen"

fun requireAddr(name: string): address {
    val a = env<address>(name);
    if (a == null) {
        Assert.fail(format("required env address {} is missing", name));
    }
    return a!;
}

fun main() {
    val deployer = scripts.wallet("deployer");

    val vaultAddr = requireAddr("VAULT_ADDR");
    val escrowAddr = requireAddr("ESCROW_ADDR");
    val anchorAddr = requireAddr("ANCHOR_ADDR");

    println("::CHECK::wallet={}", deployer.address);

    // -- 1. Stake deposit ----------------------------------------------------
    // Bond `STAKE_DEPOSIT_AMOUNT` non-custodially and mint the 1:1 transfer-locked
    // stake-receipt jetton. Sender must be the vault owner (the deployer).
    val depositAmount = env<coins>("STAKE_DEPOSIT_AMOUNT") ?? ton("0.1");
    val vault = StakeVault.fromAddress(vaultAddr);
    val depositRes = vault.sendStakeDeposit(
        deployer.address,
        1,
        depositAmount,
        // value must cover amount + MIN_TONS_FOR_STORAGE (0.05) + gas.
        { value: depositAmount + ton("0.2") },
    );
    depositRes.waitForTrace();
    val vs = vault.getVaultState();
    println("::CHECK::stake_deposit::staked={}::eligible={}", vs.staked, vs.eligible);

    // -- 2. Escrow settle (payout split, HTLC-gated) -------------------------
    // Release the per-job escrow on the agreed quorum result hash: winner base +
    // perf bonus, platform fee to treasury, fixed participation commission to
    // each agreeing non-winner; any remainder refunds to the requester.
    val winner = env<address>("SETTLE_WINNER") ?? deployer.address;
    val winnerAmount = env<coins>("SETTLE_WINNER_AMOUNT") ?? ton("0.1");
    val platformFee = env<coins>("SETTLE_FEE") ?? ton("0.02");
    val participant = env<address>("SETTLE_PARTICIPANT") ?? deployer.address;
    val participantAmount = env<coins>("SETTLE_PARTICIPANT_AMOUNT") ?? ton("0.02");
    val resultHash = env<int>("SETTLE_RESULT_HASH") ?? 0;

    var participants: map<address, coins> = createEmptyMap();
    if (participantAmount > 0) {
        participants.set(participant, participantAmount);
    }

    val escrow = JobEscrow.fromAddress(escrowAddr);
    val settleRes = escrow.sendEscrowSettle(
        deployer.address,
        2,
        resultHash,
        winner,
        winnerAmount,
        platformFee,
        participants,
        { value: ton("0.1") },
    );
    settleRes.waitForTrace();
    val es = escrow.getEscrowState();
    println("::CHECK::escrow_settle::settled={}", es.settled);

    // -- 3. Anchor the epoch Merkle root -------------------------------------
    // Read the current epoch/lastRoot from chain and submit the next epoch's
    // root chained to it (permissionless keeper; stake-weighted acceptance).
    val anchor = RecordAnchor.fromAddress(anchorAddr);
    val before = anchor.getAnchorState();
    val nextEpoch = (before.currentEpoch + 1) as uint32;
    // For a single-leaf epoch tree the root equals the leaf and the inclusion
    // proof is empty (computeRootFromProof(leaf, null) == leaf).
    val root = env<int>("ANCHOR_ROOT") ?? resultHash;
    val leaf = env<int>("ANCHOR_LEAF") ?? root;
    val stakeWeight = env<coins>("ANCHOR_STAKE_WEIGHT") ?? ton("100");

    val submitRes = anchor.sendAnchorSubmitRoot(
        deployer.address,
        3,
        nextEpoch,
        root,
        before.lastRoot,
        stakeWeight,
        { value: ton("0.1") },
    );
    submitRes.waitForTrace();
    val after = anchor.getAnchorState();
    println(
        "::CHECK::anchor_submit::epoch={}::prev_epoch={}",
        after.currentEpoch,
        before.currentEpoch,
    );

    // -- 4. Inclusion proof verified on-chain --------------------------------
    val included = anchor.verifyInclusion(leaf, root, null);
    println("::CHECK::inclusion::verified={}", included);

    // -- 5. Optional bonded dispute round ------------------------------------
    if (env<bool>("E2E_RUN_DISPUTE") ?? false) {
        val bond = env<coins>("DISPUTE_BOND") ?? ton("0.2");
        val openRes = anchor.sendAnchorOpenDispute(
            deployer.address,
            4,
            after.currentEpoch,
            leaf,
            root,
            null,
            { value: bond + ton("0.1") },
        );
        openRes.waitForTrace();
        val mid = anchor.getAnchorState();
        val disputeId = (mid.nextDisputeId - 1) as uint32;

        // Verdict from the configured authority (the deployer here): uphold the
        // challenge, which returns the bond to the challenger.
        val resolveRes = anchor.sendAnchorResolveDispute(
            deployer.address,
            5,
            disputeId,
            true,
            { value: ton("0.1") },
        );
        resolveRes.waitForTrace();

        val di = anchor.getDispute(disputeId);
        if (di != null) {
            println("::CHECK::dispute::id={}::status={}", disputeId, di!.status);
        } else {
            println("::CHECK::dispute::id={}::status=missing", disputeId);
        }
    } else {
        println("::CHECK::dispute::skipped");
    }

    println("::CHECK::done");
}
