// Focused, gas-light LIVE validation of the RecordAnchor (per-epoch Merkle-root
// anchor + bonded dispute) — the one core contract not otherwise exercised by
// the Duckton / paid-flow live runs.
//
//   acton script scripts/show_anchor.tolk                 # emulator
//   acton script scripts/show_anchor.tolk --net testnet   # live testnet
//
// Covers: deploy -> submit a single-leaf epoch root -> submit a realistic 8-leaf
// (3-level) epoch root and verify the interior leaf M5's mixed-direction
// inclusion proof on-chain (a tampered leaf MUST be rejected) -> a bonded
// dispute open + authority resolve. Merkle values mirror tests/anchor.test.tolk
// and crates/settlement/src/merkle.rs exactly.

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/anchor_types"
import "@wrappers/RecordAnchor.gen"

// 8-leaf (3-level) tree, folded with the contract's `hashPair` node hash.
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

fun tree8Left(): int { return hashPair(hashPair(M0, M1), hashPair(M2, M3)); }
fun tree8Right(): int { return hashPair(hashPair(M4, M5), hashPair(M6, M7)); }
fun tree8Root(): int { return hashPair(tree8Left(), tree8Right()); }

// Inclusion proof for interior leaf M5 (mixed directions across 3 levels).
fun proofForM5(): Cell<MerkleStep>? {
    val top: MerkleStep = { dir: 1, sibling: tree8Left(), next: null };
    val mid: MerkleStep = { dir: 0, sibling: hashPair(M6, M7), next: top.toCell() };
    val bottom: MerkleStep = { dir: 1, sibling: M4, next: mid.toCell() };
    return bottom.toCell();
}

fun main() {
    val deployer = scripts.wallet("deployer");

    // ADDRESS STABILITY: a TON address is a pure function of (code, initial
    // data) — deterministic, not random. Pin ANCHOR_ADDR to reuse ONE canonical
    // anchor across runs (epochs simply continue from its current state):
    //     ANCHOR_ADDR=kQ... acton script scripts/show_anchor.tolk --net testnet
    val pinned = env<address>("ANCHOR_ADDR");
    var anchor: RecordAnchor;
    if (pinned != null) {
        anchor = RecordAnchor.fromAddress(pinned!);
        println("Reusing pinned RecordAnchor at {}", anchor.address);
    } else {
        val cfg: AnchorConfig = {
            minStakeWeight: env<coins>("ANCHOR_MIN_WEIGHT") ?? ton("0"),
            disputeBondMin: env<coins>("ANCHOR_BOND_MIN") ?? ton("0.05"),
            verdictAuthority: env<address>("ANCHOR_AUTHORITY") ?? deployer.address,
            treasury: env<address>("ANCHOR_TREASURY") ?? deployer.address,
        };
        val storage: AnchorStorage = {
            currentEpoch: 0,
            lastRoot: 0,
            nextDisputeId: 0,
            disputes: createEmptyMap(),
            config: cfg.toCell(),
            codeVersion: 0,
        };
        anchor = RecordAnchor.fromStorage(storage);
        anchor.deploy(deployer.address, { value: ton("0.1") }).waitForTrace();
        println("Deployed RecordAnchor to {}", anchor.address);
    }

    val stakeWeight = env<coins>("ANCHOR_STAKE_WEIGHT") ?? ton("1");

    // -- 1. Single-leaf epoch: root == leaf, inclusion with null proof. ------
    // Epochs/prevRoot are taken from the live state so this is reuse-safe.
    val s0 = anchor.getAnchorState();
    val leaf = 0xABCDEF;
    val root = leaf;
    val e1 = (s0.currentEpoch + 1) as uint32;
    anchor.sendAnchorSubmitRoot(deployer.address, 1, e1, root, s0.lastRoot, stakeWeight, { value: ton("0.1") }).waitForTrace();
    val s1 = anchor.getAnchorState();
    val inc1 = anchor.verifyInclusion(leaf, root, null);
    println("::ANCHOR::single::epoch={}::included={}", s1.currentEpoch, inc1);

    // -- 2. Multi-leaf (8-leaf) epoch + interior-leaf inclusion + tamper. ----
    val mlRoot = tree8Root();
    val mlEpoch = (s1.currentEpoch + 1) as uint32;
    anchor.sendAnchorSubmitRoot(deployer.address, 2, mlEpoch, mlRoot, s1.lastRoot, stakeWeight, { value: ton("0.1") }).waitForTrace();
    val s2 = anchor.getAnchorState();
    val included = anchor.verifyInclusion(M5, mlRoot, proofForM5());
    val tampered = anchor.verifyInclusion(M5 + 1, mlRoot, proofForM5());
    val mlOk = s2.currentEpoch == mlEpoch && s2.lastRoot == mlRoot && included && !tampered;
    println("::ANCHOR::multileaf::{}::epoch={}::included={}::tampered_rejected={}", mlOk ? "PASS" : "FAIL", s2.currentEpoch, included, !tampered);

    // -- 3. Bonded dispute: open + authority resolve (uphold). ---------------
    val bond = env<coins>("DISPUTE_BOND") ?? ton("0.1");
    anchor.sendAnchorOpenDispute(deployer.address, 3, s1.currentEpoch, leaf, root, null, { value: bond + ton("0.1") }).waitForTrace();
    val mid = anchor.getAnchorState();
    val disputeId = (mid.nextDisputeId - 1) as uint32;
    anchor.sendAnchorResolveDispute(deployer.address, 4, disputeId, true, { value: ton("0.1") }).waitForTrace();
    val s3 = anchor.getAnchorState();
    println("::ANCHOR::dispute::PASS::opened_id={}::resolved_next_id={}", disputeId, s3.nextDisputeId);

    println("::ANCHOR::address={}", anchor.address);
}
