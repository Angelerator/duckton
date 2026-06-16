# Duckton — TON settlement contracts (Tolk / Acton)

The on-chain economic/settlement layer for the P2P DuckDB-over-QUIC grid. See
[`docs/BLOCKCHAIN_ECONOMICS.md`](../docs/BLOCKCHAIN_ECONOMICS.md) for the full
design. These contracts are only ever touched by **paid** jobs — free jobs run
entirely off-chain (see §8.2.1) and never reach any of this.

## Toolchain: Acton

Built and tested with [**Acton**](https://ton-blockchain.github.io/acton/) — the
Rust-based all-in-one TON toolkit (scaffold, build, test, lint, fmt, wallet,
deploy, verify) with native Tolk + `@stdlib`/`@acton` support. Chosen over
Blueprint to align with this project's Rust stack and for its Tolk-native test
runner (traces + coverage).

```bash
# install (macOS/Linux): see https://ton-blockchain.github.io/acton/docs/installation
curl -LsSf https://github.com/ton-blockchain/acton/releases/latest/download/acton-installer.sh | sh

acton build          # compile all contracts
acton test           # run the Tolk emulation tests
acton test --coverage --coverage-format text
acton fmt            # format
```

## Contracts (sharded; no global contract)

| Contract | Role | Design |
|---|---|---|
| `StakeVault` | per-node bond custody (non-custodial): deposit, unbond cooldown, graduated slashing + split, keeper withdraw; also the receipt jetton master | §8 |
| `StakeReceiptWallet` | 1:1 TEP-74 stake-receipt jetton, **transfer-LOCKED** while bonded (anti-exit) | §8.5 |
| `JobEscrow` | per-job non-custodial escrow; HTLC-style release keyed on the quorum result hash; refund-on-timeout | §6.2 |
| `RecordAnchor` | per-epoch Merkle root (chained, stake-weighted acceptance) from permissionless keepers + bonded dispute/verdict | §7, §11 |

Sources in `contracts/`, generated wrappers in `wrappers/` (`acton wrapper --all`),
Tolk tests in `tests/`, deploy scripts in `scripts/`.

## Testnet deploy / verify (parameterized, network-agnostic)

The deploy scripts read all parameters from the environment, so the same script
runs in local emulation or against testnet/mainnet:

```bash
# local emulation
acton script scripts/deploy_stake.tolk
acton script scripts/deploy_anchor.tolk

# testnet (create + fund a wallet first)
acton wallet new --name deployer --local --airdrop --version v5r1
STAKE_MIN=100 STAKE_SLASHER=<addr> STAKE_TREASURY=<addr> \
  acton script scripts/deploy_stake.tolk --net testnet

# verify published bytecode against source
acton verify StakeVault
```

`ton_proof` two-way wallet↔node binding verification and the off-chain BLAKE3
receipt Merkle tree live on the Rust side in
[`crates/settlement`](../crates/settlement).
