// Emulation mirror of the live testnet harness (scripts/e2e_testnet.tolk).
//
// Deploys ALL FOUR contracts (GlobalParams, StakeVault, JobEscrow, RecordAnchor)
// in one emulated session and runs the SAME end-to-end sequence the funded
// testnet run executes, asserting each on-chain check. This is the gas-free proof
// that the turnkey flow is wired correctly before any live broadcast:
//
//   0. GlobalParams  — admin update_params persisted (address stable), non-admin
//                      update rejected, governance blocklist round-trip
//   1. StakeVault    — deposit bonds + mints the 1:1 transfer-locked receipt,
//                      Duckton TEP-64 metadata exposed
//   2. JobEscrow     — HTLC settle on the agreed result hash
//   3. RecordAnchor  — chained single-leaf anchor + inclusion, then a multi-leaf
//                      (8-leaf) epoch root anchored with an interior-leaf proof

import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/anchor_types"
import "@contracts/common"
import "@contracts/escrow_types"
import "@contracts/global_params_types"
import "@contracts/stake_types"
import "@wrappers/GlobalParams.gen"
import "@wrappers/JobEscrow.gen"
import "@wrappers/RecordAnchor.gen"
import "@wrappers/StakeReceiptWallet.gen"
import "@wrappers/StakeVault.gen"

const T0: uint32 = 1_700_000_000
const RESULT_HASH: uint256 = 0xABCDEF0123456789

// 8-leaf (3-level) Merkle tree, identical to tests/anchor.test.tolk.
const M0 = 0xa000
const M1 = 0xb001
const M2 = 0xc002
const M3 = 0xd003
const M4 = 0xe004
const M5 = 0xf005
const M6 = 0x1006
const M7 = 0x2007

// MERKLE v2 (A7): leaves are domain-separated via hashLeaf before pairing.
fun tree8Left(): int {
    return hashPair(hashPair(hashLeaf(M0), hashLeaf(M1)), hashPair(hashLeaf(M2), hashLeaf(M3)));
}

fun tree8Right(): int {
    return hashPair(hashPair(hashLeaf(M4), hashLeaf(M5)), hashPair(hashLeaf(M6), hashLeaf(M7)));
}

fun tree8Root(): int {
    return hashPair(tree8Left(), tree8Right());
}

// Proof for interior leaf M5: the verifier seeds acc = hashLeaf(M5); the level-0
// sibling is therefore hashLeaf(M4) (a leaf-domain hash), the rest are node hashes.
fun proofForM5(): Cell<MerkleStep>? {
    val top: MerkleStep = {
        dir: 1,
        sibling: tree8Left(),
        next: null,
    };
    val mid: MerkleStep = {
        dir: 0,
        sibling: hashPair(hashLeaf(M6), hashLeaf(M7)),
        next: top.toCell(),
    };
    val bottom: MerkleStep = {
        dir: 1,
        sibling: hashLeaf(M4),
        next: mid.toCell(),
    };
    return bottom.toCell();
}

