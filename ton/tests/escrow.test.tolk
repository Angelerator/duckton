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
// Canonical admin rates: platform fee φ = 15% (1500 bps), runner commission κ = 5%.
const FEE_BPS: int = 1500
const KAPPA_BPS: int = 500

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

// Deploy a per-job escrow binding the admin fee rate φ (feeBps) + fee recipient
// (treasury) into the terms, so settle enforces `platformFee == base*φ` (the
// QUOTED price `base`, decoupled from the winner payout) paid to `treasury`.
// Behaviour tests that never exercise the fee pass feeBps = 0 (then any fee == 0
// is valid and the fee check is inert).
fun deployEscrowC(
    requester: address,
    arbiter: address,
    treasury: address,
    amount: coins,
    candidates: map<address, uint1>,
    feeBps: int,
): JobEscrow {
    // κ = 0 ⇒ the on-chain commission check is inert (these tests set arbitrary
    // participant amounts). The κ-enforcement tests use `deployEscrowCK` below.
    return deployEscrowCK(
        requester,
        arbiter,
        treasury,
        amount,
        candidates,
        feeBps,
        0,
    );
}

// Like `deployEscrowC` but also binds the participation-commission rate κ, so
// settle enforces each participation leg == `base * κ / BPS_DENOM`.
fun deployEscrowCK(
    requester: address,
    arbiter: address,
    treasury: address,
    amount: coins,
    candidates: map<address, uint1>,
    feeBps: int,
    kappaBps: int,
): JobEscrow {
    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: RESULT_HASH,
        candidatesHash: candidatesCommitment(candidates),
        paramsVersion: 1,
        platformFeeBps: feeBps as uint16,
        participationCommissionBps: kappaBps as uint16,
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    participants.set(p2.address, ton("1"));

    val wBefore = testing.getAccountBalance(winner.address);
    val tBefore = testing.getAccountBalance(treasury.address);
    val p1Before = testing.getAccountBalance(p1.address);
    val p2Before = testing.getAccountBalance(p2.address);

    // base 60 (wallet winner paid the full base) + fee 9 (= 60*15%) + commissions 2
    // = 71; refund 29 to requester.
    val base = ton("60");
    val fee = (base * FEE_BPS) / 10000; // 9 TON
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base, // wallet winner: winnerAmount == base
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });

    expect(escrow.getEscrowState().settled).toBeTrue();
    val wd = testing.getAccountBalance(winner.address) - wBefore;
    val td = testing.getAccountBalance(treasury.address) - tBefore;
    val p1d = testing.getAccountBalance(p1.address) - p1Before;
    val p2d = testing.getAccountBalance(p2.address) - p2Before;
    expect(wd).toBeGreaterOrEqual(ton("59.99"));
    expect(wd).toBeLessOrEqual(ton("60"));
    // Platform fee lands at the BOUND treasury at EXACTLY 15% of the quoted base.
    expect(td).toBeGreaterOrEqual(ton("8.99"));
    expect(td).toBeLessOrEqual(ton("9"));
    expect(p1d).toBeGreaterOrEqual(ton("0.99"));
    expect(p1d).toBeLessOrEqual(ton("1"));
    expect(p2d).toBeGreaterOrEqual(ton("0.99"));
    expect(p2d).toBeLessOrEqual(ton("1"));
}

// COMMISSION ENFORCEMENT (κ): when the admin κ rate is bound, settle pays each
// agreeing verifier EXACTLY κ·base — and a settle that under/over-pays a
// participation leg is rejected on-chain (a compromised arbiter cannot shave the
// verifiers' commission into the requester's refund).
get fun `test settle enforces kappa commission: each verifier paid exactly base*kappa`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val p1 = testing.treasury("p1");
    val p2 = testing.treasury("p2");
    val cands = candidatesOf3(winner.address, p1.address, p2.address);
    // Bind both φ = 15% and κ = 5% into the terms.
    val escrow = deployEscrowCK(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
        KAPPA_BPS,
    );

    val base = ton("60");
    val fee = (base * FEE_BPS) / 10000;          // 9 TON
    val commission = (base * KAPPA_BPS) / 10000; // 3 TON each
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, commission);
    participants.set(p2.address, commission);

    val tBefore = testing.getAccountBalance(treasury.address);
    val p1Before = testing.getAccountBalance(p1.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();
    // Each verifier received exactly κ·base (= 3 TON), treasury got φ·base (= 9 TON).
    val p1d = testing.getAccountBalance(p1.address) - p1Before;
    expect(p1d).toBeGreaterOrEqual(ton("2.99"));
    expect(p1d).toBeLessOrEqual(ton("3"));
    expect(testing.getAccountBalance(treasury.address) - tBefore).toBeGreaterOrEqual(ton("8.99"));
}

