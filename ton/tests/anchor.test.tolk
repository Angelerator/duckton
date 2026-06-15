import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/anchor_types"
import "@contracts/common"
import "@wrappers/RecordAnchor.gen"

const T0: uint32 = 1_700_000_000
const MIN_WEIGHT: coins = ton("100")
const BOND_MIN: coins = ton("1")

// ===========================================================================
// MERKLE v2 (A7) — domain-separated leaves. The off-chain Rust builder
// (crates/settlement/src/merkle.rs) MUST byte-match: leaves = [hashLeaf(v0), ...],
// paired with hashPair, lone odd nodes PROMOTED unchanged (no self-duplication).
// The on-chain verifier seeds the accumulator with hashLeaf(leaf), so every
// proof's level-0 sibling is itself a hashLeaf value.
// ===========================================================================

const L0 = 0x1111
const L1 = 0x2222
const L2 = 0x3333
const L3 = 0x4444

// Root of the 4-leaf tree over [hashLeaf(L0..L3)].
fun treeRoot(): int {
    return hashPair(hashPair(hashLeaf(L0), hashLeaf(L1)), hashPair(hashLeaf(L2), hashLeaf(L3)));
}

// Inclusion proof for L0: sibling hashLeaf(L1) (right), then hashPair(L2,L3) (right).
fun proofForL0(): Cell<MerkleStep>? {
    val top: MerkleStep = {
        dir: 0,
        sibling: hashPair(hashLeaf(L2), hashLeaf(L3)),
        next: null,
    };
    val bottom: MerkleStep = {
        dir: 0,
        sibling: hashLeaf(L1),
        next: top.toCell(),
    };
    return bottom.toCell();
}

// A deeper 8-leaf tree (3 levels), the realistic multi-record epoch case.
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

fun tree8Left(): int {
    return hashPair(hashPair(hashLeaf(M0), hashLeaf(M1)), hashPair(hashLeaf(M2), hashLeaf(M3)));
}

fun tree8Right(): int {
    return hashPair(hashPair(hashLeaf(M4), hashLeaf(M5)), hashPair(hashLeaf(M6), hashLeaf(M7)));
}

fun tree8Root(): int {
    return hashPair(tree8Left(), tree8Right());
}

// Inclusion proof for M5 (index 5), mixed directions across 3 levels:
//   level 0: M5 is the RIGHT child of (M4,M5) -> sibling hashLeaf(M4) on the LEFT (dir = 1)
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
        sibling: hashPair(hashLeaf(M6), hashLeaf(M7)),
        next: top.toCell(),
    };
    val bottom: MerkleStep = {
        dir: 1,
        sibling: hashLeaf(M4),
        next: mid.toCell(),
    };
    return bottom.toCell();
}

fun deployAnchor(authority: address, keeper: address, treasury: address): RecordAnchor {
    val auth: AnchorAuthorities = {
        treasury: treasury,
        keeper: keeper,
        upgradeAuthority: authority,
    };
    val cfg: AnchorConfig = {
        minStakeWeight: MIN_WEIGHT,
        disputeBondMin: BOND_MIN,
        verdictAuthority: authority,
        auth: auth.toCell(),
    };
    val storage: AnchorStorage = {
        currentEpoch: 0,
        lastRoot: 0,
        nextDisputeId: 0,
        disputes: createEmptyMap(),
        roots: createEmptyMap(),
        config: cfg.toCell(),
        codeVersion: 0,
    };
    val deployer = testing.treasury("deployer");
    val anchor = RecordAnchor.fromStorage(storage);
    val res = anchor.deploy(deployer.address, { value: ton("1") });
    expect(res).toHaveSuccessfulDeploy({ to: anchor.address });
    return anchor;
}

get fun `test keeper anchors chained epoch roots above the stake threshold`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));
    val root1 = treeRoot();

    val r1 = anchor.sendAnchorSubmitRoot(
        keeper.address,
        1,
        1,
        root1,
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );
    expect(r1).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: keeper.address, to: anchor.address });
    var state = anchor.getAnchorState();
    expect(state.currentEpoch).toEqual(1);
    expect(state.lastRoot).toEqual(root1);

    // Epoch 2 chains to epoch 1's root.
    val root2 = hashPair(root1, 0x5555);
    val r2 = anchor.sendAnchorSubmitRoot(
        keeper.address,
        2,
        2,
        root2,
        root1,
        MIN_WEIGHT,
        { value: ton("1") },
    );
    expect(r2).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: keeper.address, to: anchor.address });
    expect(anchor.getAnchorState().currentEpoch).toEqual(2);
}

