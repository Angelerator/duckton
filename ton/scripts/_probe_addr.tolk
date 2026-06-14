import "@acton/build"
// Probe: emit reference StateInit hashes / addresses so the off-chain Rust cell
// model (crates/settlement/src/cell.rs) can be pinned byte-for-byte against the
// real TON cell hashing. Run: `acton script scripts/_probe_addr.tolk`.
import "@acton/io"

const OWNER_HASH = 0x00112233_44556677_8899aabb_ccddeeff_00112233_44556677_8899aabb_ccddeeff
const BINDING_HASH = 0xdeadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef

fun main() {
    // (1) Plain StateInit over a simple code + data cell.
    val code = beginCell().storeUint(0xC0DE, 16).endCell();
    val data = beginCell().storeUint(0x12345678, 32).endCell();
    println("SI_CODE_DATA={}", StateInit.calcHashCodeData(code, data));

    // (2) A faithful per-node StakeVault init data cell (matching VaultStorage
    //     field order: owner, staked, unbondingAmount, unbondingAt, totalSupply,
    //     bindingHash, config-ref, upgrade-ref). Config + upgrade are simple
    //     stand-in / fresh child cells. The `upgrade` child is the fresh
    //     VaultUpgradeState: codeVersion(32)=0, pendingCodeHash(256)=0,
    //     pendingCodeAt(32)=0 — matching `freshVaultUpgrade()` and the Rust
    //     `build_vault_data` upgrade child.
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
        .endCell();
    val vaultAddrHash = StateInit.calcHashCodeData(code, vaultData);
    println("VAULT_SI={}", vaultAddrHash);
}
