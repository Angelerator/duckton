// Comprehensive on-chain scenario runner (TON testnet) for the duckdb-p2p
// settlement contracts. This is the LIVE counterpart of ton/SCENARIOS.md: it
// drives every TESTNET-BROADCASTABLE *positive* scenario from the catalog as a
// real broadcast, reads the contract getters back, and asserts the state change,
// emitting one `::CHECK::<name>::PASS|FAIL` marker per scenario (parsed by
// scripts/scenarios_testnet.sh into a PASS/FAIL summary with tonviewer links and
// `acton rpc trace` of representative tx hashes).
//
// Negative (expected-to-fail) and time-gated (cooldown/timelock) scenarios are
// intentionally NOT broadcast here — they would leave scary aborted txs and burn
// gas for no extra signal — they are proven in the Acton emulator suite
// (`acton test`), which scripts/scenarios_testnet.sh also runs. See
// ton/SCENARIOS.md for the full classification.
//
//   acton script scripts/scenarios_testnet.tolk                # emulator
//   acton script scripts/scenarios_testnet.tolk --net testnet  # live testnet
//
// Gas: deploys fresh, frugal instances by default (tiny nanoton amounts). To run
// gas-light against ALREADY-DEPLOYED contracts (the testnet faucet allows only
// 2 airdrops / 24h), pin them and/or gate sections:
//   VAULT_ADDR=kQ.. ANCHOR_ADDR=kQ.. GLOBAL_PARAMS_ADDR=kQ..  (reuse instances)
//   RUN_VAULT / RUN_ANCHOR / RUN_GP / RUN_ESCROW = 0|1         (section gates)
//   WINNER_ADDR=kQ..                                           (escrow winner != arbiter)
//
// Testnet only. No mainnet. No commits.

import "@acton/env"
import "@acton/io"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/anchor_types"
import "@contracts/escrow_types"
import "@contracts/global_params_types"
import "@contracts/stake_types"
import "@wrappers/GlobalParams.gen"
import "@wrappers/JobEscrow.gen"
import "@wrappers/RecordAnchor.gen"
import "@wrappers/StakeReceiptWallet.gen"
import "@wrappers/StakeVault.gen"

// ---------------------------------------------------------------------------
// 8-leaf (3-level) Merkle tree — identical values to tests/anchor.test.tolk and
// crates/settlement/src/merkle.rs (MERKLE v2: leaves domain-separated).
// ---------------------------------------------------------------------------
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

fun tree8Left(): int { return hashPair(hashPair(hashLeaf(M0), hashLeaf(M1)), hashPair(hashLeaf(M2), hashLeaf(M3))); }
fun tree8Right(): int { return hashPair(hashPair(hashLeaf(M4), hashLeaf(M5)), hashPair(hashLeaf(M6), hashLeaf(M7))); }
fun tree8Root(): int { return hashPair(tree8Left(), tree8Right()); }

fun proofForM5(): Cell<MerkleStep>? {
    val top: MerkleStep = { dir: 1, sibling: tree8Left(), next: null };
    val mid: MerkleStep = { dir: 0, sibling: hashPair(hashLeaf(M6), hashLeaf(M7)), next: top.toCell() };
    val bottom: MerkleStep = { dir: 1, sibling: hashLeaf(M4), next: mid.toCell() };
    return bottom.toCell();
}

// §12 default economic params (surchargeBps = 500 so GlobalParamsV2's new
// get_surcharge_bps has a known value to read back after the in-place upgrade).
fun defaultParams(): EcoParams {
    return {
        platformFeeBps: 200,
        surchargeBps: 500,
        participationCommissionBps: 200,
        slashWrongBps: 1500,
        slashCheatBps: 10000,
        slashDowntimeBps: 200,
        slashEquivocationBps: 5000,
        splitChallengerBps: 4000,
        splitRedundancyBps: 3000,
        splitBurnBps: 2000,
        splitTreasuryBps: 1000,
        minStake: ton("0"),
        minStakeInternal: ton("100"),
        minStakeSensitive: ton("1000"),
        stakeCap: ton("100000"),
        unbondingSecs: 604800,
        challengeWindowSecs: 86400,
        nPublic: 3,
        nDefault: 5,
        nMax: 10,
        quorum: 3,
        checksumMin: 3,
        wQualityBps: 6000,
        wStakeBps: 1500,
        wPriceBps: 2500,
        slashFailedCommitmentBps: 1000,
        attemptDeadlineMs: 60000,
        progressIntervalMs: 2000,
        progressStallMult: 5,
    };
}