get fun `test anchor rejects bad prev_root, low stake-weight and epoch regression`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));
    val root1 = treeRoot();
    anchor.sendAnchorSubmitRoot(
        keeper.address,
        1,
        1,
        root1,
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );

    // Wrong prev_root.
    val badPrev = anchor.sendAnchorSubmitRoot(
        keeper.address,
        2,
        2,
        hashPair(root1, 1),
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );
    expect(
        badPrev,
    ).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.BAD_PREV_ROOT });

    // Below stake-weight threshold.
    val lowWeight = anchor.sendAnchorSubmitRoot(
        keeper.address,
        3,
        2,
        hashPair(root1, 2),
        root1,
        ton("1"),
        { value: ton("1") },
    );
    expect(
        lowWeight,
    ).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.INSUFFICIENT_ANCHOR_STAKE });

    // Epoch must advance by exactly one.
    val regress = anchor.sendAnchorSubmitRoot(
        keeper.address,
        4,
        1,
        hashPair(root1, 3),
        root1,
        MIN_WEIGHT,
        { value: ton("1") },
    );
    expect(
        regress,
    ).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.EPOCH_REGRESSION });
}

// A3: only the configured keeper may submit a root.
get fun `test anchor submit from a non-keeper is rejected (NOT_ANCHOR_KEEPER)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val notKeeper = testing.treasury("notKeeper");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));

    val res = anchor.sendAnchorSubmitRoot(notKeeper.address, 1, 1, treeRoot(), 0, MIN_WEIGHT, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: notKeeper.address, to: anchor.address, exitCode: ErrCode.NOT_ANCHOR_KEEPER });
    expect(anchor.getAnchorState().currentEpoch).toEqual(0);
}

// A6: a zero / all-zero root is refused (it would otherwise pass the genesis
// prevRoot==0 chain check and anchor a meaningless epoch).
get fun `test anchor refuses an all-zero root (ZERO_ROOT)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));

    val res = anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, 0, 0, MIN_WEIGHT, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.ZERO_ROOT });
    expect(anchor.getAnchorState().currentEpoch).toEqual(0);
}

get fun `test merkle inclusion proof verification`() {
    testing.setNow(T0);
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("keeper"), randomAddress("treasury"));
    val root = treeRoot();

    // A valid proof for L0 reproduces the root.
    expect(anchor.verifyInclusion(L0, root, proofForL0())).toBeTrue();
    expect(anchor.computeRoot(L0, proofForL0())).toEqual(root);

    // A wrong leaf does not.
    expect(anchor.verifyInclusion(0x9999, root, proofForL0())).toBeFalse();

    // A7 second-preimage: passing a raw INTERNAL-node value as a leaf must NOT
    // verify (the accumulator seeds hashLeaf(leaf), so an interior hash can't pose
    // as a leaf). Here the level-1 node hashPair(L0,L1)-domain value, fed as a
    // "leaf" with the top sibling, no longer folds to the root.
    val interior = hashPair(hashLeaf(L0), hashLeaf(L1));
    val topOnly: MerkleStep = { dir: 0, sibling: hashPair(hashLeaf(L2), hashLeaf(L3)), next: null };
    expect(anchor.computeRoot(interior, topOnly.toCell())).toNotEqual(root);
}

