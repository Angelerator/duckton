// Deploy a per-job JobEscrow (the requester locks their max bid `B` here up
// front; settlement is HTLC-gated on the agreed quorum result hash and bounded
// by `B`, with any remainder refunded to the requester). BLOCKCHAIN_ECONOMICS
// §6.2, §13.
//
// This fills the deploy-script gap for the third contract (StakeVault and
// RecordAnchor already had deploy scripts). Network-agnostic — run locally or
// against testnet/mainnet exactly like the others:
//     acton script scripts/deploy_escrow.tolk
//     acton script scripts/deploy_escrow.tolk --net testnet
//
// All parameters come from the environment with sensible defaults. The escrow
// is "opened" by deploying it WITH the locked funds attached, so the deploy
// value must cover `ESCROW_AMOUNT` plus a gas/storage buffer. Example:
//     ESCROW_AMOUNT=300000000 ESCROW_TREASURY=<addr> \
//     ESCROW_EXPECTED_HASH=0x<64hex> ESCROW_DEADLINE=<unix-secs> \
//     acton script scripts/deploy_escrow.tolk --net testnet
//
// NOTE: amounts are nanoton (1 TON = 1e9). `ESCROW_DEADLINE` is an ABSOLUTE
// unix timestamp (compute it in the harness, e.g. `date +%s` + window) — feeding
// a fresh deadline each run yields a fresh escrow address (the address is a pure
// function of storage), which keeps the harness re-runnable.

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/escrow_types"
import "@wrappers/JobEscrow.gen"

fun main() {
    val deployer = scripts.wallet("deployer");

    // The requester funds B and receives refunds; the arbiter (coordinator /
    // quorum oracle) is the only party allowed to settle. For a single-wallet
    // turnkey demo both default to the deployer.
    val requester = env<address>("ESCROW_REQUESTER") ?? deployer.address;
    val arbiter = env<address>("ESCROW_ARBITER") ?? deployer.address;
    val treasury = env<address>("ESCROW_TREASURY") ?? deployer.address;

    val escrowAmount = env<coins>("ESCROW_AMOUNT") ?? ton("0.3");
    val deadline = (env<int>("ESCROW_DEADLINE") ?? 0) as uint32;
    // HTLC lock: settle must later present exactly this hash (the agreed quorum
    // result hash — e.g. the hash of the DuckDB query result the harness runs).
    val expectedHash = env<int>("ESCROW_EXPECTED_HASH") ?? 0;
    // GlobalParams version in force when this job was opened (pins which
    // ecosystem params govern settlement / disputes). `0` = unbound.
    val paramsVersion = (env<int>("ESCROW_PARAMS_VERSION") ?? 0) as uint32;
    // FEE ENFORCEMENT (φ): the admin's authoritative platform-fee rate (bps) bound
    // into the escrow at open. At settle the contract enforces
    // `platformFee == winnerAmount * platformFeeBps / BPS_DENOM` to `treasury`.
    // Bind it to `GlobalParams.get_platform_fee_bps` for `paramsVersion`. Default
    // = the canonical φ = 15% (1500 bps). `0` = no fee enforced (legacy/free).
    val platformFeeBps = (env<int>("ESCROW_PLATFORM_FEE_BPS") ?? 1500) as uint16;
    // COMMISSION ENFORCEMENT (κ): the admin's participation-commission rate (bps)
    // bound at open. When > 0, settle enforces each participation leg ==
    // `base * κ / BPS_DENOM`. Bind it to `GlobalParams.get_participation_commission_bps`.
    // Default `0` keeps this manual/demo deploy flexible (the paired settle script
    // sets an arbitrary ESCROW_PARTICIPANT_AMOUNT); set it to the live κ (e.g. 500)
    // to exercise on-chain commission enforcement.
    val participationCommissionBps = (env<int>("ESCROW_PARTICIPATION_COMMISSION_BPS") ?? 0) as uint16;

    // B1: the requester pre-commits to the payout-eligible candidate set; settle
    // can only pay members of it. Two ways to bind it:
    //   * ESCROW_CANDIDATES_HASH — a precomputed commitment, used verbatim; or
    //   * ESCROW_WINNER (+ optional ESCROW_PARTICIPANT, included iff
    //     ESCROW_PARTICIPANT_AMOUNT > 0) — this script then computes
    //     candidatesCommitment over the SAME set the settle will present, so the
    //     on-chain B1 check (candidatesCommitment(settle.candidates) ==
    //     terms.candidatesHash) holds. The set is built IDENTICALLY to
    //     scripts/e2e_testnet.tolk (winner always; participant iff its amount > 0),
    //     so a deploy + settle driven from the same env vars round-trip cleanly.
    // `0` = unbound (no candidate restriction set yet — the legacy default).
    var candidatesHash = env<int>("ESCROW_CANDIDATES_HASH") ?? 0;
    val candidateWinner = env<address>("ESCROW_WINNER");
    if (candidatesHash == 0 && candidateWinner != null) {
        val candidateParticipant = env<address>("ESCROW_PARTICIPANT");
        val candidateParticipantAmount: coins = env<coins>("ESCROW_PARTICIPANT_AMOUNT") ?? ton("0");
        var candidates: map<address, uint1> = createEmptyMap();
        candidates.set(candidateWinner!, 1);
        if (candidateParticipant != null && candidateParticipantAmount > 0) {
            candidates.set(candidateParticipant!, 1);
        }
        candidatesHash = candidatesCommitment(candidates);
    }

    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: expectedHash,
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
    // Attach B + a buffer so the contract actually holds the locked escrow and
    // can pay storage rent / gas for the eventual settle.
    val res = escrow.deploy(deployer.address, { value: escrowAmount + ton("0.15") });
    res.waitForTrace();

    // Stable, machine-readable line the harness greps for ("Deployed <C> to <addr>").
    println("Deployed JobEscrow to {}", escrow.address);
    println("  requester     = {}", requester);
    println("  arbiter       = {}", arbiter);
    println("  treasury      = {}", treasury);
    println("  escrow amount = {}", escrowAmount);
    println("  deadline      = {}", deadline);
}
