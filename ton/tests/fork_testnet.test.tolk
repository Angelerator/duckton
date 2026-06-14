// Fork-test smoke check: read the REAL deployed Duckton StakeVault state from
// testnet via `acton test --fork-net testnet`, to prove fork testing works
// end-to-end against our live deployment (BLOCKCHAIN_ECONOMICS §8 / docs/TESTNET).
//
// This test is SELF-GATING so the default offline `acton test` stays green and
// network-free: it runs the live assertions ONLY when the `FORK_VAULT_ADDR` env
// var is provided (and the run forks testnet). When the var is absent it returns
// immediately as a trivial pass. Run the live check with:
//
//   set -a; . ~/.config/duckdb-p2p/testnet.env; set +a
//   export TONCENTER_TESTNET_API_KEY="$TON_TESTNET_API_KEY"
//   FORK_VAULT_ADDR="<deployed vault>" \
//     acton test tests/fork_testnet.test.tolk --fork-net testnet
//
// (The vault address comes from ton/deployments/testnet.env:TON_TESTNET_VAULT_ADDR.)

import "@acton/build"
import "@acton/env"
import "@acton/emulation/network"
import "@acton/emulation/testing"
import "@acton/testing/expect"

import "@contracts/common"
import "@contracts/stake_types"
import "@wrappers/StakeVault.gen"

get fun `test fork testnet reads the deployed Duckton vault state`() {
    val vaultAddr = env<address>("FORK_VAULT_ADDR");
    if (vaultAddr == null) {
        // Default offline run: no address supplied -> skip (trivial pass). The
        // live read only happens under `--fork-net testnet` with FORK_VAULT_ADDR.
        return;
    }

    val vault = StakeVault.fromAddress(vaultAddr);

    // Read the on-chain jetton-master data from forked testnet state. The vault
    // is its own (Duckton) receipt-jetton master, so it must report itself as the
    // admin and be mintable. (We deliberately do NOT assert the jetton-content or
    // wallet-code hash equals the CURRENT local build: a smoke test should prove
    // the fork READ works, not that the deployed revision is byte-identical to
    // HEAD — the live address may lag local source between redeploys.)
    val jd = vault.getJettonData();
    expect(jd.mintable).toBeTrue();
    expect(jd.adminAddress).toEqual(vaultAddr);

    // Core supply invariant from §8 (holds for any vault revision): receipts
    // outstanding == bonded + cooling.
    val st = vault.getVaultState();
    expect(st.totalSupply).toEqual(st.staked + st.unbondingAmount);
}