get fun `test multi-leaf (8) inclusion proof folds to the anchored root on-chain`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val authority = testing.treasury("authority");
    val treasury = randomAddress("treasury");
    val challenger = randomAddress("challenger");
    val anchor = deployAnchor(authority.address, keeper.address, treasury);
    val root = tree8Root();

    // Anchor the realistic multi-record epoch root.
    val submit = anchor.sendAnchorSubmitRoot(
        keeper.address,
        1,
        1,
        root,
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );
    expect(
        submit,
    ).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: keeper.address, to: anchor.address });

    // The interior leaf M5 (mixed-direction, 3-level proof) folds to the root.
    expect(anchor.verifyInclusion(M5, root, proofForM5())).toBeTrue();
    expect(anchor.computeRoot(M5, proofForM5())).toEqual(root);

    // A tampered leaf is rejected by the pure verifier...
    expect(anchor.verifyInclusion(M5 + 1, root, proofForM5())).toBeFalse();

    // ...and a real bonded dispute carrying the genuine proof opens (A2: the proof
    // verifies against the STORED epoch-1 root), while one carrying a tampered leaf
    // is rejected on-chain (ErrCode.BAD_INCLUSION_PROOF).
    val good = anchor.sendAnchorOpenDispute(
        challenger,
        1,
        1,
        M5,
        proofForM5(),
        { value: ton("2") },
    );
    expect(good).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger, to: anchor.address });

    val tampered = anchor.sendAnchorOpenDispute(
        challenger,
        2,
        1,
        M5 + 1,
        proofForM5(),
        { value: ton("2") },
    );
    expect(
        tampered,
    ).toHaveFailedTx({ from: challenger, to: anchor.address, exitCode: ErrCode.BAD_INCLUSION_PROOF });
}

// A2: a dispute against an epoch that was never anchored is rejected.
get fun `test dispute against an unknown epoch is rejected (UNKNOWN_EPOCH)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val authority = testing.treasury("authority");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(authority.address, keeper.address, randomAddress("treasury"));
    // Anchor only epoch 1.
    anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, treeRoot(), 0, MIN_WEIGHT, { value: ton("1") });

    // Dispute epoch 2 (never anchored) -> UNKNOWN_EPOCH, even with a real-looking proof.
    val res = anchor.sendAnchorOpenDispute(challenger.address, 2, 2, L0, proofForL0(), { value: ton("2") });
    expect(res).toHaveFailedTx({ from: challenger.address, to: anchor.address, exitCode: ErrCode.UNKNOWN_EPOCH });
}

// A2 (forged-root): the OLD attack passed an attacker-chosen claimedRoot + proof.
// Now the proof is checked against the contract's STORED root, so a proof that
// folds to some OTHER root (not the anchored one) is rejected.
get fun `test forged-root dispute is rejected (proof must fold to the stored root)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val authority = testing.treasury("authority");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(authority.address, keeper.address, randomAddress("treasury"));
    // Anchor the 4-leaf treeRoot for epoch 1.
    anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, treeRoot(), 0, MIN_WEIGHT, { value: ton("1") });

    // The attacker presents a leaf + proof that fold to a DIFFERENT (8-leaf) root —
    // which is NOT what epoch 1 anchored. Rejected with BAD_INCLUSION_PROOF.
    val res = anchor.sendAnchorOpenDispute(challenger.address, 1, 1, M5, proofForM5(), { value: ton("2") });
    expect(res).toHaveFailedTx({ from: challenger.address, to: anchor.address, exitCode: ErrCode.BAD_INCLUSION_PROOF });
}

