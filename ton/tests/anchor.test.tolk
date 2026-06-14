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

// A small 4-leaf Merkle tree built with the contract's hash domain.
const L0 = 0x1111
const L1 = 0x2222
const L2 = 0x3333
const L3 = 0x4444

fun treeRoot(): int {
    return hashPair(hashPair(L0, L1), hashPair(L2, L3));
}

// Inclusion proof for L0: sibling L1 (right), then sibling hash(L2,L3) (right).
fun proofForL0(): Cell<MerkleStep>? {
    val top: MerkleStep = {
        dir: 0,
        sibling: hashPair(L2, L3),
        next: null,
    };
    val bottom: MerkleStep = {
        dir: 0,
        sibling: L1,
        next: top.toCell(),
    };
    return bottom.toCell();
}

// A deeper 8-leaf tree (3 levels), the realistic multi-record epoch case the
// testnet e2e never exercised. Leaf values stand in for BLAKE3(JobRecord) leaves;
// the off-chain Rust builder (crates/settlement/src/merkle.rs) folds them with the
// IDENTICAL node hash (`sha256(00 80 ‖ left ‖ right)` == this `hashPair`), pinned
// by the Rust cross-check test against these same `hashPair` reference values.
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

fun tree8Left(): int {
    // A = hashPair(hashPair(M0,M1), hashPair(M2,M3))
    return hashPair(hashPair(M0, M1), hashPair(M2, M3));
}

fun tree8Right(): int {
    // B = hashPair(hashPair(M4,M5), hashPair(M6,M7))
    return hashPair(hashPair(M4, M5), hashPair(M6, M7));
}

fun tree8Root(): int {
    return hashPair(tree8Left(), tree8Right());
}

// Inclusion proof for M5 (index 5), mixed directions across 3 levels:
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

fun deployAnchor(authority: address, treasury: address): RecordAnchor {
    val cfg: AnchorConfig = {
        minStakeWeight: MIN_WEIGHT,
        disputeBondMin: BOND_MIN,
        verdictAuthority: authority,
        treasury: treasury,
    };
    val storage: AnchorStorage = {
        currentEpoch: 0,
        lastRoot: 0,
        nextDisputeId: 0,
        disputes: createEmptyMap(),
        config: cfg.toCell(),
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
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));
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
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));
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

get fun `test merkle inclusion proof verification`() {
    testing.setNow(T0);
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));
    val root = treeRoot();

    // A valid proof for L0 reproduces the root.
    expect(anchor.verifyInclusion(L0, root, proofForL0())).toBeTrue();
    expect(anchor.computeRoot(L0, proofForL0())).toEqual(root);

    // A wrong leaf does not.
    expect(anchor.verifyInclusion(0x9999, root, proofForL0())).toBeFalse();
}

get fun `test multi-leaf (8) inclusion proof folds to the anchored root on-chain`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val authority = testing.treasury("authority");
    val treasury = randomAddress("treasury");
    val challenger = randomAddress("challenger");
    val anchor = deployAnchor(authority.address, treasury);
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

    // ...and a real bonded dispute carrying the genuine proof opens, while one
    // carrying a tampered claimed root is rejected on-chain (ErrCode.BAD_INCLUSION_PROOF).
    val good = anchor.sendAnchorOpenDispute(
        challenger,
        1,
        1,
        M5,
        root,
        proofForM5(),
        { value: ton("2") },
    );
    expect(good).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger, to: anchor.address });

    val tampered = anchor.sendAnchorOpenDispute(
        challenger,
        2,
        1,
        M5 + 1,
        root,
        proofForM5(),
        { value: ton("2") },
    );
    expect(
        tampered,
    ).toHaveFailedTx({ from: challenger, to: anchor.address, exitCode: ErrCode.BAD_INCLUSION_PROOF });
}

