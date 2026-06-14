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

fun deployEscrow(
    requester: address,
    arbiter: address,
    treasury: address,
    amount: coins,
): JobEscrow {
    val terms: EscrowTerms = { treasury: treasury, expectedHash: RESULT_HASH, paramsVersion: 1 };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: amount,
        deadline: DEADLINE,
        settled: false,
        terms: terms.toCell(),
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
    val treasury = randomAddress("treasury");
    val winner = randomAddress("winner");
    val p1 = randomAddress("p1");
    val p2 = randomAddress("p2");
    val escrow = deployEscrow(requester.address, arbiter.address, treasury, ton("100"));

    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1, ton("1"));
    participants.set(p2, ton("1"));

    // winner 60 + fee 2 + commissions 2 = 64; refund 36 to requester.
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner,
        ton("60"),
        ton("2"),
        participants,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });

    expect(escrow.getEscrowState().settled).toBeTrue();
    expect(testing.getAccountBalance(winner)).toEqual(ton("60"));
    expect(testing.getAccountBalance(treasury)).toEqual(ton("2"));
    expect(testing.getAccountBalance(p1)).toEqual(ton("1"));
    expect(testing.getAccountBalance(p2)).toEqual(ton("1"));
}

get fun `test settle with wrong result hash is rejected (HTLC lock)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        0xDEAD,
        randomAddress("w"),
        ton("60"),
        ton("2"),
        participants,
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
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        requester.address,
        1,
        RESULT_HASH,
        randomAddress("w"),
        ton("60"),
        ton("2"),
        participants,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: requester.address, to: escrow.address, exitCode: ErrCode.NOT_ARBITER });
}

get fun `test payout exceeding escrow is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        randomAddress("w"),
        ton("150"),
        ton("0"),
        participants,
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
    val keeper = randomAddress("keeper");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

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
        randomAddress("w"),
        ton("10"),
        ton("0"),
        participants,
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
    val keeper = randomAddress("keeper");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    // Settle first.
    var participants: map<address, coins> = createEmptyMap();
    val settle = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, randomAddress("w"), ton("10"), ton("0"), participants, { value: ton("1") },
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
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    // winner 80 + fee 10 + participant 20 = 110 > 100. Each summand must count:
    // this pins both `+` operators in the total and the participant-summing loop
    // (dropping fee, participants, or the loop would let 110 slip under 100).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(randomAddress("p1"), ton("20"));
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, randomAddress("w"), ton("80"), ton("10"), participants, { value: ton("1") },
    );
    expect(res).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test payout total exactly equal to escrow is accepted (boundary)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    // winner 98 + fee 1 + participant 1 = 100 == escrow: must succeed (pins the
    // `<=` boundary in `total <= escrowAmount`; a strict `<` would reject this).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(randomAddress("p1"), ton("1"));
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, randomAddress("w"), ton("98"), ton("1"), participants, { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();
}

get fun `test settle refunds the unspent slack to the requester`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    // winner 60 + fee 2 + commissions 2 = 64; the remaining 36 must refund to the
    // requester (pins the `if (refund > 0)` refund branch — without it the
    // requester would receive nothing).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(randomAddress("p1"), ton("1"));
    participants.set(randomAddress("p2"), ton("1"));
    val before = testing.getAccountBalance(requester.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address, 1, RESULT_HASH, randomAddress("w"), ton("60"), ton("2"), participants, { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    val delta = testing.getAccountBalance(requester.address) - before;
    expect(delta).toBeGreaterOrEqual(ton("35.5"));
    expect(delta).toBeLessOrEqual(ton("36.5"));
}

get fun `test escrow rejects an unknown opcode (non-empty body) with INVALID_OP`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = escrow.sendAny(arbiter.address, body, { value: ton("1") });
    expect(res).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test escrow getters and top-up cover read paths`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val escrow = deployEscrow(requester.address, arbiter.address, randomAddress("t"), ton("100"));

    expect(escrow.getRequester()).toEqual(requester.address);
    expect(escrow.getExpectedHash()).toEqual(RESULT_HASH);

    // A plain top-up (additional funding / storage rent) is accepted.
    val top = escrow.sendEscrowTopUp(requester.address, 1, { value: ton("0.5") });
    expect(top).toHaveSuccessfulTx<EscrowTopUp>({ from: requester.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeFalse();
}
