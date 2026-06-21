// Deploy the RecordAnchor (per-epoch Merkle-root anchor + dispute) contract.
//
// Network-agnostic — run locally or against testnet/mainnet:
//     acton script scripts/deploy_anchor.tolk
//     acton script scripts/deploy_anchor.tolk --net testnet
//
// Parameters come from the environment with defaults, e.g.:
//     ANCHOR_AUTHORITY=<addr> ANCHOR_TREASURY=<addr> ANCHOR_MIN_WEIGHT=100 \
//     ANCHOR_BOND_MIN=1 acton script scripts/deploy_anchor.tolk --net testnet

import "@acton/io"
import "@acton/env"
import "@acton/build"
import "@acton/emulation/network"
import "@acton/emulation/scripts"
import "@contracts/anchor_types"
import "@wrappers/RecordAnchor.gen"

fun main() {
    val deployer = scripts.wallet("deployer");

    val authority = env<address>("ANCHOR_AUTHORITY") ?? deployer.address;
    val treasury = env<address>("ANCHOR_TREASURY") ?? deployer.address;
    // A3: the authorized root-submission keeper (defaults to the authority).
    val keeper = env<address>("ANCHOR_KEEPER") ?? authority;
    // D2: distinct upgrade authority (defaults to the authority; set to a multisig).
    val upgradeAuthority = env<address>("ANCHOR_UPGRADE_AUTHORITY") ?? authority;
    val minWeight = env<coins>("ANCHOR_MIN_WEIGHT") ?? ton("100");
    val bondMin = env<coins>("ANCHOR_BOND_MIN") ?? ton("1");
    // D2: timelock for in-place code upgrades (announce -> wait -> apply). Default
    // 7 days so the ecosystem can react to a queued anchor code change.
    val upgradeDelay = env<int>("ANCHOR_UPGRADE_DELAY") ?? 604800;

    val auth: AnchorAuthorities = {
        treasury: treasury,
        keeper: keeper,
        upgradeAuthority: upgradeAuthority,
    };
    val cfg: AnchorConfig = {
        minStakeWeight: minWeight,
        disputeBondMin: bondMin,
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
        // Fresh deploy = original code; `upgrade_code` bumps it in place.
        codeVersion: 0,
        // D2: no upgrade announced yet; timelock window for when one is.
        pendingCodeHash: 0,
        pendingCodeAt: 0,
        upgradeDelay: upgradeDelay as uint32,
    };

    val anchor = RecordAnchor.fromStorage(storage);
    val res = anchor.deploy(deployer.address, { value: ton("0.1") });
    res.waitForTrace();

    println("Deployed RecordAnchor to {}", anchor.address);
    println("  verdict authority = {}", authority);
    println("  min stake weight  = {}", minWeight);
}