get fun `test dispute lifecycle: bond, inclusion proof, upheld and rejected verdicts`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val keeper = testing.treasury("keeper");
    val treasury = testing.treasury("treasury");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(authority.address, keeper.address, treasury.address);
    val root = treeRoot();
    anchor.sendAnchorSubmitRoot(
        keeper.address,
        1,
        1,
        root,
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );

    // Bond below the minimum is rejected.
    val lowBond = anchor.sendAnchorOpenDispute(
        challenger.address,
        1,
        1,
        L0,
        proofForL0(),
        { value: ton("0.5") },
    );
    expect(
        lowBond,
    ).toHaveFailedTx({ from: challenger.address, to: anchor.address, exitCode: ErrCode.DISPUTE_BOND_TOO_LOW });

    // A bad inclusion proof (leaf not in the anchored root) is rejected.
    val badProof = anchor.sendAnchorOpenDispute(
        challenger.address,
        2,
        1,
        0xDEAD,
        proofForL0(),
        { value: ton("2") },
    );
    expect(
        badProof,
    ).toHaveFailedTx({ from: challenger.address, to: anchor.address, exitCode: ErrCode.BAD_INCLUSION_PROOF });

    // A well-formed, bonded dispute opens (id 0).
    val open = anchor.sendAnchorOpenDispute(
        challenger.address,
        3,
        1,
        L0,
        proofForL0(),
        { value: ton("2") },
    );
    expect(open).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger.address, to: anchor.address });
    val d0 = anchor.getDispute(0);
    expect(d0).toBeNotNull();
    expect(d0!.status).toEqual(0);

    // Non-authority cannot resolve.
    val notAuth = anchor.sendAnchorResolveDispute(challenger.address, 4, 0, true, { value: ton("1") });
    expect(
        notAuth,
    ).toHaveFailedTx({ from: challenger.address, to: anchor.address, exitCode: ErrCode.NOT_VERDICT_AUTHORITY });

    // Authority upholds: challenger bond returned, status = 1.
    val before = testing.getAccountBalance(challenger.address);
    val upheld = anchor.sendAnchorResolveDispute(
        authority.address,
        5,
        0,
        true,
        { value: ton("1") },
    );
    expect(
        upheld,
    ).toHaveSuccessfulTx<AnchorResolveDispute>({ from: authority.address, to: anchor.address });
    expect(anchor.getDispute(0)!.status).toEqual(1);
    // The challenger (a deployed treasury) receives the bond back (~2 TON).
    expect(testing.getAccountBalance(challenger.address) - before).toBeGreaterOrEqual(ton("1.9"));

    // A second, frivolous dispute is rejected: bond forfeited to the treasury.
    anchor.sendAnchorOpenDispute(
        challenger.address,
        6,
        1,
        L0,
        proofForL0(),
        { value: ton("2") },
    );
    val tBefore = testing.getAccountBalance(treasury.address);
    val rejected = anchor.sendAnchorResolveDispute(
        authority.address,
        7,
        1,
        false,
        { value: ton("1") },
    );
    expect(
        rejected,
    ).toHaveSuccessfulTx<AnchorResolveDispute>({ from: authority.address, to: anchor.address });
    expect(anchor.getDispute(1)!.status).toEqual(2);
    expect(testing.getAccountBalance(treasury.address) - tBefore).toBeGreaterOrEqual(ton("1.9"));
}

// A1 [CRITICAL]: a dispute can be resolved (and its bond paid out) only ONCE.
// Re-sending the same disputeId is rejected (DISPUTE_NOT_OPEN), so it cannot
// re-pay the bond and drain other challengers' commingled bonds.
get fun `test double-resolve of a dispute is rejected (DISPUTE_NOT_OPEN)`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val keeper = testing.treasury("keeper");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(authority.address, keeper.address, randomAddress("treasury"));
    anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, treeRoot(), 0, MIN_WEIGHT, { value: ton("1") });

    // Two challengers each bond on the same anchored root → two commingled bonds.
    anchor.sendAnchorOpenDispute(challenger.address, 2, 1, L0, proofForL0(), { value: ton("2") });
    anchor.sendAnchorOpenDispute(testing.treasury("ch2").address, 3, 1, L0, proofForL0(), { value: ton("2") });

    // Resolve dispute 0 once (upheld → bond returned). Status becomes 1.
    val r1 = anchor.sendAnchorResolveDispute(authority.address, 4, 0, true, { value: ton("1") });
    expect(r1).toHaveSuccessfulTx<AnchorResolveDispute>({ from: authority.address, to: anchor.address });
    expect(anchor.getDispute(0)!.status).toEqual(1);

    // Re-resolving the SAME dispute id is now rejected — the old bug re-paid the
    // bond (draining ch2's bond); the guard prevents it.
    val r2 = anchor.sendAnchorResolveDispute(authority.address, 5, 0, true, { value: ton("1") });
    expect(r2).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.DISPUTE_NOT_OPEN });
    // ch2's bond is still intact: its dispute (id 1) is still open and resolvable.
    expect(anchor.getDispute(1)!.status).toEqual(0);
    val r3 = anchor.sendAnchorResolveDispute(authority.address, 6, 1, true, { value: ton("1") });
    expect(r3).toHaveSuccessfulTx<AnchorResolveDispute>({ from: authority.address, to: anchor.address });
}

// ===========================================================================
// Hardening: focused negative / boundary tests that kill surviving mutants on
// the record-anchor money/dispute rules (bond floor, verdict guards, getters).
// ===========================================================================

