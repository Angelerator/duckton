// Property-based (fuzz) tests for the money rules. Each `@test.fuzz` feeds many
// random inputs (deterministic via [test.fuzz].seed in Acton.toml) and asserts a
// money invariant holds for ALL of them: slash conservation + split, escrow
// payout bounded by the locked B, stake bucket consistency, GlobalParams bounds,
// and Merkle inclusion round-trips. `fuzz.bound` maps raw inputs into range;
// `fuzz.assume` discards inputs that violate a precondition.

import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"
import "@acton/testing/fuzz"
import "@acton/tolk-stdlib/tvm-dicts"

import "@contracts/common"
import "@contracts/stake_types"
import "@contracts/escrow_types"
import "@contracts/global_params_types"
import "@contracts/anchor_types"
import "@wrappers/StakeVault.gen"
import "@wrappers/JobEscrow.gen"
import "@wrappers/GlobalParams.gen"
import "@wrappers/RecordAnchor.gen"

const T0: uint32 = 1_700_000_000
const RESULT_HASH: uint256 = 0xABCDEF0123456789
const NANO: int = 1_000_000_000

// --- local deploy helpers (self-contained) ---------------------------------

fun deployVaultSplit(
    owner: address,
    slasher: address,
    treasury: address,
    redundancy: address,
    splitCh: uint16,
    splitRe: uint16,
    splitBu: uint16,
    splitTr: uint16,
): StakeVault {
    val slash: SlashConfig = {
        treasury: treasury,
        redundancyPool: redundancy,
        splitChallengerBps: splitCh,
        splitRedundancyBps: splitRe,
        splitBurnBps: splitBu,
        splitTreasuryBps: splitTr,
    };
    val cfg: VaultConfig = {
        minStake: ton("0"),
        unbondingPeriod: 1000,
        challengeWindow: 500,
        keeperGrace: 100,
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

// B1: a single-candidate set (the winner) + its commitment.
fun candidateSet(a: address): map<address, uint1> {
    var c: map<address, uint1> = createEmptyMap();
    c.set(a, 1);
    return c;
}

fun deployEscrow(requester: address, arbiter: address, treasury: address, winner: address, amount: coins): JobEscrow {
    val terms: EscrowTerms = {
        treasury: treasury,
        expectedHash: RESULT_HASH,
        candidatesHash: candidatesCommitment(candidateSet(winner)),
        paramsVersion: 1,
    };
    val storage: EscrowStorage = {
        requester: requester,
        arbiter: arbiter,
        escrowAmount: amount,
        deadline: 1_700_000_500,
        settled: false,
        terms: terms.toCell(),
        pending: createEmptyMap(),
    };
    val escrow = JobEscrow.fromStorage(storage);
    expect(escrow.deploy(requester, { value: amount + ton("2") })).toHaveSuccessfulDeploy({ to: escrow.address });
    return escrow;
}

fun validParams(): EcoParams {
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

fun deployAnchor(authority: address): RecordAnchor {
    val auth: AnchorAuthorities = {
        treasury: authority,
        keeper: authority,
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
    };
    val anchor = RecordAnchor.fromStorage(storage);
    expect(anchor.deploy(authority, { value: ton("1") })).toHaveSuccessfulDeploy({ to: anchor.address });
    return anchor;
}

// --- fuzz properties --------------------------------------------------------

@test.fuzz(64)
get fun `test fuzz slash from staked conserves balance and applies the fixed split`(rawAmount: int) {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    // C1: split recipients are bounceable payouts → must be DEPLOYED to receive.
    val challenger = testing.treasury("ch");
    val treasury = testing.treasury("tr");
    val redundancy = testing.treasury("re");
    val staked: coins = (100 * NANO) as coins;
    val vault = deployVaultSplit(owner.address, slasher.address, treasury.address, redundancy.address, 4000, 3000, 2000, 1000);
    vault.sendStakeDeposit(owner.address, 1, staked, { value: ton("110") });

    val cBefore = testing.getAccountBalance(challenger.address);
    val rBefore = testing.getAccountBalance(redundancy.address);
    val tBefore = testing.getAccountBalance(treasury.address);
    // Slash any amount in [1, staked]; it always comes out of the bonded bucket.
    val a: coins = (fuzz.bound(rawAmount, 1, 100) * NANO) as coins;
    val res = vault.sendStakeSlash(slasher.address, 2, a, 1, challenger.address, { value: ton("1") });
    expect(res).toHaveSuccessfulTx<StakeSlash>({ from: slasher.address, to: vault.address });

    val st = vault.getVaultState();
    expect(st.staked).toEqual(staked - a);
    expect(st.totalSupply).toEqual(staked - a);
    // Split payouts are mulDivFloor(amount, bps, 10000); the recipient nets that
    // minus a few microTON of gas to process the op-tagged (bounceable) payout.
    val cd = testing.getAccountBalance(challenger.address) - cBefore;
    val rd = testing.getAccountBalance(redundancy.address) - rBefore;
    val td = testing.getAccountBalance(treasury.address) - tBefore;
    val GAS: coins = ton("0.005");
    expect(cd).toBeLessOrEqual((a * 4000 / 10000) as coins);
    expect(cd).toBeGreaterOrEqual(((a * 4000 / 10000) as coins) - GAS);
    expect(rd).toBeLessOrEqual((a * 3000 / 10000) as coins);
    expect(rd).toBeGreaterOrEqual(((a * 3000 / 10000) as coins) - GAS);
    expect(td).toBeLessOrEqual((a * 1000 / 10000) as coins);
    expect(td).toBeGreaterOrEqual(((a * 1000 / 10000) as coins) - GAS);
}

@test.fuzz(64)
get fun `test fuzz any split summing to 100% conserves the slashed amount`(c: int, r: int, b: int) {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    // C1: split recipients are bounceable payouts → must be DEPLOYED to receive.
    val challenger = testing.treasury("ch");
    val treasury = testing.treasury("tr");
    val redundancy = testing.treasury("re");

    // Construct a valid split that sums to exactly 10000 bps.
    val ch = fuzz.bound(c, 0, 10000);
    val re = fuzz.bound(r, 0, 10000 - ch);
    val bu = fuzz.bound(b, 0, 10000 - ch - re);
    val tr = 10000 - ch - re - bu;

    val staked: coins = (100 * NANO) as coins;
    val vault = deployVaultSplit(
        owner.address, slasher.address, treasury.address, redundancy.address,
        ch as uint16, re as uint16, bu as uint16, tr as uint16,
    );
    vault.sendStakeDeposit(owner.address, 1, staked, { value: ton("110") });

    val cBefore = testing.getAccountBalance(challenger.address);
    val rBefore = testing.getAccountBalance(redundancy.address);
    val tBefore = testing.getAccountBalance(treasury.address);
    val res = vault.sendStakeSlash(slasher.address, 2, staked, 1, challenger.address, { value: ton("1") });
    expect(res).toHaveSuccessfulTx<StakeSlash>({ from: slasher.address, to: vault.address });

    // Conservation: paid-out shares (floored) never exceed the slashed amount and
    // match the per-share floor minus a little gas for the op-tagged payout.
    val paidCh = testing.getAccountBalance(challenger.address) - cBefore;
    val paidRe = testing.getAccountBalance(redundancy.address) - rBefore;
    val paidTr = testing.getAccountBalance(treasury.address) - tBefore;
    val GAS: coins = ton("0.005");
    expect(paidCh).toBeLessOrEqual((staked * ch / 10000) as coins);
    expect(paidCh).toBeGreaterOrEqual(((staked * ch / 10000) as coins) - GAS);
    expect(paidRe).toBeLessOrEqual((staked * re / 10000) as coins);
    expect(paidRe).toBeGreaterOrEqual(((staked * re / 10000) as coins) - GAS);
    expect(paidTr).toBeLessOrEqual((staked * tr / 10000) as coins);
    expect(paidTr).toBeGreaterOrEqual(((staked * tr / 10000) as coins) - GAS);
    expect(paidCh + paidRe + paidTr).toBeLessOrEqual(staked);
}

@test.fuzz(64)
get fun `test fuzz escrow settles iff winner+fee is within the locked B`(w: int, f: int) {
    testing.setNow(T0);
    val requester = testing.treasury("req");
    val arbiter = testing.treasury("arb");
    // C1: a winner receiving a bounceable payout must be DEPLOYED to receive it.
    val winner = testing.treasury("win");
    val treasury = randomAddress("tre");
    val B: coins = (100 * NANO) as coins;
    val escrow = deployEscrow(requester.address, arbiter.address, treasury, winner.address, B);

    val wa: coins = (fuzz.bound(w, 0, 150) * NANO) as coins;
    val fa: coins = (fuzz.bound(f, 0, 50) * NANO) as coins;
    val total = wa + fa;

    val wBefore = testing.getAccountBalance(winner.address);
    var participants: map<address, coins> = createEmptyMap();
    val res = escrow.sendEscrowSettle(arbiter.address, 1, RESULT_HASH, winner.address, wa, fa, participants, candidateSet(winner.address), { value: ton("1") });

    if (total <= B) {
        expect(res).toHaveSuccessfulTx<EscrowSettle>({ from: arbiter.address, to: escrow.address });
        expect(escrow.getEscrowState().settled).toBeTrue();
        if (wa > 0) {
            // winner nets ~wa (minus a few microTON of gas for the op-tagged payout).
            val wd = testing.getAccountBalance(winner.address) - wBefore;
            expect(wd).toBeLessOrEqual(wa);
            expect(wd).toBeGreaterOrEqual(wa - ton("0.005"));
        }
    } else {
        expect(
            res,
        ).toHaveFailedTx({ from: arbiter.address, to: escrow.address, exitCode: ErrCode.PAYOUT_EXCEEDS_ESCROW });
        expect(escrow.getEscrowState().settled).toBeFalse();
    }
}

@test.fuzz(64)
get fun `test fuzz deposit then unbond keeps the bonded and cooldown buckets consistent`(d: int, u: int) {
    testing.setNow(T0);
    val owner = testing.treasury("owner");
    val slasher = testing.treasury("slasher");
    val vault = deployVaultSplit(
        owner.address, slasher.address, randomAddress("t"), randomAddress("r"), 4000, 3000, 2000, 1000,
    );

    val deposit: coins = (fuzz.bound(d, 1, 100) * NANO) as coins;
    vault.sendStakeDeposit(owner.address, 1, deposit, { value: deposit + ton("1") });
    expect(vault.getVaultState().staked).toEqual(deposit);

    val unbond: coins = (fuzz.bound(u, 1, 130) * NANO) as coins;
    val res = vault.sendStakeRequestUnbond(owner.address, 2, unbond);
    if (unbond <= deposit) {
        expect(res).toHaveSuccessfulTx<StakeRequestUnbond>({ from: owner.address, to: vault.address });
        val st = vault.getVaultState();
        expect(st.staked).toEqual(deposit - unbond);
        expect(st.unbondingAmount).toEqual(unbond);
        expect(st.totalSupply).toEqual(deposit); // receipts only burn on withdraw/slash
    } else {
        expect(
            res,
        ).toHaveFailedTx({ from: owner.address, to: vault.address, exitCode: ErrCode.NOTHING_TO_UNBOND });
    }
}

@test.fuzz(64)
get fun `test fuzz platform fee bps is accepted iff within [0, 10000]`(x: int) {
    val admin = testing.treasury("admin");
    val gp = deployGlobalParams(admin.address);

    val fee = fuzz.bound(x, 0, 20000);
    var p = validParams();
    p.platformFeeBps = fee as uint16;
    val res = gp.sendUpdateParams(admin.address, 1, randomAddress("f"), p, { value: ton("0.5") });

    if (fee <= 10000) {
        expect(res).toHaveSuccessfulTx<UpdateParams>({ from: admin.address, to: gp.address });
        expect(gp.getParams().params.load().platformFeeBps).toEqual(fee);
    } else {
        expect(res).toHaveFailedTx({ from: admin.address, to: gp.address, exitCode: ErrCode.PARAMS_OUT_OF_BOUNDS });
        expect(gp.getParams().params.load().platformFeeBps).toEqual(200); // unchanged
    }
}

@test.fuzz(64)
get fun `test fuzz two-leaf merkle inclusion always round-trips and rejects tampering`(lRaw: int, rRaw: int) {
    testing.setNow(T0);
    val anchor = deployAnchor(randomAddress("auth"));

    val leftLeaf = fuzz.bound(lRaw, 1, 1_000_000_000);
    val rightLeaf = fuzz.bound(rRaw, 1, 1_000_000_000);
    fuzz.assume(leftLeaf != rightLeaf);

    // MERKLE v2 (A7): leaves are domain-separated via hashLeaf before pairing, so
    // the sibling in a proof is the OTHER leaf's hashLeaf, not its raw value.
    val root = hashPair(hashLeaf(leftLeaf), hashLeaf(rightLeaf));
    // Inclusion proof for the left leaf: sibling is the right node (dir = 0).
    val proofLeft: MerkleStep = { dir: 0, sibling: hashLeaf(rightLeaf), next: null };
    expect(anchor.verifyInclusion(leftLeaf, root, proofLeft.toCell())).toBeTrue();
    expect(anchor.computeRoot(leftLeaf, proofLeft.toCell())).toEqual(root);

    // Inclusion proof for the right leaf: sibling is the left node (dir = 1).
    val proofRight: MerkleStep = { dir: 1, sibling: hashLeaf(leftLeaf), next: null };
    expect(anchor.verifyInclusion(rightLeaf, root, proofRight.toCell())).toBeTrue();

    // A tampered leaf must NOT verify against the same proof.
    expect(anchor.verifyInclusion(leftLeaf + 1, root, proofLeft.toCell())).toBeFalse();
}