fun pass(name: string, ok: bool) {
    println("::CHECK::{}::{}", name, ok ? "PASS" : "FAIL");
}

fun envFlag(name: string, dflt: bool): bool {
    return env<bool>(name) ?? dflt;
}

// ===========================================================================
// StakeVault (+ StakeReceiptWallet): deposit/mint/metadata/TEP-74/TEP-89,
// request_unbond, top-up. Reuses VAULT_ADDR if pinned (gas-light), else deploys
// a fresh free-tier (min_stake 0) vault.
// ===========================================================================
fun runVault(deployer: ActonWallet) {
    println("::SECTION::vault");
    val pinned = env<address>("VAULT_ADDR");
    var vault: StakeVault;
    if (pinned != null) {
        vault = StakeVault.fromAddress(pinned!);
        println("::ADDR::vault={}", vault.address);
    } else {
        val slash: SlashConfig = {
            treasury: deployer.address,
            redundancyPool: deployer.address,
            splitChallengerBps: 4000,
            splitRedundancyBps: 3000,
            splitBurnBps: 2000,
            splitTreasuryBps: 1000,
        };
        val cfg: VaultConfig = {
            minStake: ton("0"),
            unbondingPeriod: 604800,
            challengeWindow: 86400,
            keeperGrace: 86400,
            keeperBountyBps: 200,
            slasher: deployer.address,
            upgradeAuthority: deployer.address,
            receiptWalletCode: build("StakeReceiptWallet"),
            slash: slash.toCell(),
        };
        val storage: VaultStorage = {
            owner: deployer.address,
            staked: 0,
            unbondingAmount: 0,
            unbondingAt: 0,
            totalSupply: 0,
            bindingHash: 0xDEADBEEF,
            config: cfg.toCell(),
            upgrade: freshVaultUpgrade(),
            pending: createEmptyMap(),
        };
        vault = StakeVault.fromStorage(storage);
        vault.deploy(deployer.address, { value: ton("0.1") }).waitForTrace();
        println("::ADDR::vault={}", vault.address);
    }

    // SV-1: deposit bonds TON and mints the 1:1 transfer-locked receipt.
    val depAmt = env<coins>("SCN_DEPOSIT") ?? ton("0.02");
    val vsBefore = vault.getVaultState();
    vault.sendStakeDeposit(deployer.address, 101, depAmt, { value: depAmt + ton("0.2") }).waitForTrace();
    val vsAfter = vault.getVaultState();
    val staked = vsAfter.staked == vsBefore.staked + depAmt;
    val supply = vsAfter.totalSupply == vsBefore.totalSupply + depAmt;
    pass("sv_deposit", staked && supply && vsAfter.eligible);

    // SV-2/SV-4: the receipt wallet was minted 1:1 and is transfer-locked.
    val receiptAddr = vault.getReceiptWalletAddress(deployer.address);
    val receipt = StakeReceiptWallet.fromAddress(receiptAddr);
    val wd = receipt.getWalletData();
    pass("sv_receipt_mint_1to1_locked", wd.balance == vsAfter.staked && receipt.isTransferLocked() && wd.minter == vault.address);

    // SV-3: Duckton TEP-64 on-chain metadata (content-cell hash + mintable + admin + wallet code).
    val jd = vault.getJettonData();
    val metaOk = jd.jettonContent.hash() == buildReceiptJettonContent().hash()
        && jd.mintable
        && jd.adminAddress == vault.address
        && jd.jettonWalletCode.hash() == build("StakeReceiptWallet").hash();
    pass("sv_duckton_tep64_metadata", metaOk);

    // SV-5: TEP-89 wallet discovery — get_wallet_address matches the deterministic
    // receipt wallet, and a ProvideWalletAddress is answered (TakeWalletAddress).
    vault.sendProvideWalletAddress(deployer.address, 102, deployer.address, true, { value: ton("0.05") }).waitForTrace();
    pass("sv_tep89_wallet_discovery", vault.getWalletAddress(deployer.address) == receiptAddr);

    // SV-10: ReceiptTopUp accepted (storage rent) — no state field change, no abort.
    vault.sendReceiptTopUp(deployer.address, 103, { value: ton("0.03") }).waitForTrace();
    pass("sv_topup_accepted", vault.getVaultState().staked == vsAfter.staked);

    // SV-8: request_unbond moves the bonded stake into the slashable cooldown bucket.
    // (Skipped if pinning a vault that already has an unbond in progress.)
    val vsPre = vault.getVaultState();
    if (vsPre.unbondingAt == 0 && vsPre.staked > 0) {
        val unbondAmt = vsPre.staked;
        vault.sendStakeRequestUnbond(deployer.address, 104, unbondAmt).waitForTrace();
        val vsUnb = vault.getVaultState();
        pass("sv_request_unbond", vsUnb.unbondingAmount == unbondAmt && vsUnb.unbondingAt != 0 && vsUnb.staked == vsPre.staked - unbondAmt);
    } else {
        println("::CHECK::sv_request_unbond::SKIP (unbond already in progress on pinned vault)");
    }
}

