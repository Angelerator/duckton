import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/common"
import "@contracts/escrow_types"
import "@wrappers/JobEscrow.gen"

const T0: uint32 = 1_700_000_000
const DEADLINE: uint32 = 1_700_000_500
const RESULT_HASH: uint256 = 0xABCDEF0123456789

// B1: build the requester-committed candidate set from an explicit list and the
// matching commitment used in `EscrowTerms.candidatesHash`.
fun candidatesOf2(a: address, b: address): map<address, uint1> {
    var c: map<address, uint1> = createEmptyMap();
    c.set(a, 1);
    c.set(b, 1);
    return c;
}

fun candidatesOf3(a: address, b: address, d: address): map<address, uint1> {
    var c: map<address, uint1> = createEmptyMap();
    c.set(a, 1);
    c.set(b, 1);
    c.set(d, 1);
    return c;
}

fun deployEscrowC(
    requester: address,
    arbiter: address,
    treasury: address,
    amount: coins,
    candidates: map<address, uint1>,
): JobEscrow {
    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: RESULT_HASH,
        candidatesHash: candidatesCommitment(candidates),
        paramsVersion: 1,
    };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: amount,
        deadline: DEADLINE,
        settled: false,
        terms: terms.toCell(),
        pending: createEmptyMap(),
    };
    val escrow = JobEscrow.fromStorage(storage);
    // Requester pre-funds the escrow with B (+ gas) at deploy (§8.2).
    val res = escrow.deploy(requester, { value: amount + ton("2") });
    expect(res).toHaveSuccessfulDeploy({ to: escrow.address });
    return escrow;
}

get fun `test settle on correct quorum hash pays winner, commissions, fee and refunds slack`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    // Payout recipients are deployed wallets (production reality): bounceable
    // sends require an initialized recipient, else they fall to the pull-ledger.
    val winner = testing.treasury("winner");
    val p1 = testing.treasury("p1");
    val p2 = testing.treasury("p2");
    val cands = candidatesOf3(winner.address, p1.address, p2.address);
    val escrow = deployEscrowC(requester.address, arbiter.address, treasury.address, ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    participants.set(p2.address, ton("1"));

    val wBefore = testing.getAccountBalance(winner.address);
    val tBefore = testing.getAccountBalance(treasury.address);
    val p1Before = testing.getAccountBalance(p1.address);
    val p2Before = testing.getAccountBalance(p2.address);

    // winner 60 + fee 2 + commissions 2 = 64; refund 36 to requester.
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("2"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });

    expect(escrow.getEscrowState().settled).toBeTrue();
    // Bounceable payouts carry a small op-tagged body, so the receiving wallet
    // spends a few microTON of gas — assert tight ranges (recipient nets ~amount).
    val wd = testing.getAccountBalance(winner.address) - wBefore;
    val td = testing.getAccountBalance(treasury.address) - tBefore;
    val p1d = testing.getAccountBalance(p1.address) - p1Before;
    val p2d = testing.getAccountBalance(p2.address) - p2Before;
    expect(wd).toBeGreaterOrEqual(ton("59.99"));
    expect(wd).toBeLessOrEqual(ton("60"));
    expect(td).toBeGreaterOrEqual(ton("1.99"));
    expect(td).toBeLessOrEqual(ton("2"));
    expect(p1d).toBeGreaterOrEqual(ton("0.99"));
    expect(p1d).toBeLessOrEqual(ton("1"));
    expect(p2d).toBeGreaterOrEqual(ton("0.99"));
    expect(p2d).toBeLessOrEqual(ton("1"));
}

