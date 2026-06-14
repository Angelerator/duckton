// Live end-to-end scenario over the FOUR already-deployed contracts, run in a
// single broadcast session:
//
//   0. GlobalParams           -> admin update_params (persisted, address stable),
//                                  non-admin update_params rejected, governance
//                                  blocklist round-trip (set -> clear)
//   1. stake deposit          -> StakeVault.StakeDeposit   (bond + mint receipt)
//   1b. receipt jetton         -> minted 1:1 + transfer-locked (anti-exit)
//   1c. Duckton metadata       -> StakeVault.get_jetton_data (TEP-64 name/symbol/dec)
//   2. open-escrow settle      -> JobEscrow.EscrowSettle     (HTLC release on the
//                                  agreed result hash: winner + platform fee +
//                                  participation commissions, remainder refunds)
//   3. anchor epoch root       -> RecordAnchor.AnchorSubmitRoot (chained, weighted)
//   4. inclusion proof         -> RecordAnchor.verify_inclusion (get-method)
//   4b. multi-leaf inclusion    -> anchor an 8-leaf epoch root + verify an interior
//                                  leaf's inclusion proof on-chain
//   5. optional dispute        -> AnchorOpenDispute + AnchorResolveDispute
//
// The escrow is "opened" (deployed with the locked B) by scripts/deploy_escrow.tolk
// before this runs; the per-node StakeVault, per-epoch RecordAnchor and the
// platform-wide GlobalParams are deployed by scripts/deploy_stake.tolk,
// scripts/deploy_anchor.tolk and scripts/deploy_global_params.tolk. This script
// only drives the post-deploy flow and reads state back, printing `::CHECK::...`
// lines the bash harness parses into a PASS/FAIL summary (new scenarios emit an
// explicit `::CHECK::<name>::PASS` / `::CHECK::<name>::FAIL` token).
//
// All inputs come from the environment (addresses + amounts + hashes); see
// scripts/testnet_e2e.sh and docs/TESTNET.md. Run:
//     acton script scripts/e2e_testnet.tolk --net testnet
//
// Re-runnability: the anchor epoch is read from chain and advanced each run
// (prevRoot chains to the stored lastRoot), GlobalParams is mutated in place at a
// stable address (the admin update toggles the fee bps so the write is provable
// on a re-run), the non-admin check targets a throwaway GlobalParams whose admin
// is the (deterministic) vault address, and the escrow/anchor addresses vary per
// run via the deadline / fresh deploys, so the scenario can be replayed.

import "@acton/env"
import "@acton/fmt"
import "@acton/io"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@acton/testing/assert"
import "@contracts/anchor_types"
import "@contracts/escrow_types"
import "@contracts/global_params_types"
import "@contracts/stake_types"
import "@wrappers/GlobalParams.gen"
import "@wrappers/JobEscrow.gen"
import "@wrappers/RecordAnchor.gen"
import "@wrappers/StakeReceiptWallet.gen"
import "@wrappers/StakeVault.gen"

fun requireAddr(name: string): address {
    val a = env<address>(name);
    if (a == null) {
        Assert.fail(format("required env address {} is missing", name));
    }
    return a!;
}

// A valid platform-wide parameter set (mirrors the §12 defaults / the emulator
// `global_params.test.tolk::validParams`). Used to seed the throwaway non-admin
// GlobalParams and as the base for the admin in-place update.
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

// A deterministic 8-leaf Merkle tree (3 levels) — the realistic multi-record
// epoch case. Folded with the contract's `hashPair` node hash, IDENTICAL to the
// off-chain Rust builder (crates/settlement/src/merkle.rs). Mirrors the
// `tests/anchor.test.tolk` reference values so the on-chain inclusion check here
// matches the emulator test exactly.
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

fun tree8Left(): int {
    return hashPair(hashPair(M0, M1), hashPair(M2, M3));
}

fun tree8Right(): int {
    return hashPair(hashPair(M4, M5), hashPair(M6, M7));
}

fun tree8Root(): int {
    return hashPair(tree8Left(), tree8Right());
}

