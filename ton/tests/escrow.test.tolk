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
    val terms: EscrowTerms = { treasury: treasury, expectedHash: RESULT_HASH };
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
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ERROR_HASH_MISMATCH });
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
    ).toHaveFailedTx({ from: requester.address, to: escrow.address, exitCode: ERROR_NOT_ARBITER });
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
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ERROR_PAYOUT_EXCEEDS_ESCROW });
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
    ).toHaveFailedTx({ from: keeper, to: escrow.address, exitCode: ERROR_TIMEOUT_NOT_REACHED });

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
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ERROR_ALREADY_SETTLED });
}