get fun `test settle rejects a shaved kappa commission (COMMISSION_MISMATCH)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val p1 = testing.treasury("p1");
    val cands = candidatesOf2(winner.address, p1.address);
    val escrow = deployEscrowCK(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
        KAPPA_BPS,
    );

    val base = ton("60");
    val fee = (base * FEE_BPS) / 10000;
    // Arbiter tries to pay the verifier LESS than the bound κ·base (1 < 3).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.COMMISSION_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

// FREE-NODE POLICY [HIGH]: a FREE (walletless) node WINS a paid job. The winner is
// paid 0, the quoted `base` is REFUNDED to the requester, the platform STILL
// collects 15% of base to the treasury, and the wallet-holding verifier earns 5%.
get fun `test free node wins: base refunds to requester, platform still gets 15%`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    // A free winner: model "no payee" by paying it 0 (the contract is agnostic to
    // whether the address is a wallet — winnerAmount 0 means it receives nothing).
    val freeWinner = testing.treasury("free_winner");
    val verifier = testing.treasury("verifier"); // a wallet-holding agreeing runner
    val cands = candidatesOf2(freeWinner.address, verifier.address);
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    val base = ton("60");
    val fee = (base * FEE_BPS) / 10000;    // 9 TON to treasury
    val comm = (base * KAPPA_BPS) / 10000; // 3 TON to the wallet verifier
    var participants: map<address, coins> = createEmptyMap();
    participants.set(verifier.address, comm);

    val reqBefore = testing.getAccountBalance(requester.address);
    val tBefore = testing.getAccountBalance(treasury.address);
    val wBefore = testing.getAccountBalance(freeWinner.address);
    val vBefore = testing.getAccountBalance(verifier.address);

    // base 60 refunds (winnerAmount 0); spent = fee 9 + comm 3 = 12; refund ~88.
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        freeWinner.address,
        base,
        ton("0"), // FREE winner: paid nothing
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();

    // The free winner earns NOTHING.
    val wd = testing.getAccountBalance(freeWinner.address) - wBefore;
    expect(wd).toEqual(0);
    // The platform STILL collects exactly 15% of the quoted base.
    val td = testing.getAccountBalance(treasury.address) - tBefore;
    expect(td).toBeGreaterOrEqual(ton("8.99"));
    expect(td).toBeLessOrEqual(ton("9"));
    // The wallet verifier still earns its 5%.
    val vd = testing.getAccountBalance(verifier.address) - vBefore;
    expect(vd).toBeGreaterOrEqual(ton("2.99"));
    expect(vd).toBeLessOrEqual(ton("3"));
    // The requester is refunded the unpaid base (~60) plus the deploy slack, minus
    // only the 15% fee + 5% commission it was charged: delta well above 80 TON.
    val reqd = testing.getAccountBalance(requester.address) - reqBefore;
    expect(reqd).toBeGreaterOrEqual(ton("85"));
}

// STRICT fee enforcement: the platform fee MUST equal exactly base*φ at the admin's
// bound rate; a fee that is too low (the platform short-changed) is rejected with
// FEE_MISMATCH and the escrow stays unsettled.
get fun `test settle with wrong platform fee amount is rejected (FEE_MISMATCH)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    var participants: map<address, coins> = createEmptyMap();
    // Correct fee would be 9 (= 60*15%); the arbiter tries to shave it to 8.
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("60"),
        ton("8"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.FEE_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();

    // An OVER-payment of the fee is rejected too (must be EXACT, not just >=).
    val over = escrow.sendEscrowSettle(
        arbiter.address,
        2,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("60"),
        ton("10"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        over,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.FEE_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

// The platform fee can ONLY ever be paid to the admin-bound feeRecipient
// (terms.treasury) — the arbiter has no fee-recipient field at settle. We assert
// the fee lands at the bound recipient and that the bound recipient + rate are
// exposed via the getters for off-chain verification.
get fun `test platform fee is routed to the bound fee recipient, never the arbiter`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val feeRecipient = testing.treasury("admin_treasury");
    val winner = testing.treasury("winner");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        feeRecipient.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    expect(escrow.getFeeRecipient()).toEqual(feeRecipient.address);
    expect(escrow.getPlatformFeeBps()).toEqual(FEE_BPS);

    var participants: map<address, coins> = createEmptyMap();
    val base = ton("40");
    val fee = (base * FEE_BPS) / 10000; // 6 TON
    val frBefore = testing.getAccountBalance(feeRecipient.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    val frDelta = testing.getAccountBalance(feeRecipient.address) - frBefore;
    expect(frDelta).toBeGreaterOrEqual(ton("5.99"));
    expect(frDelta).toBeLessOrEqual(ton("6"));
}

// φ=15% / κ=5% across MULTIPLE agreeing runners: exact shares, all-or-nothing,
// total == B, accepted. base 80 + fee 12 (15%) + 2×κ·base (4 each) = 100 = B.
get fun `test multi-party settle at admin rates pays exact shares and is accepted`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val p1 = testing.treasury("p1");
    val p2 = testing.treasury("p2");
    val cands = candidatesOf3(winner.address, p1.address, p2.address);
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    val base = ton("80");
    val fee = (base * FEE_BPS) / 10000;    // 12 TON
    val comm = (base * KAPPA_BPS) / 10000; // 4 TON each
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, comm);
    participants.set(p2.address, comm);

    val tBefore = testing.getAccountBalance(treasury.address);
    val p1Before = testing.getAccountBalance(p1.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();
    val td = testing.getAccountBalance(treasury.address) - tBefore;
    val p1d = testing.getAccountBalance(p1.address) - p1Before;
    expect(td).toBeGreaterOrEqual(ton("11.99"));
    expect(td).toBeLessOrEqual(ton("12"));
    expect(p1d).toBeGreaterOrEqual(ton("3.99"));
    expect(p1d).toBeLessOrEqual(ton("4"));
}

// On-chain coverage: a settle whose ALL legs (winner + 15% fee + 5%×N commissions)
// exceed the locked B is rejected — the contract is all-or-nothing and never
// partially pays. base 90 + fee 13 + 2×4 = 111 > 100 = B.
get fun `test settle exceeding escrow across all legs is rejected on-chain`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val p1 = testing.treasury("p1");
    val p2 = testing.treasury("p2");
    val cands = candidatesOf3(winner.address, p1.address, p2.address);
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    val base = ton("90");
    val fee = (base * FEE_BPS) / 10000;    // 13.5 -> 13 TON (floor)
    val comm = (base * KAPPA_BPS) / 10000; // 4.5 -> 4 TON each
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, comm);
    participants.set(p2.address, comm);
    // winner 90 + fee 13 + 8 = 111 > 100.
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

// The winner can never be paid MORE than the quoted base price (defends the
// requester against an arbiter inflating the winner leg above the quote).
get fun `test winner paid above the quoted base is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val winner = testing.treasury("winner");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        cands,
        FEE_BPS,
    );

    var participants: map<address, coins> = createEmptyMap();
    val base = ton("40");
    val fee = (base * FEE_BPS) / 10000; // 6 TON
    // winnerAmount 50 > base 40 → rejected (PAYOUT_EXCEEDS_ESCROW guards it).
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        ton("50"),
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test settle with wrong result hash is rejected (HTLC lock)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        0xDEAD,
        winner.address,
        ton("60"),
        ton("60"),
        ton("0"),
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        requester.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("60"),
        ton("0"),
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );
    var participants: map<address, coins> = createEmptyMap();

    // (1) arbiter names ITSELF as the winner -> rejected (winner != arbiter).
    val toSelf = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        arbiter.address,
        ton("90"),
        ton("90"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        toSelf,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.NOT_OWNER_SIG });
    expect(escrow.getEscrowState().settled).toBeFalse();

    // (2) arbiter names an OUTSIDER not in the committed candidate set -> rejected.
    val toOutsider = escrow.sendEscrowSettle(
        arbiter.address,
        2,
        RESULT_HASH,
        outsider.address,
        ton("90"),
        ton("90"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        toOutsider,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();

    // (3) arbiter forges a DIFFERENT candidate set (one that includes itself) ->
    // the commitment check (which runs before the winner!=arbiter check) catches
    // it first: SETTLE_POLICY_MISMATCH. Either way the arbiter cannot self-pay.
    val forged = candidatesOf2(arbiter.address, winner.address);
    val toForged = escrow.sendEscrowSettle(
        arbiter.address,
        3,
        RESULT_HASH,
        arbiter.address,
        ton("90"),
        ton("90"),
        ton("0"),
        participants,
        forged,
        { value: ton("1") },
    );
    expect(
        toForged,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

// B1: a participant outside the committed set is rejected.
get fun `test settle with a non-candidate participant is rejected (B1)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    var participants: map<address, coins> = createEmptyMap();
    participants.set(randomAddress("intruder"), ton("1")); // not in cands
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("60"),
        ton("60"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.SETTLE_POLICY_MISMATCH });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test payout exceeding escrow is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val cands = candidatesOf2(winner.address, randomAddress("x"));
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("150"),
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    // Settle first.
    var participants: map<address, coins> = createEmptyMap();
    val settle = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("10"),
        ton("10"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(settle).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });

    // A refund after the deadline must now be rejected (the settled guard is
    // checked before the deadline gate), pinning EscrowRefund's `!settled`.
    testing.setNow(DEADLINE);
    val refund = escrow.sendEscrowRefund(keeper, 2, { value: ton("1") });
    expect(
        refund,
    ).toHaveFailedTx({ from: keeper, to: escrow.address, exitCode: ErrCode.ALREADY_SETTLED });
}

