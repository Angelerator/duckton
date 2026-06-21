// Fee-enforcement proof harness for the upgraded JobEscrow (φ=15% strict fee to
// the admin treasury, free-winner refunds base, coverage preflight). Opens one
// fresh escrow and settles it, fully parameterized so the same script drives:
//   * wallet-winner happy path  (WINNER_AMOUNT == BASE)
//   * free/walletless winner     (WINNER_AMOUNT == 0; base stays + refunds)
//   * FEE_MISMATCH negative      (PLATFORM_FEE != BASE*φ/BPS)
//   * under-funded negative      (ESCROW_AMOUNT < winner+fee+commissions)
//   * wrong-treasury negative    (TREASURY != GlobalParams.feeRecipient)
//
// On a negative the on-chain settle ABORTS (FEE_MISMATCH / PAYOUT_EXCEEDS_ESCROW);
// this script still completes and reports `settled=false` so the rejection is
// visible. Amounts are nanoton. Run:
//   TREASURY=<addr> WINNER=<addr> PARTICIPANT=<addr> BASE=.. WINNER_AMOUNT=.. \
//   COMMISSION=.. ESCROW_AMOUNT=.. acton script scripts/fee_proof_testnet.tolk --net testnet

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/escrow_types"
import "@wrappers/JobEscrow.gen"

fun main() {
    val deployer = scripts.wallet("deployer");
    val requester = deployer.address;
    val arbiter = deployer.address;

    val treasury = env<address>("TREASURY")!;       // bound fee recipient
    val winner = env<address>("WINNER")!;            // candidate winner
    val participant = env<address>("PARTICIPANT");   // optional wallet verifier

    val base = env<coins>("BASE")!;                  // quoted winning price (fee base)
    val winnerAmount = env<coins>("WINNER_AMOUNT") ?? base; // actual winner payout (0 = free)
    val platformFeeBps = (env<int>("PLATFORM_FEE_BPS") ?? 1500) as uint16;
    // Settle's platform fee. Default = the strictly-correct base*φ/BPS; override
    // with PLATFORM_FEE to force the FEE_MISMATCH negative.
    val defaultFee: coins = (base * platformFeeBps) / 10000;
    val platformFee: coins = env<coins>("PLATFORM_FEE") ?? defaultFee;
    // COMMISSION ENFORCEMENT (κ): the admin participation-commission rate (bps)
    // bound into the terms; when > 0 the settle enforces each participation leg ==
    // base*κ/BPS. Default κ = 5% (500 bps). Verifier commission defaults to exactly
    // base*κ/BPS so the default scenario passes the on-chain check; override
    // COMMISSION to force a COMMISSION_MISMATCH negative.
    val participationCommissionBps = (env<int>("PARTICIPATION_COMMISSION_BPS") ?? 500) as uint16;
    val defaultCommission: coins = (base * participationCommissionBps) / 10000;
    val commission: coins = env<coins>("COMMISSION") ?? defaultCommission;

    val escrowAmount = env<coins>("ESCROW_AMOUNT")!; // locked B (cap)
    val deployBuffer = env<coins>("DEPLOY_BUFFER") ?? ton("0.05");
    val settleValue = env<coins>("SETTLE_VALUE") ?? ton("0.03");
    val paramsVersion = (env<int>("ESCROW_PARAMS_VERSION") ?? 1) as uint32;
    val resultHash = env<int>("RESULT_HASH")
        ?? 0x5e2c7b9a0b1d4e3f6a8c0d2e4f6080a1c3e5074829fbad6c8e0f2143658799ab;
    val deadline = (env<int>("DEADLINE") ?? 0) as uint32;

    // Candidate set: winner always; participant iff it earns a commission. Built
    // IDENTICALLY for the terms commitment and the settle so the B1 check holds.
    var candidates: map<address, uint1> = createEmptyMap();
    candidates.set(winner, 1);
    if (participant != null && commission > 0) {
        candidates.set(participant!, 1);
    }
    val candidatesHash = candidatesCommitment(candidates);

    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: resultHash,
        candidatesHash: candidatesHash,
        paramsVersion: paramsVersion,
        platformFeeBps: platformFeeBps,
        participationCommissionBps: participationCommissionBps,
    };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: escrowAmount,
        deadline: deadline,
        settled: false,
        terms: terms.toCell(),
        pending: createEmptyMap(),
    };

    val escrow = JobEscrow.fromStorage(storage);
    escrow.deploy(deployer.address, { value: escrowAmount + deployBuffer }).waitForTrace();
    println("::ESCROW::address={}", escrow.address);
    println("::ESCROW::fee_recipient={}", escrow.getFeeRecipient());
    println("::ESCROW::platform_fee_bps={}", escrow.getPlatformFeeBps());
    println("::ESCROW::params_version={}", escrow.getParamsVersion());
    println("::ESCROW::B={}::base={}::winnerAmount={}::fee={}::commission={}",
        escrowAmount, base, winnerAmount, platformFee, commission);

    // Wrong-treasury negative: an honest coordinator binds `treasury` to the
    // authoritative on-chain GlobalParams.feeRecipient and REFUSES to fund/settle
    // an escrow whose get_fee_recipient differs (SettleError::TreasuryMismatch).
    // So we deploy it to read the divergent binding, but never settle it.
    if ((env<int>("SKIP_SETTLE") ?? 0) != 0) {
        println("::RESULT::settled=skipped_no_honest_settle");
        println("::CHECK::done");
        return;
    }

    var participants: map<address, coins> = createEmptyMap();
    if (participant != null && commission > 0) {
        participants.set(participant!, commission);
    }

    escrow.sendEscrowSettle(
        deployer.address,
        1,
        resultHash,
        winner,
        base,
        winnerAmount,
        platformFee,
        participants,
        candidates,
        { value: settleValue },
    ).waitForTrace();

    val es = escrow.getEscrowState();
    println("::RESULT::settled={}", es.settled);
    println("::CHECK::done");
}