get fun `test settle with wrong result hash is rejected (HTLC lock)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        0xDEAD,
        winner.address,
        ton("60"),
        ton("2"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.HASH_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test settle from non-arbiter is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        requester.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("2"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: requester.address, to: escrow.address, exitCode: ErrCode.NOT_ARBITER });
}

// B1 [HIGH]: a compromised arbiter CANNOT redirect funds to itself or any address
// outside the requester-committed candidate set.
get fun `test arbiter cannot redirect payout to itself or a non-candidate (B1)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val outsider = testing.treasury("outsider");
    val cands = candidatesOf2(winner.address, randomAddress("x2"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);
    var participants: map<address, coins> = createEmptyMap();

    // (1) arbiter names ITSELF as the winner -> rejected (winner != arbiter).
    val toSelf = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, arbiter.address, ton("90"), ton("0"), participants, cands, { value: ton("1") },
    );
    expect(toSelf).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.NOT_OWNER_SIG });
    expect(escrow.getEscrowState().settled).toBeFalse();

    // (2) arbiter names an OUTSIDER not in the committed candidate set -> rejected.
    val toOutsider = escrow.sendEscrowSettle(
        arbiter.address, 2, RESULT_HASH, outsider.address, ton("90"), ton("0"), participants, cands, { value: ton("1") },
    );
    expect(toOutsider).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();

    // (3) arbiter forges a DIFFERENT candidate set (one that includes itself) ->
    // the commitment check (which runs before the winner!=arbiter check) catches
    // it first: SETTLE_POLICY_MISMATCH. Either way the arbiter cannot self-pay.
    val forged = candidatesOf2(arbiter.address, winner.address);
    val toForged = escrow.sendEscrowSettle(
        arbiter.address, 3, RESULT_HASH, arbiter.address, ton("90"), ton("0"), participants, forged, { value: ton("1") },
    );
    expect(toForged).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

// B1: a participant outside the committed set is rejected.
get fun `test settle with a non-candidate participant is rejected (B1)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    participants.set(randomAddress("intruder"), ton("1")); // not in cands
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner.address, ton("60"), ton("0"), participants, cands, { value: ton("1") },
    );
    expect(res).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test payout exceeding escrow is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("150"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
}

get fun `test refund before deadline is rejected, after deadline succeeds`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val keeper = randomAddress("keeper");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    // Before the deadline, refund-on-timeout is not allowed.
    val early = escrow.sendEscrowRefund(keeper, 1, { value: ton("1") });
    expect(
        early,
    ).toHaveFailedTx({ from: keeper, to: escrow.address, exitCode: ErrCode.TIMEOUT_NOT_REACHED });

    // After the deadline, anyone may trigger a full refund to the requester.
    testing.setNow(DEADLINE);
    val res = escrow.sendEscrowRefund(keeper, 2, { value: ton("1") });
    expect(res).toHaveSuccessfulTx<EscrowRefund>({ from: keeper, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();

    // A settle after a timeout refund is rejected (already settled).
    var participants: map<address, coins> = createEmptyMap();
    val late = escrow.sendEscrowSettle(
        arbiter.address,
        3,
        RESULT_HASH,
        winner.address,
        ton("10"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        late,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.ALREADY_SETTLED });
}

// ===========================================================================
// Hardening: focused negative / boundary tests that kill surviving mutants on
// the per-job escrow money rules (payout-bound, refund slack, settle guards).
// ===========================================================================

get fun `test refund after a successful settle is rejected (already settled)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val keeper = randomAddress("keeper");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    // Settle first.
    var participants: map<address, coins> = createEmptyMap();
    val settle = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner.address, ton("10"), ton("0"), participants, cands, { value: ton("1") },
    );
    expect(settle).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });

    // A refund after the deadline must now be rejected (the settled guard is
    // checked before the deadline gate), pinning EscrowRefund's `!settled`.
    testing.setNow(DEADLINE);
    val refund = escrow.sendEscrowRefund(keeper, 2, { value: ton("1") });
    expect(refund).toHaveFailedTx({ from: keeper, to: escrow.address, exitCode: ErrCode.ALREADY_SETTLED });
}

