// Per-node payout proof (NO staking) — BLOCKCHAIN_ECONOMICS §6.2.
//
// Proves that a node's bound payout wallet RECEIVES a real on-chain TON transfer
// without the node having to be funded or staked: only the requester/arbiter
// (the funded `deployer`) needs gas. In ONE script we:
//
//   1. open a fresh JobEscrow funded by the deployer, with its candidate set
//      pre-committed to a couple of the per-node payout wallets (winner +
//      participant), HTLC-locked on a result hash; then
//   2. settle it — paying the WINNER node wallet (base+bonus), a participation
//      commission to the PARTICIPANT node wallet, the platform fee to the
//      treasury, and refunding the remainder to the requester.
//
// The winner/participant are bound payout wallets from ton/deployments/bindings.toml.
// No StakeVault is deployed or bonded (this is explicitly the without-staking path).
//
//   WINNER=<addr> PARTICIPANT=<addr> acton script scripts/payout_nodes_testnet.tolk --net testnet
//
// Amounts are nanoton (1 TON = 1e9). Deploy + settle run in the same session, so
// the escrow's expectedHash == the settle result hash by construction.

import "@acton/env"
import "@acton/io"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/escrow_types"
import "@wrappers/JobEscrow.gen"

fun main() {
    val deployer = scripts.wallet("deployer");

    // The two per-node payout wallets under test. WINNER must differ from the
    // arbiter (the deployer) per the escrow's B1 anti-self-payout rule.
    val winner = env<address>("WINNER")!;
    val participant = env<address>("PARTICIPANT");

    // requester funds B and gets the refund; arbiter is the only settler;
    // treasury collects the platform fee. All three are the funded deployer here.
    val requester = deployer.address;
    val arbiter = deployer.address;
    // Platform-fee recipient. A DEDICATED active wallet (distinct from the
    // requester/arbiter/deployer) makes the fee-landing delta cleanly isolated:
    // its balance moves by EXACTLY the platform fee and nothing else.
    val treasury = env<address>("TREASURY") ?? deployer.address;

    val escrowAmount = env<coins>("ESCROW_AMOUNT") ?? ton("0.3");
    val winnerAmount = env<coins>("WINNER_AMOUNT") ?? ton("0.05");
    // FEE ENFORCEMENT (φ): the admin φ-rate bound into the escrow; the settle's
    // platformFee is DERIVED as exactly winnerAmount*φ so the on-chain strict
    // fee-equality check passes. Default φ = 15% (1500 bps).
    val platformFeeBps = (env<int>("PLATFORM_FEE_BPS") ?? 1500) as uint16;
    val platformFee = (winnerAmount * platformFeeBps) / 10000;
    val participantAmount: coins = env<coins>("PARTICIPANT_AMOUNT") ?? ton("0.02");
    // Operational gas/storage buffer attached on top of B at deploy, and the value
    // attached to the settle message. The contract keeps MIN_TONS_FOR_STORAGE
    // (0.05 TON) as a permanent reserve and refunds the rest to the requester;
    // shrink these to keep a cost-focused demo's refund tight.
    val deployBuffer = env<coins>("DEPLOY_BUFFER") ?? ton("0.15");
    val settleValue = env<coins>("SETTLE_VALUE") ?? ton("0.1");
    // HTLC lock — settle must present exactly this hash (it does, below).
    val resultHash = env<int>("RESULT_HASH")
        ?? 0x790adc4ba2024c23468b925c9725856aac0164822cf7c06b961b8384adae62d1;
    val deadline = (env<int>("DEADLINE") ?? 0) as uint32;

    // B1: the requester-committed payout-eligible set (winner + participant iff it
    // earns a commission). Built IDENTICALLY for the terms commitment (here) and
    // the settle message (below) so the on-chain membership/commitment check holds.
    var candidates: map<address, uint1> = createEmptyMap();
    candidates.set(winner, 1);
    if (participant != null && participantAmount > 0) {
        candidates.set(participant!, 1);
    }
    val candidatesHash = candidatesCommitment(candidates);

    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: resultHash,
        candidatesHash: candidatesHash,
        paramsVersion: 0,
        platformFeeBps: platformFeeBps,
        // κ check inert: this cost demo uses an arbitrary PARTICIPANT_AMOUNT, not a
        // κ-derived commission. Production binds the live κ (see fee_proof_testnet).
        participationCommissionBps: 0,
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

    // -- 1. Open the escrow (deploy WITH the locked B + a gas/storage buffer) --
    val escrow = JobEscrow.fromStorage(storage);
    val deployRes = escrow.deploy(deployer.address, { value: escrowAmount + deployBuffer });
    deployRes.waitForTrace();
    println("Deployed JobEscrow to {}", escrow.address);
    println("  escrow amount = {}", escrowAmount);
    println("  winner        = {}", winner);
    if (participant != null) {
        println("  participant   = {}", participant!);
    }

    // -- 2. Settle: HTLC release to the per-node payout wallets ----------------
    var participants: map<address, coins> = createEmptyMap();
    if (participant != null && participantAmount > 0) {
        participants.set(participant!, participantAmount);
    }
    val settleRes = escrow.sendEscrowSettle(
        deployer.address,
        1,
        resultHash,
        winner,
        winnerAmount, // quoted base (wallet winner: == winnerAmount)
        winnerAmount,
        platformFee,
        participants,
        candidates,
        { value: settleValue },
    );
    settleRes.waitForTrace();

    val es = escrow.getEscrowState();
    println("::CHECK::escrow_settle::settled={}", es.settled);
    println("::CHECK::winner_paid::{}::amount={}", winner, winnerAmount);
    if (participant != null && participantAmount > 0) {
        println("::CHECK::participant_paid::{}::amount={}", participant!, participantAmount);
    }
    println("::CHECK::done");
}