get fun `test dispute lifecycle: bond, inclusion proof, upheld and rejected verdicts`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val treasury = randomAddress("treasury");
    val challenger = randomAddress("challenger");
    val anchor = deployAnchor(authority.address, treasury);
    val root = treeRoot();
    anchor.sendAnchorSubmitRoot(
        testing.treasury("keeper").address,
        1,
        1,
        root,
        0,
        MIN_WEIGHT,
        { value: ton("1") },
    );

    // Bond below the minimum is rejected.
    val lowBond = anchor.sendAnchorOpenDispute(
        challenger,
        1,
        1,
        L0,
        root,
        proofForL0(),
        { value: ton("0.5") },
    );
    expect(
        lowBond,
    ).toHaveFailedTx({ from: challenger, to: anchor.address, exitCode: ErrCode.DISPUTE_BOND_TOO_LOW });

    // A bad inclusion proof (wrong claimed root) is rejected.
    val badProof = anchor.sendAnchorOpenDispute(
        challenger,
        2,
        1,
        L0,
        hashPair(root, 1),
        proofForL0(),
        { value: ton("2") },
    );
    expect(
        badProof,
    ).toHaveFailedTx({ from: challenger, to: anchor.address, exitCode: ErrCode.BAD_INCLUSION_PROOF });

    // A well-formed, bonded dispute opens (id 0).
    val open = anchor.sendAnchorOpenDispute(
        challenger,
        3,
        1,
        L0,
        root,
        proofForL0(),
        { value: ton("2") },
    );
    expect(open).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger, to: anchor.address });
    val d0 = anchor.getDispute(0);
    expect(d0).toBeNotNull();
    expect(d0!.status).toEqual(0);

    // Non-authority cannot resolve.
    val notAuth = anchor.sendAnchorResolveDispute(challenger, 4, 0, true, { value: ton("1") });
    expect(
        notAuth,
    ).toHaveFailedTx({ from: challenger, to: anchor.address, exitCode: ErrCode.NOT_VERDICT_AUTHORITY });

    // Authority upholds: challenger bond returned, status = 1.
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
    expect(testing.getAccountBalance(challenger)).toBeGreaterOrEqual(ton("1"));

    // A second, frivolous dispute is rejected: bond forfeited to the treasury.
    anchor.sendAnchorOpenDispute(
        challenger,
        6,
        1,
        L0,
        root,
        proofForL0(),
        { value: ton("2") },
    );
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
    expect(testing.getAccountBalance(treasury)).toBeGreaterOrEqual(ton("2"));
}

// ===========================================================================
// Hardening: focused negative / boundary tests that kill surviving mutants on
// the record-anchor money/dispute rules (bond floor, verdict guards, getters).
// ===========================================================================

get fun `test resolving an unknown dispute id is rejected`() {
    testing.setNow(T0);
    val authority = testing.treasury("authority");
    val anchor = deployAnchor(authority.address, randomAddress("treasury"));

    // The authority resolves a dispute id that was never opened: must be rejected
    // with NO_SUCH_DISPUTE (pins the `found.isFound` guard before loadValue()).
    val res = anchor.sendAnchorResolveDispute(authority.address, 1, 999, true, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: authority.address, to: anchor.address, exitCode: ErrCode.NO_SUCH_DISPUTE });
}

get fun `test dispute bond exactly at the minimum is accepted (boundary)`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val challenger = testing.treasury("challenger");
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));
    val root = treeRoot();
    anchor.sendAnchorSubmitRoot(keeper.address, 1, 1, root, 0, MIN_WEIGHT, { value: ton("1") });

    // A bond EXACTLY equal to disputeBondMin must open the dispute (pins the
    // `>=` in `valueCoins >= disputeBondMin`; a strict `>` would reject the floor).
    val res = anchor.sendAnchorOpenDispute(challenger.address, 2, 1, L0, root, proofForL0(), { value: BOND_MIN });
    expect(res).toHaveSuccessfulTx<AnchorOpenDispute>({ from: challenger.address, to: anchor.address });
    expect(anchor.getDispute(0)).toBeNotNull();
}

get fun `test get_dispute returns null for an unknown id`() {
    testing.setNow(T0);
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));
    // No disputes opened: the getter's not-found branch must return null (pins
    // the `if (!found.isFound) return null` guard against a load on an empty slot).
    expect(anchor.getDispute(0)).toBeNull();
    expect(anchor.getDispute(999)).toBeNull();
}

get fun `test anchor rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    testing.setNow(T0);
    val keeper = testing.treasury("keeper");
    val anchor = deployAnchor(randomAddress("auth"), randomAddress("treasury"));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = anchor.sendAny(keeper.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: keeper.address, to: anchor.address, exitCode: ErrCode.INVALID_OP });
}