get fun `test payout total over escrow (winner+fee+participants) is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val p1 = testing.treasury("p1");
    val cands = candidatesOf2(winner.address, p1.address);
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    // winner 80 + fee 10 + participant 20 = 110 > 100. Each summand must count:
    // this pins both `+` operators in the total and the participant-summing loop
    // (dropping fee, participants, or the loop would let 110 slip under 100).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("20"));
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner.address, ton("80"), ton("10"), participants, cands, { value: ton("1") },
    );
    expect(res).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test payout total exactly equal to escrow is accepted (boundary)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val p1 = testing.treasury("p1");
    val cands = candidatesOf2(winner.address, p1.address);
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    // winner 98 + fee 1 + participant 1 = 100 == escrow: must succeed (pins the
    // `<=` boundary in `total <= escrowAmount`; a strict `<` would reject this).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner.address, ton("98"), ton("1"), participants, cands, { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();
}

// B3: the requester's slack refund is paid via RAWRESERVE + CARRY_ALL_BALANCE, so
// it is the TRUE remainder (never a stale computed slack underpaid by rent).
get fun `test settle refunds the unspent slack to the requester`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val p1 = testing.treasury("p1");
    val p2 = testing.treasury("p2");
    val cands = candidatesOf3(winner.address, p1.address, p2.address);
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    // winner 60 + fee 2 + commissions 2 = 64; the remaining ~36 must refund to the
    // requester (pins the carry-remaining refund — without it the requester would
    // receive nothing).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    participants.set(p2.address, ton("1"));
    val before = testing.getAccountBalance(requester.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner.address, ton("60"), ton("2"), participants, cands, { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    // The requester recovers the TRUE remainder via carry-all: the 36 unspent
    // slack PLUS the ~2 TON over-funded deploy gas, minus the reserved buffer.
    val delta = testing.getAccountBalance(requester.address) - before;
    expect(delta).toBeGreaterOrEqual(ton("35.5"));
    expect(delta).toBeLessOrEqual(ton("39"));
}

// B2 [HIGH]: a payout whose recipient cannot accept it BOUNCES and is credited to
// the pull-ledger (not silently lost); the recipient can then claim it.
get fun `test bounced payout is recoverable via the pull-ledger (B2)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    // An UNINITIALIZED winner: a bounceable payout to it bounces back.
    val winner = randomAddress("uninit_winner");
    val cands = candidatesOf2(winner, randomAddress("x"));
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), cands);

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, winner, ton("60"), ton("0"), participants, cands, { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    // The ~60 TON was NOT lost: it is queued in the pull-ledger for the winner
    // (the bounce itself costs a tiny forward fee, so it is just under 60).
    expect(escrow.getPending(winner)).toBeGreaterOrEqual(ton("59.99"));
    expect(escrow.getPending(winner)).toBeLessOrEqual(ton("60"));

    // Anyone can trigger the claim, but it can only go to the keyed recipient.
    val claim = escrow.sendEscrowClaim(requester.address, 2, winner, { value: ton("1") });
    expect(claim).toHaveSuccessfulTx<EscrowClaim>({ from: requester.address, to: escrow.address });
    // After claim, it re-sends (and bounces again to the still-uninit winner), so
    // the ledger is re-credited (minus a little bounce fee) — proving funds are
    // never lost across the round trip; the credit stays ~60 TON.
    expect(escrow.getPending(winner)).toBeGreaterOrEqual(ton("59.9"));
    expect(escrow.getPending(winner)).toBeLessOrEqual(ton("60"));
}

get fun `test escrow rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), candidatesOf2(randomAddress("w"), randomAddress("x")));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = escrow.sendAny(arbiter.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test escrow getters and top-up cover read paths`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrowC(requester.address, arbiter.address, randomAddress("t"), ton("100"), candidatesOf2(randomAddress("w"), randomAddress("x")));

    expect(escrow.getRequester()).toEqual(requester.address);
    expect(escrow.getExpectedHash()).toEqual(RESULT_HASH);
    expect(escrow.getPending(randomAddress("nobody"))).toEqual(0);

    // A plain top-up (additional funding / storage rent) is accepted.
    val top = escrow.sendEscrowTopUp(requester.address, 1, { value: ton("0.5") });
    expect(top).toHaveSuccessfulTx<EscrowTopUp>({ from: requester.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeFalse();
}
