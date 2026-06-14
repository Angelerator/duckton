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

    val terms: EscrowTerms = { treasury: treasury, expectedHash: expectedHash, paramsVersion: paramsVersion };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: escrowAmount,
        deadline: deadline,
        settled: false,
        terms: terms.toCell(),
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