fun validParams(): EcoParams {
    return {
        platformFeeBps: 1500,
        surchargeBps: 500,
        participationCommissionBps: 500,
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

fun deployGlobalParams(admin: address): GlobalParams {
    val storage: GlobalParamsStorage = {
        admin: admin,
        feeRecipient: admin,
        params: validParams().toCell(),
        blocklist: createEmptyMap(),
        paramsVersion: 1,
        codeVersion: 0,
        upgradeDelay: 86400,
        pendingCodeHash: 0,
        pendingCodeAt: 0,
    };
    val gp = GlobalParams.fromStorage(storage);
    expect(gp.deploy(admin, { value: ton("1") })).toHaveSuccessfulDeploy({ to: gp.address });
    return gp;
}

fun deployVault(owner: address, slasher: address): StakeVault {
    val slash: SlashConfig = {
        treasury: owner,
        redundancyPool: owner,
        splitChallengerBps: 4000,
        splitRedundancyBps: 3000,
        splitBurnBps: 2000,
        splitTreasuryBps: 1000,
    };
    val cfg: VaultConfig = {
        minStake: ton("10"),
        unbondingPeriod: 604800,
        challengeWindow: 86400,
        keeperGrace: 86400,
        keeperBountyBps: 200,
        slasher: slasher,
        upgradeAuthority: slasher,
        receiptWalletCode: build("StakeReceiptWallet"),
        slash: slash.toCell(),
    };
    val storage: VaultStorage = {
        owner: owner,
        staked: 0,
        unbondingAmount: 0,
        unbondingAt: 0,
        totalSupply: 0,
        bindingHash: 0xDEADBEEF,
        config: cfg.toCell(),
        upgrade: freshVaultUpgrade(),
        pending: createEmptyMap(),
    };
    val vault = StakeVault.fromStorage(storage);
    expect(vault.deploy(owner, { value: ton("1") })).toHaveSuccessfulDeploy({ to: vault.address });
    return vault;
}

// Build the requester-committed candidate set (B1) + its commitment.
fun candidateSet(a: address): map<address, uint1> {
    var c: map<address, uint1> = createEmptyMap();
    c.set(a, 1);
    return c;
}

fun deployEscrow(requester: address, arbiter: address, winner: address): JobEscrow {
    val terms: EscrowTerms = {
        treasury: requester,
        expectedHash: RESULT_HASH,
        candidatesHash: candidatesCommitment(candidateSet(winner)),
        paramsVersion: 1,
        platformFeeBps: 1500,          // φ = 15% bound + enforced at settle
        participationCommissionBps: 0, // κ check inert (winner-only flow, no commissions)
    };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: ton("100"),
        deadline: 1_700_000_500,
        settled: false,
        terms: terms.toCell(),
        pending: createEmptyMap(),
    };
    val escrow = JobEscrow.fromStorage(storage);
    expect(
        escrow.deploy(requester, { value: ton("102") }),
    ).toHaveSuccessfulDeploy({ to: escrow.address });
    return escrow;
}

fun deployAnchor(authority: address, keeper: address): RecordAnchor {
    val auth: AnchorAuthorities = {
        treasury: authority,
        keeper: keeper,
        upgradeAuthority: authority,
    };
    val cfg: AnchorConfig = {
        minStakeWeight: ton("100"),
        disputeBondMin: ton("1"),
        verdictAuthority: authority,
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
        pendingCodeHash: 0,
        pendingCodeAt: 0,
        upgradeDelay: 0,
    };
    val anchor = RecordAnchor.fromStorage(storage);
    expect(
        anchor.deploy(authority, { value: ton("1") }),
    ).toHaveSuccessfulDeploy({ to: anchor.address });
    return anchor;
}

get fun `test turnkey e2e flow across all four contracts (emulation mirror)`() {
    testing.setNow(T0);
    val deployer = testing.treasury("deployer");
    val attacker = testing.treasury("attacker");

    // -- 0. GlobalParams -----------------------------------------------------
    val gp = deployGlobalParams(deployer.address);
    val gpAddr = gp.address;

    // (a) admin update_params: toggle fee bps, assert persisted + address stable.
    var updated = validParams();
    updated.platformFeeBps = 350;
    expect(
        gp.sendUpdateParams(
            deployer.address,
            100,
            deployer.address,
            updated,
            { value: ton("0.5") },
        ),
    ).toHaveSuccessfulTx<UpdateParams>({ from: deployer.address, to: gpAddr });
    val same = GlobalParams.fromAddress(gpAddr);
    expect(same.getParams().params.load().platformFeeBps).toEqual(350);
    expect(same.getAdmin()).toEqual(deployer.address);
    expect(same.address).toEqual(gpAddr);

    // (b) non-admin update_params is rejected; state unchanged.
    var evil = validParams();
    evil.platformFeeBps = 9999;
    expect(
        gp.sendUpdateParams(attacker.address, 101, attacker.address, evil, { value: ton("0.5") }),
    ).toHaveFailedTx({ from: attacker.address, to: gpAddr, exitCode: ErrCode.NOT_ADMIN });
    expect(gp.getParams().params.load().platformFeeBps).toEqual(350);

    // (c) governance blocklist round-trip.
    val subject = 0x1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0_1234_5678_9abc_def0;
    expect(gp.getBlocklisted(subject)).toEqual(false);
    gp.sendSetBlocklisted(deployer.address, 102, subject, true, { value: ton("0.1") });
    expect(gp.getBlocklisted(subject)).toEqual(true);
    gp.sendSetBlocklisted(deployer.address, 103, subject, false, { value: ton("0.1") });
    expect(gp.getBlocklisted(subject)).toEqual(false);

    // -- 1. StakeVault: deposit, 1:1 transfer-locked receipt, Duckton metadata --
    val vault = deployVault(deployer.address, deployer.address);
    expect(
        vault.sendStakeDeposit(deployer.address, 1, ton("100"), { value: ton("103") }),
    ).toHaveSuccessfulTx<StakeDeposit>({ from: deployer.address, to: vault.address });
    expect(vault.getVaultState().staked).toEqual(ton("100"));

    val receipt = StakeReceiptWallet.fromAddress(vault.getReceiptWalletAddress(deployer.address));
    expect(receipt.getWalletData().balance).toEqual(ton("100"));
    expect(receipt.isTransferLocked()).toBeTrue();

    // A transfer attempt is rejected (anti-exit); balance untouched.
    expect(
        receipt.sendJettonTransfer(
            deployer.address,
            11,
            ton("100"),
            attacker.address,
            deployer.address,
            null,
            ton("0"),
            beginCell().endCell().beginParse(),
            { value: ton("1") },
        ),
    ).toHaveFailedTx({ from: deployer.address, to: receipt.address, exitCode: ErrCode.RECEIPT_LOCKED });
    expect(receipt.getWalletData().balance).toEqual(ton("100"));

    // Duckton TEP-64 metadata exactly matches the canonical content bytes.
    val jd = vault.getJettonData();
    expect(jd.mintable).toBeTrue();
    expect(jd.adminAddress).toEqual(vault.address);
    expect(jd.jettonWalletCode.hash()).toEqual(build("StakeReceiptWallet").hash());
    expect(jd.jettonContent.hash()).toEqual(buildReceiptJettonContent().hash());

    // -- 2. JobEscrow: HTLC settle on the agreed result hash -----------------
    // B1: the winner must be in the requester-committed candidate set and cannot
    // be the arbiter, so use a distinct winner wallet.
    val winner = randomAddress("winner");
    val escrow = deployEscrow(deployer.address, deployer.address, winner);
    var participants: map<address, coins> = createEmptyMap();
    expect(
        escrow.sendEscrowSettle(
            deployer.address,
            2,
            RESULT_HASH,
            winner,
            ton("60"), // quoted base (fee base)
            ton("60"), // wallet winner paid the full base
            ton("9"),  // φ=15% of the 60 base, enforced on-chain
            participants,
            candidateSet(winner),
            { value: ton("1") },
        ),
    ).toHaveSuccessfulTx<EscrowSettle>({ from: deployer.address, to: escrow.address });
    expect(escrow.getEscrowState().settled).toBeTrue();

    // -- 3. RecordAnchor: chained single-leaf + multi-leaf inclusion ---------
    // A3: a configured keeper anchors; here the deployer is the keeper.
    val anchor = deployAnchor(deployer.address, deployer.address);

    // Single-leaf epoch 1: MERKLE v2 → a 1-leaf tree's root is hashLeaf(leaf).
    val singleRoot = hashLeaf(RESULT_HASH);
    expect(
        anchor.sendAnchorSubmitRoot(
            deployer.address,
            3,
            1,
            singleRoot,
            0,
            ton("100"),
            { value: ton("1") },
        ),
    ).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: deployer.address, to: anchor.address });
    expect(anchor.verifyInclusion(RESULT_HASH, singleRoot, null)).toBeTrue();

    // Multi-leaf epoch 2, chained to epoch 1; verify interior leaf M5.
    val mlRoot = tree8Root();
    expect(
        anchor.sendAnchorSubmitRoot(
            deployer.address,
            31,
            2,
            mlRoot,
            singleRoot,
            ton("100"),
            { value: ton("1") },
        ),
    ).toHaveSuccessfulTx<AnchorSubmitRoot>({ from: deployer.address, to: anchor.address });
    expect(anchor.getAnchorState().currentEpoch).toEqual(2);
    expect(anchor.getAnchorState().lastRoot).toEqual(mlRoot);
    expect(anchor.verifyInclusion(M5, mlRoot, proofForM5())).toBeTrue();
    expect(anchor.computeRoot(M5, proofForM5())).toEqual(mlRoot);
    expect(anchor.verifyInclusion(M5 + 1, mlRoot, proofForM5())).toBeFalse();
}