// ===========================================================================
// RecordAnchor (+ RecordAnchorV2): submit single + multi-leaf roots, inclusion +
// tamper, dispute open/resolve (uphold AND reject), and the authority-gated
// in-place SETCODE upgrade -> V2 get_verdict_authority live (immediate, no
// timelock). Reuses ANCHOR_ADDR if pinned, else deploys a fresh anchor.
// ===========================================================================
fun runAnchor(deployer: ActonWallet) {
    println("::SECTION::anchor");
    val pinned = env<address>("ANCHOR_ADDR");
    var anchor: RecordAnchor;
    if (pinned != null) {
        anchor = RecordAnchor.fromAddress(pinned!);
        println("::ADDR::anchor={}", anchor.address);
    } else {
        val auth: AnchorAuthorities = {
            treasury: deployer.address,
            keeper: deployer.address,
            upgradeAuthority: deployer.address,
        };
        val cfg: AnchorConfig = {
            minStakeWeight: ton("0"),
            disputeBondMin: ton("0.05"),
            verdictAuthority: deployer.address,
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
        anchor = RecordAnchor.fromStorage(storage);
        anchor.deploy(deployer.address, { value: ton("0.1") }).waitForTrace();
        println("::ADDR::anchor={}", anchor.address);
    }

    val stakeWeight = env<coins>("SCN_ANCHOR_WEIGHT") ?? ton("100");
    val bond = env<coins>("SCN_DISPUTE_BOND") ?? ton("0.1");

    // RA-1/RA-2: submit a single-leaf epoch root (== hashLeaf(leaf)); inclusion verifies with a null proof.
    val s0 = anchor.getAnchorState();
    val leaf = 0xABCDEF;
    val root = hashLeaf(leaf);
    val e1 = (s0.currentEpoch + 1) as uint32;
    anchor.sendAnchorSubmitRoot(deployer.address, 201, e1, root, s0.lastRoot, stakeWeight, { value: ton("0.08") }).waitForTrace();
    val s1 = anchor.getAnchorState();
    pass("ra_submit_single", s1.currentEpoch == e1 && s1.lastRoot == root);
    pass("ra_inclusion_single", anchor.verifyInclusion(leaf, root, null));

    // RA-3: submit a realistic 8-leaf root; an interior-leaf inclusion proof folds
    // to it on-chain, and a tampered leaf does NOT.
    val mlRoot = tree8Root();
    val e2 = (s1.currentEpoch + 1) as uint32;
    anchor.sendAnchorSubmitRoot(deployer.address, 202, e2, mlRoot, s1.lastRoot, stakeWeight, { value: ton("0.08") }).waitForTrace();
    val s2 = anchor.getAnchorState();
    val incl = anchor.verifyInclusion(M5, mlRoot, proofForM5());
    val tamper = anchor.verifyInclusion(M5 + 1, mlRoot, proofForM5());
    pass("ra_multileaf_inclusion", s2.currentEpoch == e2 && s2.lastRoot == mlRoot && incl && !tamper);

    // RA-4: dispute the single-leaf epoch (proof folds to its stored root); resolve UPHELD -> status 1.
    anchor.sendAnchorOpenDispute(deployer.address, 203, e1, leaf, null, { value: bond + ton("0.1") }).waitForTrace();
    val mid1 = anchor.getAnchorState();
    val did1 = (mid1.nextDisputeId - 1) as uint32;
    anchor.sendAnchorResolveDispute(deployer.address, 204, did1, true, { value: ton("0.08") }).waitForTrace();
    val d1 = anchor.getDispute(did1);
    pass("ra_dispute_upheld", d1 != null && d1!.status == 1);

    // RA-5: a second dispute on the same epoch; resolve REJECTED (frivolous) -> status 2 (bond forfeited to treasury).
    anchor.sendAnchorOpenDispute(deployer.address, 205, e1, leaf, null, { value: bond + ton("0.1") }).waitForTrace();
    val mid2 = anchor.getAnchorState();
    val did2 = (mid2.nextDisputeId - 1) as uint32;
    anchor.sendAnchorResolveDispute(deployer.address, 206, did2, false, { value: ton("0.08") }).waitForTrace();
    val d2 = anchor.getDispute(did2);
    pass("ra_dispute_rejected", d2 != null && d2!.status == 2);

    // RA-6: authority-gated in-place SETCODE upgrade -> RecordAnchorV2 (immediate,
    // no timelock). Same address, epoch chain preserved, codeVersion bumped, and
    // the v2-only get_verdict_authority getter resolves live.
    if (envFlag("SCN_ANCHOR_UPGRADE", true)) {
        val cvBefore = anchor.getCodeVersion();
        val stBefore = anchor.getAnchorState();
        anchor.sendAnchorUpgradeCode(deployer.address, 207, build("RecordAnchorV2"), { value: ton("0.1") }).waitForTrace();
        val cvAfter = anchor.getCodeVersion();
        val stAfter = anchor.getAnchorState();
        val vAuth = net.runGetMethod<address>(anchor.address, "get_verdict_authority");
        val ok = cvAfter == cvBefore + 1
            && stAfter.currentEpoch == stBefore.currentEpoch
            && stAfter.lastRoot == stBefore.lastRoot
            && vAuth == deployer.address;
        pass("ra_upgrade_v2_get_verdict_authority", ok);
    } else {
        println("::CHECK::ra_upgrade_v2_get_verdict_authority::SKIP");
    }
}

// ===========================================================================
// GlobalParams (+ GlobalParamsV2): update_params (version monotonic, address
// stable), boundary accept, blocklist round-trip, scalar getters, announce+
// cancel, the timelocked SETCODE upgrade -> V2 get_surcharge_bps live (proven on
// a DEDICATED instance deployed with upgradeDelay=0 so announce+apply run in one
// session without weakening the production singleton's timelock), and finally
// admin rotation. Always a FRESH instance (the upgrade needs delay 0).
// ===========================================================================
fun runGlobalParams(deployer: ActonWallet) {
    println("::SECTION::globalparams");
    val storage: GlobalParamsStorage = {
        admin: deployer.address,
        feeRecipient: deployer.address,
        params: defaultParams().toCell(),
        blocklist: createEmptyMap(),
        paramsVersion: 1,
        codeVersion: 0,
        upgradeDelay: 0, // dedicated test instance: announce+apply in one session
        pendingCodeHash: 0,
        pendingCodeAt: 0,
    };
    val gp = GlobalParams.fromStorage(storage);
    gp.deploy(deployer.address, { value: ton("0.1") }).waitForTrace();
    val addr0 = gp.address;
    println("::ADDR::globalparams={}", gp.address);

    // GP-1: admin update_params mutates in place; paramsVersion bumps; address stable.
    val pvBefore = gp.getParamsVersion();
    var p = defaultParams();
    p.platformFeeBps = 333;
    gp.sendUpdateParams(deployer.address, 301, deployer.address, p, { value: ton("0.08") }).waitForTrace();
    val after = GlobalParams.fromAddress(addr0);
    val pv1 = after.getParamsVersion();
    pass(
        "gp_update_params_monotonic_stable",
        pv1 == pvBefore + 1
            && after.getParams().params.load().platformFeeBps == 333
            && after.address == addr0
            && after.getAdmin() == deployer.address,
    );

    // GP-5: scalar policy getters read back the stored params.
    pass("gp_scalar_getters", gp.getPlatformFeeBps() == 333 && gp.getParticipationCommissionBps() == 200);

    // GP-3: governance blocklist set/clear round-trip.
    val subject = 0x1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0;
    gp.sendSetBlocklisted(deployer.address, 302, subject, true, { value: ton("0.05") }).waitForTrace();
    val blocked = gp.getBlocklisted(subject);
    gp.sendSetBlocklisted(deployer.address, 303, subject, false, { value: ton("0.05") }).waitForTrace();
    val cleared = gp.getBlocklisted(subject);
    pass("gp_blocklist_roundtrip", blocked && !cleared);

    // GP-6: announce a code upgrade, then CANCEL it (pending set -> cleared).
    gp.sendAnnounceCode(deployer.address, 304, build("GlobalParamsV2").hash(), { value: ton("0.05") }).waitForTrace();
    val pendSet = gp.getPendingUpgrade().pendingCodeHash != 0;
    gp.sendCancelCode(deployer.address, 305, { value: ton("0.05") }).waitForTrace();
    val pendCleared = gp.getPendingUpgrade().pendingCodeHash == 0;
    pass("gp_announce_cancel", pendSet && pendCleared);

    // GP-7: timelocked SETCODE upgrade -> GlobalParamsV2. With upgradeDelay=0,
    // announce + apply run in one session; codeVersion bumps 0->1; address +
    // params/admin preserved; the v2-only get_surcharge_bps resolves (== 500).
    if (envFlag("SCN_GP_UPGRADE", true)) {
        val cvBefore = gp.getCodeVersion();
        val pvBeforeUp = gp.getParamsVersion();
        val newCode = build("GlobalParamsV2");
        gp.sendAnnounceCode(deployer.address, 306, newCode.hash(), { value: ton("0.05") }).waitForTrace();
        gp.sendUpgradeCode(deployer.address, 307, newCode, { value: ton("0.12") }).waitForTrace();
        val cvAfter = gp.getCodeVersion();
        val surcharge = net.runGetMethod<int>(addr0, "get_surcharge_bps");
        val ok = cvAfter == cvBefore + 1
            && gp.address == addr0
            && gp.getParamsVersion() == pvBeforeUp
            && gp.getParams().params.load().platformFeeBps == 333
            && surcharge == 500;
        pass("gp_upgrade_v2_get_surcharge_bps", ok);
    } else {
        println("::CHECK::gp_upgrade_v2_get_surcharge_bps::SKIP");
    }

    // GP-4: admin rotation (multisig handoff) — done LAST (rotates control away).
    // Rotate to WINNER_ADDR if provided, else to the deployer (idempotent handoff).
    val newAdmin = env<address>("WINNER_ADDR") ?? deployer.address;
    gp.sendUpdateAdmin(deployer.address, 308, newAdmin, { value: ton("0.05") }).waitForTrace();
    pass("gp_admin_rotation", gp.getAdmin() == newAdmin);
}

// ===========================================================================
// JobEscrow: HTLC settle (winner != arbiter, requester-committed candidate set)
// releases the escrow, then a bounced winner payout is recovered via the
// pull-ledger + EscrowClaim. Deploys a fresh, frugal escrow bound to
// {winner, deployer}; the winner must differ from the arbiter (= deployer).
// ===========================================================================
fun runEscrow(deployer: ActonWallet) {
    println("::SECTION::escrow");
    val winner = env<address>("WINNER_ADDR");
    if (winner == null) {
        println("::CHECK::escrow_settle::SKIP (set WINNER_ADDR != arbiter to run)");
        return;
    }

    val B = env<coins>("SCN_ESCROW_B") ?? ton("0.05");
    val winnerAmount = env<coins>("SCN_WINNER_AMT") ?? ton("0.01");
    val platformFee = env<coins>("SCN_FEE") ?? ton("0.002");
    val participantAmount = env<coins>("SCN_PARTICIPANT_AMT") ?? ton("0.002");
    val resultHash = env<int>("SCN_RESULT_HASH") ?? 0xC0FFEE;
    val deadline = ((env<int>("SCN_DEADLINE") ?? 0) as uint32);

    // Build the requester-committed candidate set = {winner, deployer(participant)}
    // and bind its commitment into the escrow terms at open (B1).
    var candidates: map<address, uint1> = createEmptyMap();
    candidates.set(winner!, 1);
    candidates.set(deployer.address, 1);
    val candHash = candidatesCommitment(candidates);

    val terms: EscrowTerms = {
        treasury: deployer.address,
        expectedHash: resultHash,
        candidatesHash: candHash,
        paramsVersion: 7,
    };
    val storage: EscrowStorage = {
        requester: deployer.address,
        arbiter: deployer.address,
        escrowAmount: B,
        deadline: deadline,
        settled: false,
        terms: terms.toCell(),
        pending: createEmptyMap(),
    };
    val escrow = JobEscrow.fromStorage(storage);
    escrow.deploy(deployer.address, { value: B + ton("0.15") }).waitForTrace();
    println("::ADDR::escrow={}", escrow.address);

    // JE-5: the params version is bound into terms.
    pass("escrow_params_version_bound", escrow.getParamsVersion() == 7);

    // JE-1: settle on the agreed quorum hash — winner (!= arbiter) + fee + the
    // participation commission to the agreeing non-winner (deployer), remainder
    // refunded to the requester. settled flips true.
    var participants: map<address, coins> = createEmptyMap();
    participants.set(deployer.address, participantAmount);
    escrow
        .sendEscrowSettle(
            deployer.address,
            401,
            resultHash,
            winner!,
            winnerAmount,
            platformFee,
            participants,
            candidates,
            { value: ton("0.1") },
        )
        .waitForTrace();
    val es = escrow.getEscrowState();
    pass("escrow_settle_htlc_release", es.settled);

    // JE-4: the winner wallet is uninitialized, so its payout BOUNCED into the
    // pull-ledger (B2). Assert it is claimable, then EscrowClaim re-delivers it.
    val pendingWinner = escrow.getPending(winner!);
    if (pendingWinner > 0) {
        escrow.sendEscrowClaim(deployer.address, 402, winner!, { value: ton("0.05") }).waitForTrace();
        // B2: the claim dequeues the credit and RE-ATTEMPTS delivery. If the
        // winner is an initialized wallet the re-delivery lands and the ledger
        // drains to 0; if the demo winner wallet is uninitialized, the bounceable
        // re-delivery bounces again and the ledger correctly RE-CREDITS the same
        // amount. Both are correct pull-ledger behavior, so assert the claim
        // executed and the funds remain fully accounted (drained OR re-credited).
        // Drained to 0 (initialized recipient accepted it) OR re-credited a
        // strictly smaller amount (uninitialized recipient: the re-delivery
        // bounced again, minus fees) — both prove the claim dequeued + re-attempted.
        val afterClaim = escrow.getPending(winner!);
        pass("escrow_bounce_pull_ledger_claim", afterClaim == 0 || (afterClaim > 0 && afterClaim < pendingWinner));
    } else {
        // Winner was initialized and accepted the payout directly (no bounce).
        println("::CHECK::escrow_bounce_pull_ledger_claim::SKIP (winner accepted payout directly)");
    }
}

fun main() {
    val deployer = scripts.wallet("deployer");
    println("::CHECK::runner_wallet={}", deployer.address);

    if (envFlag("RUN_VAULT", true)) { runVault(deployer); }
    if (envFlag("RUN_ANCHOR", true)) { runAnchor(deployer); }
    if (envFlag("RUN_GP", true)) { runGlobalParams(deployer); }
    if (envFlag("RUN_ESCROW", true)) { runEscrow(deployer); }

    println("::CHECK::done");
}
