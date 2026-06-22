# Paid queries (TON)

Public jobs are **free and fully off-chain** — no wallet, no escrow, no fees, no
chain client. When you want **guaranteed, accountable compute**, a job can be
**paid** through a per-job escrow on **The Open Network (TON)**. It is strictly
opt-in and **default-off**. For the full design see
[How it works](../HOW_IT_WORKS.md) and [Blockchain economics](../BLOCKCHAIN_ECONOMICS.md).

## The on-chain settlement model

A paid query locks the requester's max bid `B` in a per-job **`JobEscrow`**. On
settle, the contract pays out and the splits are **enforced on-chain**:

- the **winner** receives its quoted base,
- a **platform fee** (e.g. 15%) goes to the admin treasury (`GlobalParams.fee_recipient`),
- a **commission** (e.g. 5%) goes to **each agreeing wallet verifier**,
- the **remainder is refunded** to the requester.

A **free (walletless) winner** is paid nothing and its base is refunded — but the
platform fee and verifier commissions are *still* collected, so the platform and
verifiers earn on every paid job regardless of the node mix. Settle is rejected
on-chain if the fee is wrong (`FEE_MISMATCH`), the commission is shaved
(`COMMISSION_MISMATCH`), or the escrow can't cover the split
(`PAYOUT_EXCEEDS_ESCROW`).

The four contracts in brief: **`GlobalParams`** (admin-set fees/params singleton),
**`JobEscrow`** (per-job HTLC-style escrow), **`StakeVault`** (per-node bonded
stake with a 7-day unbond and a 1:1 transfer-locked receipt jetton), and
**`RecordAnchor`** (per-epoch Merkle-root anchoring + bonded disputes).

## Network mode (real-funds safety)

`economics.network` defaults to **`testnet`** (never silently mainnet). Switching
to mainnet requires an explicit confirmation, and paid/on-chain actions on
mainnet are blocked until confirmed:

```sql
CALL p2p_economics(network => 'mainnet');                  -- ERROR: requires confirm (real TON)
CALL p2p_economics(network => 'mainnet', confirm => true); -- OK, with a clear warning
```

Both networks can be configured at once — contract addresses, wallet/API-key
references, and RPC are stored **per network**, so flipping `network` switches
endpoints without reconfiguring. `p2p_status()` prominently shows the active
network and warns when mainnet is active.

## Enable economics & configure a wallet

```sql
-- Turn on the money rail and pick the network + treasury:
CALL p2p_economics(enabled => true, settlement => 'ton', network => 'testnet',
                   fee_recipient => 'kQ...');

-- Configure the wallet. Prefer FILE references over pasting secrets into SQL:
CALL p2p_wallet(rpc => 'https://testnet.toncenter.com/api/v2/',
                mnemonic_file => '/path/outside/repo/wallet.mnemonic',
                api_key_file  => '/path/outside/repo/toncenter.key',
                address       => 'kQ...');

-- Point at deployed contracts (per network):
CALL p2p_contracts(global_params => 'kQ...', job_escrow => 'kQ...',
                   stake_vault => 'kQ...', record_anchor => 'kQ...');
```

!!! danger "Never paste raw secrets into SQL"
    If a raw inline `mnemonic`/`api_key` is supplied, it is written to a `0600`
    file **outside the repo** and only the path reference is persisted — the raw
    secret is never written to the config file and never echoed; `p2p_config()`
    redacts it. Prefer the `*_file` references shown above.

## Tune pricing, fees & selection

```sql
CALL p2p_pricing(unit_price => 5, max_bid => 100);                 -- whole TON
CALL p2p_fees(platform_fee_pct => 0.15, participation_commission_frac => 0.05);
CALL p2p_bidding(w_quality => 0.6, w_stake => 0.15, w_price => 0.25);
CALL p2p_selection(replicas => 5, quorum => 3, checksum_min => 3);
```

## Provider staking

A host can bond stake for eligibility/priority on paid work (non-custodial; 7-day
unbond cooldown; slashable):

```sql
CALL p2p_stake(amount => 100);     -- drives the on-chain deposit (or says what's needed)
CALL p2p_unstake(amount => 100);
```

## Run a paid query

```sql
SELECT * FROM p2p_query('SELECT ...', payment => 'paid', replicas => 3, quorum => 2);
```

## Deploying contracts to testnet

The contracts (written in Tolk) plus a full deploy + live end-to-end runbook are
documented in the [TON testnet runbook](../TESTNET.md), including a captured,
real on-chain proof of the split and the staking lifecycle.