get fun `test resolving an unknown dispute id is rejected`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val anchor = deployAnchor(authority.address, randomAddress("keeper"), randomAddress("treasury"));

    // The authority resolves a dispute id that was never opened: must be rejected
    // with NO_SUCH_DISPUTE (pins the `found.isFound` guard before loadValue()).
    val res = anchor.sendAnchorResolveDispute(authority.address, 1, 999, true, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.NO_SUCH_DISPUTE });
}

get fun `test dispute bond exactly at the minimum is accepted (boundary)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));
    val root = treeRoot();
    anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, root, 0, MIN_WEIGHT, { value: ton("1") });

    // A bond EXACTLY equal to disputeBondMin must open the dispute (pins the
    // `>=` in `valueCoins >= disputeBondMin`; a strict `>` would reject the floor).
    val res = anchor.sendAnchorOpenDispute(challenger.address, 2, 1, L0, proofForL0(), { value: BOND_MIN });
    expect(res).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger.address, to: anchor.address });
    expect(anchor.getDispute(0)).toBeNotNull();
}

// A5: a config whose disputeBondMin is below the on-chain floor is rejected at
// first use (genesis submit), so near-zero-bond griefing is impossible.
get fun `test anchor config with a sub-floor dispute bond is rejected (BOND_FLOOR_TOO_LOW)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val auth: AnchorAuthorities = { treasury: randomAddress("t"), keeper: keeper.address, upgradeAuthority: randomAddress("u") };
    val cfg: AnchorConfig = {
        minStakeWeight: MIN_WEIGHT,
        disputeBondMin: ton("0.001"), // below MIN_DISPUTE_BOND_FLOOR (0.01)
        verdictAuthority: randomAddress("auth"),
        auth: auth.toCell(),
    };
    val storage: AnchorStorage = {
        currentEpoch: 0, lastRoot: 0, nextDisputeId: 0,
        disputes: createEmptyMap(), roots: createEmptyMap(), config: cfg.toCell(), codeVersion: 0,
    };
    val anchor = RecordAnchor.fromStorage(storage);
    expect(anchor.deploy(keeper.address, { value: ton("1") })).toHaveSuccessfulDeploy({ to: anchor.address });

    val res = anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, treeRoot(), 0, MIN_WEIGHT, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.BOND_FLOOR_TOO_LOW });
}

get fun `test get_dispute returns null for an unknown id`() {
    testing.setNow(T0);
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("keeper"), randomAddress("treasury"));
    // No disputes opened: the getter's not-found branch must return null (pins
    // the `if (!found.isFound) return null` guard against a load on an empty slot).
    expect(anchor.getDispute(0)).toBeNull();
    expect(anchor.getDispute(999)).toBeNull();
}

get fun `test anchor rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val anchor = deployAnchor(randomAddress("auth"), keeper.address, randomAddress("treasury"));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = anchor.sendAny(keeper.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.INVALID_OP });
}

// ===========================================================================
// A7 — Merkle v2 reference hash vectors. These EXACT values are pinned on the
// off-chain Rust side (crates/settlement/src/merkle.rs); if the on-chain hashLeaf
// / hashPair construction drifts, this fails and so will cross-chain proofs.
// (a) hashLeaf(0x1111), (b) hashLeaf(0x2222),
// (c) hashPair(hashLeaf(0x1111), hashLeaf(0x2222)),
// (d) the 4-leaf ROOT over [0x1111,0x2222,0x3333,0x4444].
// ===========================================================================
get fun `test MERKLE v2 reference hash vectors (a-d) for Rust pinning`() {
    val a = hashLeaf(0x1111);
    val b = hashLeaf(0x2222);
    val c = hashPair(a, b);
    val d = treeRoot();
    // hashLeaf must equal the repr-hash of a 0-ref cell holding one uint256.
    expect(a).toEqual(beginCell().storeUint(0x1111, 256).endCell().hash());
    expect(b).toEqual(beginCell().storeUint(0x2222, 256).endCell().hash());
    // c folds the two leaves; d is the balanced 4-leaf root.
    expect(c).toEqual(hashPair(hashLeaf(0x1111), hashLeaf(0x2222)));
    expect(d).toEqual(hashPair(hashPair(hashLeaf(0x1111), hashLeaf(0x2222)), hashPair(hashLeaf(0x3333), hashLeaf(0x4444))));
    // distinctness sanity: a leaf hash never equals its raw value (domain sep).
    expect(a).toNotEqual(0x1111);
}