get fun `test payout total over escrow (winner+fee+participants) is rejected`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val p1 = testing.treasury("p1");
    val cands = candidatesOf2(winner.address, p1.address);
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    // winner 80 + fee 10 + participant 20 = 110 > 100. The total bound is checked
    // before the fee-equality, so this pins PAYOUT_EXCEEDS_ESCROW (each summand
    // counts: dropping fee, participants, or the loop would let 110 slip under 100).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("20"));
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        ton("80"),
        ton("80"),
        ton("10"),
        participants,
        cands,
        { value: ton("1") },
    );
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
    expect(escrow.getEscrowState().settled).toBeFalse();
}

get fun `test payout total exactly equal to escrow is accepted (boundary)`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val winner = testing.treasury("w");
    val p1 = testing.treasury("p1");
    val cands = candidatesOf2(winner.address, p1.address);
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        FEE_BPS,
    );

    // base 80 + fee 12 (15%) + participant 8 = 100 == escrow: must succeed (pins
    // the `<=` boundary in `total <= escrowAmount`; a strict `<` would reject this).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("8"));
    val base = ton("80");
    val fee = (base * FEE_BPS) / 10000; // 12 TON
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        FEE_BPS,
    );

    // winner 60 + fee 9 (15%) + commissions 2 = 71; the remaining ~29 must refund
    // to the requester (pins the carry-remaining refund — without it the requester
    // would receive nothing).
    var participants: map<address, coins> = createEmptyMap();
    participants.set(p1.address, ton("1"));
    participants.set(p2.address, ton("1"));
    val base = ton("60");
    val fee = (base * FEE_BPS) / 10000; // 9 TON
    val before = testing.getAccountBalance(requester.address);
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner.address,
        base,
        base,
        fee,
        participants,
        cands,
        { value: ton("1") },
    );
    expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
    // The requester recovers the TRUE remainder via carry-all: the 29 unspent
    // slack PLUS the ~2 TON over-funded deploy gas, minus the reserved buffer.
    val delta = testing.getAccountBalance(requester.address) - before;
    expect(delta).toBeGreaterOrEqual(ton("28.5"));
    expect(delta).toBeLessOrEqual(ton("32"));
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        cands,
        0,
    );

    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(
        arbiter.address,
        1,
        RESULT_HASH,
        winner,
        ton("60"),
        ton("60"),
        ton("0"),
        participants,
        cands,
        { value: ton("1") },
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
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        randomAddress("t"),
        ton("100"),
        candidatesOf2(randomAddress("w"), randomAddress("x")),
        0,
    );

    val body = beginCell().storeUint(0xDEADBEEF, 32).endCell();
    val res = escrow.sendAny(arbiter.address, body, { value: ton("1") });
    expect(
        res,
    ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.INVALID_OP });
}

get fun `test escrow getters and top-up cover read paths`() {
    testing.setNow(T0);
    val requester = testing.treasury("requester");
    val arbiter = testing.treasury("arbiter");
    val treasury = testing.treasury("treasury");
    val escrow = deployEscrowC(
        requester.address,
        arbiter.address,
        treasury.address,
        ton("100"),
        candidatesOf2(randomAddress("w"), randomAddress("x")),
        FEE_BPS,
    );

    expect(escrow.getRequester()).toEqual(requester.address);
    expect(escrow.getExpectedHash()).toEqual(RESULT_HASH);
    expect(escrow.getPending(randomAddress("nobody"))).toEqual(0);
    // Fee-enforcement bindings are exposed for off-chain verification.
    expect(escrow.getPlatformFeeBps()).toEqual(FEE_BPS);
    expect(escrow.getFeeRecipient()).toEqual(treasury.address);

    // A plain top-up (additional funding / storage rent) is accepted.
    val top = escrow.sendEscrowTopUp(requester.address, 1, { value: ton("0.5") });
    expect(top).toHaveSuccessfulTx<EscrowTopUp>({ from: requester.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeFalse();
}
