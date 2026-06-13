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
    val minWeight = env<coins>("ANCHOR_MIN_WEIGHT") ?? ton("100");
    val bondMin = env<coins>("ANCHOR_BOND_MIN") ?? ton("1");

    val cfg: AnchorConfig = {
        minStakeWeight: minWeight,
        disputeBondMin: bondMin,
        verdictAuthority: authority,
        treasury: treasury,
    };
    val storage: AnchorStorage = {
        currentEpoch: 0,
        lastRoot: 0,
        nextDisputeId: 0,
        disputes: createEmptyMap(),
        config: cfg.toCell(),
    };

    val anchor = RecordAnchor.fromStorage(storage);
    val res = anchor.deploy(deployer.address, { value: ton("0.1") });
    res.waitForTrace();

    println("Deployed RecordAnchor to {}", anchor.address);
    println("  verdict authority = {}", authority);
    println("  min stake weight  = {}", minWeight);
}