// Inclusion proof for the interior leaf M5 (index 5), mixed directions across 3
// levels (matches `tests/anchor.test.tolk::proofForM5`):
//   level 0: M5 is the RIGHT child of (M4,M5) -> sibling M4 on the LEFT  (dir = 1)
//   level 1: hash(M4,M5) is the LEFT child of (h45,h67) -> sibling h67 RIGHT (dir = 0)
//   level 2: B is the RIGHT child of (A,B) -> sibling A on the LEFT (dir = 1)
fun proofForM5(): Cell<MerkleStep>? {
    val top: MerkleStep = {
        dir: 1,
        sibling: tree8Left(),
        next: null,
    };
    val mid: MerkleStep = {
        dir: 0,
        sibling: hashPair(M6, M7),
        next: top.toCell(),
    };
    val bottom: MerkleStep = {
        dir: 1,
        sibling: M4,
        next: mid.toCell(),
    };
    return bottom.toCell();
}

fun main() {
    val deployer = scripts.wallet("deployer");

    val vaultAddr = requireAddr("VAULT_ADDR");
    val escrowAddr = requireAddr("ESCROW_ADDR");
    val anchorAddr = requireAddr("ANCHOR_ADDR");
    val globalParamsAddr = requireAddr("GLOBAL_PARAMS_ADDR");

    println("::CHECK::wallet={}", deployer.address);

    // -- 0. GlobalParams: admin update + governance blocklist -------------------
    // The platform-wide config contract (BLOCKCHAIN_ECONOMICS §12) is editable in
    // place by its admin (the deployer), at a STABLE address. The matching
    // NON-admin rejection is exercised last (section 6) so its bouncing tx cannot
    // disrupt these happy-path checks.
    val gp = GlobalParams.fromAddress(globalParamsAddr);
    val viewBefore = gp.getParams();
    val curParams = viewBefore.params.load();

    // (a) Admin update_params, persisted at the same address. Toggle the fee bps
    // to a deterministic-but-different value so the write is provable on a re-run.
    var updated = curParams;
    val newFeeBps = (curParams.platformFeeBps >= 300 ? 250 : 350) as uint16;
    updated.platformFeeBps = newFeeBps;
    val updRes = gp.sendUpdateParams(
        deployer.address,
        100,
        viewBefore.feeRecipient,
        updated,
        { value: ton("0.1") },
    );
    updRes.waitForTrace();
    val same = GlobalParams.fromAddress(globalParamsAddr);
    val viewAfter = same.getParams();
    val persisted = viewAfter.params.load().platformFeeBps == newFeeBps;
    val adminStable = same.getAdmin() == deployer.address;
    val addrStable = same.address == globalParamsAddr;
    println(
        "::CHECK::globalparams_update::{}::fee_bps={}::addr_stable={}",
        (persisted && adminStable && addrStable) ? "PASS" : "FAIL",
        newFeeBps,
        addrStable,
    );

    // (b) Governance blocklist round-trip (admin-gated, mutates in place).
    val subject = 0x1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0;
    gp.sendSetBlocklisted(deployer.address, 102, subject, true, { value: ton("0.1") }).waitForTrace();
    val blockedNow = gp.getBlocklisted(subject);
    gp.sendSetBlocklisted(deployer.address, 103, subject, false, { value: ton("0.1") }).waitForTrace();
    val clearedNow = gp.getBlocklisted(subject);
    println(
        "::CHECK::globalparams_blocklist::{}::blocked={}::cleared={}",
        (blockedNow && !clearedNow) ? "PASS" : "FAIL",
        blockedNow,
        clearedNow,
    );

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

    // -- 1b. Stake-receipt jetton: minted 1:1 + transfer-locked (§8.5) -------
    // Explicit live verification (item 5): the StakeReceiptWallet was minted 1:1
    // with the bonded TON, and a transfer attempt is rejected (anti-exit), so the
    // balance is unchanged afterwards.
    val receiptAddr = vault.getReceiptWalletAddress(deployer.address);
    val receipt = StakeReceiptWallet.fromAddress(receiptAddr);
    val wd0 = receipt.getWalletData();
    val locked = receipt.isTransferLocked();
    println("::CHECK::jetton::minted={}::staked={}::locked={}", wd0.balance, vs.staked, locked);

    // Attempt a standard TEP-74 transfer to dodge slashing -> must be rejected;
    // re-read the balance to confirm it did not move.
    val xferRes = receipt.sendJettonTransfer(
        deployer.address,
        11,
        wd0.balance,
        deployer.address,
        deployer.address,
        null,
        ton("0"),
        beginCell().endCell().beginParse(),
        { value: ton("0.1") },
    );
    xferRes.waitForTrace();
    val wd1 = receipt.getWalletData();
    println(
        "::CHECK::jetton_transfer::before={}::after={}::rejected={}",
        wd0.balance,
        wd1.balance,
        wd1.balance == wd0.balance,
    );

    // -- 1c. Duckton TEP-64 metadata on the (freshly redeployed) vault -------
    // Assert get_jetton_data exposes the on-chain Duckton metadata: the content
    // cell is rebuilt INLINE from the SAME builder the contract uses (name
    // "Duckton", symbol "DUCKTON", decimals 9, description) and its hash must
    // match; the master is the vault and the receipt-wallet code is the deployed
    // StakeReceiptWallet. A stale (pre-Duckton) vault would deploy at a different
    // deterministic address, so a matching content hash here proves the harness
    // deployed the CURRENT Duckton-enabled StakeVault.
    val jd = vault.getJettonData();
    val expectedContent = buildReceiptJettonContent();
    val metaMatch = jd.jettonContent.hash() == expectedContent.hash()
        && jd.mintable
        && jd.adminAddress == vaultAddr
        && jd.jettonWalletCode.hash() == build("StakeReceiptWallet").hash();
    println(
        "::CHECK::duckton_metadata::{}::name=Duckton::symbol=DUCKTON::decimals={}",
        metaMatch ? "PASS" : "FAIL",
        RECEIPT_JETTON_DECIMALS,
    );

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

    // -- 4b. Multi-leaf Merkle inclusion anchored + verified on-chain --------
    // Anchor a realistic multi-record (8-leaf, 3-level) epoch root chained to the
    // single-leaf epoch just submitted, then verify the interior leaf M5's
    // mixed-direction inclusion proof against it via the contract get-method. A
    // tampered leaf must NOT verify. Mirrors `tests/anchor.test.tolk`.
    val mlRoot = tree8Root();
    val mlEpoch = (after.currentEpoch + 1) as uint32;
    val mlSubmit = anchor.sendAnchorSubmitRoot(
        deployer.address,
        31,
        mlEpoch,
        mlRoot,
        after.lastRoot,
        stakeWeight,
        { value: ton("0.1") },
    );
    mlSubmit.waitForTrace();
    val mlAfter = anchor.getAnchorState();
    val mlAnchored = mlAfter.currentEpoch == mlEpoch && mlAfter.lastRoot == mlRoot;
    val mlIncluded = anchor.verifyInclusion(M5, mlRoot, proofForM5());
    val mlTampered = anchor.verifyInclusion(M5 + 1, mlRoot, proofForM5());
    println(
        "::CHECK::multileaf_inclusion::{}::epoch={}::included={}::tampered_rejected={}",
        (mlAnchored && mlIncluded && !mlTampered) ? "PASS" : "FAIL",
        mlAfter.currentEpoch,
        mlIncluded,
        !mlTampered,
    );

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

    // -- 6. GlobalParams non-admin update_params is rejected -----------------
    // Run LAST so a (correctly) rejected, bouncing transaction cannot disrupt the
    // earlier happy-path checks. We only sign as the deployer, so to exercise a
    // NON-admin sender we target a throwaway GlobalParams whose admin is the
    // (deterministic) vault address: the deployer is not its admin, so the update
    // must be rejected and the decoy's params must stay at the seeded default
    // (200 bps). The decoy address is deterministic, so a re-run is a harmless
    // top-up.
    val decoyStorage: GlobalParamsStorage = {
        admin: vaultAddr,
        feeRecipient: vaultAddr,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
    };
    val decoy = GlobalParams.fromStorage(decoyStorage);
    decoy.deploy(deployer.address, { value: ton("0.1") }).waitForTrace();
    var evil = validParams();
    evil.platformFeeBps = 9999;
    decoy.sendUpdateParams(
        deployer.address,
        101,
        deployer.address,
        evil,
        { value: ton("0.1") },
    ).waitForTrace();
    val decoyFeeBps = GlobalParams.fromAddress(decoy.address).getParams().params.load().platformFeeBps;
    println(
        "::CHECK::globalparams_nonadmin::{}::decoy_fee_bps={}",
        decoyFeeBps == 200 ? "PASS" : "FAIL",
        decoyFeeBps,
    );

    println("::CHECK::done");
}
