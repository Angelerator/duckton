import "@acton/build"
import "@acton/io"
import "@contracts/escrow_types"

// Probe (read-only): emit reference StateInit hashes / cell hashes for the v2
// storage layouts (VaultStorage gains a trailing `pending` map; EscrowTerms gains
// `candidatesHash`; EscrowStorage gains a trailing `pending` map). Data cells are
// hand-built in the EXACT contract `.toCell()` field order (verified from
// stake_types.tolk / escrow_types.tolk) so they mirror the off-chain Rust
// builders; `candidatesCommitment` is the REAL contract function. The code +
// owner + binding + config stand-ins MATCH `_probe_addr.tolk` and the existing
// pinned offline tests. Run: `acton script scripts/_probe_v2.tolk`.

const OWNER_HASH = 0x00112233_44556677_8899aabb_ccddeeff_00112233_44556677_8899aabb_ccddeeff
const BINDING_HASH = 0xdeadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef
const TREASURY_HASH = 0x7e7e7e7e_7e7e7e7e_7e7e7e7e_7e7e7e7e_7e7e7e7e_7e7e7e7e_7e7e7e7e_7e7e7e7e
const EXPECTED_HASH = 0xabababab_abababab_abababab_abababab_abababab_abababab_abababab_abababab
const WINNER = 0x0202020202020202020202020202020202020202020202020202020202020202
const KEY1 = 0x1111111111111111111111111111111111111111111111111111111111111111

fun main() {
    val code = beginCell().storeUint(0xC0DE, 16).endCell();
    // An empty `map<K,V>` (HashmapE) serializes as a single `0` bit (hme_empty$0).

    // (1) v2 VaultStorage StateInit. Field order (stake_types.tolk::VaultStorage):
    //     owner, staked, unbondingAmount, unbondingAt, totalSupply, bindingHash,
    //     config(ref), upgrade(ref), pending(dict). Stand-in code/config, fresh
    //     upgrade child (all-zero), EMPTY pending.
    val owner = address.fromWorkchainAndHash(0, OWNER_HASH);
    val config = beginCell().storeUint(0xCF, 8).endCell();
    val upgrade = beginCell().storeUint(0, 32).storeUint(0, 256).storeUint(0, 32).endCell();
    val vaultData = beginCell()
        .storeAddress(owner)
        .storeCoins(0)
        .storeCoins(0)
        .storeUint(0, 32)
        .storeCoins(0)
        .storeUint(BINDING_HASH, 256)
        .storeRef(config)
        .storeRef(upgrade)
        .storeUint(0, 1)
        .endCell();
    println("VAULT_SI_V2={}", StateInit.calcHashCodeData(code, vaultData));

    // (2) candidatesCommitment over {winner, KEY1} via the REAL contract fn.
    val winner = address.fromWorkchainAndHash(0, WINNER);
    var cands: map<address, uint1> = createEmptyMap();
    cands.set(winner, 1);
    cands.set(address.fromWorkchainAndHash(0, KEY1), 1);
    val candHash = candidatesCommitment(cands);
    println("CANDIDATES_COMMITMENT_winnerKey1={}", candHash);

    var emptyCands: map<address, uint1> = createEmptyMap();
    println("CANDIDATES_COMMITMENT_empty={}", candidatesCommitment(emptyCands));

    // (3) v3 EscrowTerms cell hash. Field order (escrow_types.tolk::EscrowTerms):
    //     treasury, expectedHash, candidatesHash, paramsVersion, platformFeeBps(uint16),
    //     participationCommissionBps(uint16).
    val treasury = address.fromWorkchainAndHash(0, TREASURY_HASH);
    val termsData = beginCell()
        .storeAddress(treasury)
        .storeUint(EXPECTED_HASH, 256)
        .storeUint(candHash, 256)
        .storeUint(7, 32)
        .storeUint(1500, 16)
        .storeUint(500, 16)
        .endCell();
    println("ESCROW_TERMS_V3={}", termsData.hash());

    // (3b) EscrowTerms with candidatesHash = 0 (unbound) — the shape
    //      `build_escrow_terms(treasury, expected, 0, 7, 1500, 500)` produces.
    val termsUnboundData = beginCell()
        .storeAddress(treasury)
        .storeUint(EXPECTED_HASH, 256)
        .storeUint(0, 256)
        .storeUint(7, 32)
        .storeUint(1500, 16)
        .storeUint(500, 16)
        .endCell();
    println("ESCROW_TERMS_V3_UNBOUND={}", termsUnboundData.hash());

    // (4) v2 EscrowStorage StateInit. Field order (escrow_types.tolk::EscrowStorage):
    //     requester, arbiter, escrowAmount, deadline, settled(1 bit), terms(ref),
    //     pending(dict). Uses the bound terms from (3, now with platformFeeBps); EMPTY pending.
    val requester = address.fromWorkchainAndHash(0, KEY1);
    val arbiter = address.fromWorkchainAndHash(0, WINNER);
    val escrowData = beginCell()
        .storeAddress(requester)
        .storeAddress(arbiter)
        .storeCoins(1000)
        .storeUint(42, 32)
        .storeUint(0, 1)
        .storeRef(termsData)
        .storeUint(0, 1)
        .endCell();
    println("ESCROW_SI_V2={}", StateInit.calcHashCodeData(code, escrowData));
}
