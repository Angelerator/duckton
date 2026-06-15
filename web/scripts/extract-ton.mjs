#!/usr/bin/env node
// Parse the REAL TON deployment artifacts from the repo and merge them into
// web/src/data/snapshot.json under `ton` (alongside the Rust-computed
// `ton.computed`). Everything here is read from committed files:
//   ton/deployments/testnet.env          (live testnet addresses, hashes)
//   ton/deployments/economics.testnet.toml
//   ton/build/*.json                     (compiled code hash + BoC)
//   ton/deployments/logs/*.log           (deploy + verify + e2e output)
//   ton/gas-baseline.json                (measured gas per opcode)
// The per-contract schema (storage / get-methods / guards) is transcribed
// faithfully from ton/contracts/*.tolk.
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const TON = join(ROOT, "ton");
const SNAP = join(ROOT, "web", "src", "data", "snapshot.json");
const read = (p) => (existsSync(p) ? readFileSync(p, "utf8") : "");

// ---- testnet.env ----------------------------------------------------------
const env = {};
for (const m of read(join(TON, "deployments", "testnet.env")).matchAll(/export (\w+)="([^"]*)"/g))
  env[m[1]] = m[2];

// ---- build/*.json code hashes ---------------------------------------------
const buildHash = {};
for (const name of ["StakeVault", "StakeVaultV2", "JobEscrow", "RecordAnchor", "RecordAnchorV2", "GlobalParams", "GlobalParamsV2", "StakeReceiptWallet"]) {
  const p = join(TON, "build", `${name}.json`);
  if (existsSync(p)) {
    const j = JSON.parse(read(p));
    buildHash[name] = {
      codeHash: (j.hash || "").toLowerCase(),
      bocBytes: (j.code_boc64 || "").length,
      codeBoc64: j.code_boc64 || null,
    };
  }
}

// ---- verify logs ----------------------------------------------------------
function parseVerify(name) {
  const log = read(join(TON, "deployments", "logs", `verify_${name}.log`));
  if (!log) return null;
  const codeHash = (log.match(/Code hash:\s*0x([0-9a-fA-F]+)/) || [])[1]?.toLowerCase() ?? null;
  const url = (log.match(/View at:\s*(\S+)/) || [])[1] ?? null;
  const alreadyVerified = /already been verified/.test(log);
  const failed = /Verification failed|error:/.test(log);
  return { codeHash, url, verified: alreadyVerified || (!!url && !failed), alreadyVerified, failed };
}

// ---- deploy logs ----------------------------------------------------------
function parseDeploy(file) {
  const log = read(join(TON, "deployments", "logs", file));
  if (!log) return null;
  const lines = log.split("\n").filter((l) => /Deployed|owner|min stake|verdict|escrow amount|deadline|receipt for|fee recipient|admin/.test(l));
  return lines.map((l) => l.trim());
}

// ---- e2e CHECK markers ----------------------------------------------------
const e2e = [...read(join(TON, "deployments", "logs", "e2e.log")).matchAll(/::CHECK::(.+)/g)].map((m) => m[1].trim());

// ---- gas baseline ---------------------------------------------------------
let gas = [];
try {
  const gb = JSON.parse(read(join(TON, "gas-baseline.json")));
  const o = gb.opcodes || {};
  gas = Object.entries(o).map(([op, v]) => ({
    op,
    minGas: v.min_gas, maxGas: v.max_gas, avgGas: v.avg_gas, samples: v.samples,
  })).sort((a, b) => b.avgGas - a.avgGas);
} catch {}

// ---- economics.testnet.toml -----------------------------------------------
const econToml = read(join(TON, "deployments", "economics.testnet.toml"));

// ---- faithful per-contract schema (transcribed from ton/contracts/*.tolk) -
const opDesc = {
  OP_STAKE_DEPOSIT: "owner bonds stake; mints 1:1 transfer-locked receipt jetton",
  OP_STAKE_UNBOND: "begin cooldown; stake stays slashable during unbonding",
  OP_STAKE_WITHDRAW: "withdraw after cooldown; burns receipts",
  OP_STAKE_SLASH: "slasher-only; graduated slash + split (challenger/redundancy/burn/treasury)",
  OP_STAKE_ANNOUNCE_UPGRADE: "start timelocked SETCODE (records pending code hash + clock)",
  OP_STAKE_APPLY_UPGRADE: "apply SETCODE after timelock; code hash must match announced",
  OP_STAKE_CANCEL_UPGRADE: "cancel a pending upgrade",
  OP_ESCROW_TOPUP: "fund a freshly-deployed escrow",
  OP_ESCROW_SETTLE: "arbiter-only HTLC release keyed on quorum result hash; ≤ escrow B",
  OP_ESCROW_REFUND: "permissionless refund-on-timeout (entire balance → requester)",
  OP_ANCHOR_SUBMIT: "permissionless keeper anchors epoch root (chained, stake-weighted)",
  OP_ANCHOR_UPGRADE_CODE: "verdict-authority SETCODE (no timelock; holds only bonds+chain)",
  OP_UPDATE_PARAMS: "admin-only; bounds-validated on-chain; bumps paramsVersion",
  OP_UPDATE_ADMIN: "admin rotation (single-key → multisig handoff)",
  OP_UPGRADE_CODE: "admin-only in-place SETCODE (no timelock; no user funds)",
};

const schema = {
  StakeVault: {
    role: "Per-node non-custodial bond contract (one vault per operator) and the jetton master of the node's transfer-locked Duckton stake receipt. Deposit, unbonding cooldown, graduated slashing, keeper withdrawal — all governed by code, no human key.",
    doc: "§8 · §8.5 · §8.6",
    upgradeable: "timelocked SETCODE (announce → apply ≥ unbonding window)",
    storage: [
      ["owner", "address", "node operator wallet"],
      ["staked", "coins", "currently bonded"],
      ["unbondingAmount", "coins", "in cooldown (still slashable)"],
      ["unbondingAt", "uint32", "cooldown start ts (0 = none)"],
      ["totalSupply", "coins", "receipts outstanding (== staked + unbonding)"],
      ["bindingHash", "uint256", "wallet↔node identity binding (§3.2)"],
      ["config", "^VaultConfig", "minStake, windows, slasher, slash split"],
      ["upgrade", "^VaultUpgradeState", "codeVersion, pendingCodeHash, pendingAt"],
    ],
    getMethods: [
      ["get_vault_state", "{ staked, unbondingAmount, unbondingAt, totalSupply, eligible }"],
      ["get_pending_upgrade", "{ codeVersion, pendingCodeHash, pendingAt, readyAt }"],
      ["get_code_version", "int (0 = original)"],
      ["get_jetton_data", "TEP-74 master data (Duckton)"],
      ["get_wallet_address(owner)", "deterministic receipt-wallet address"],
      ["is_eligible", "bool"],
    ],
    guards: [
      "slash only by config.slasher; bounded by staked + unbondingAmount",
      "withdraw blocked until now ≥ unbondingAt + unbondingPeriod",
      "slash split must sum to 10000 bps; burn share locked forever",
      "upgrade two-step + timelocked + commit-reveal (hash must match)",
    ],
  },
  StakeReceiptWallet: {
    role: "Per-holder TEP-74-shaped but NON-transferable stake-receipt jetton (Duckton). A slashable accountability bond, not a liquid position — outgoing transfers are hard-locked for the whole bond lifetime so a host can't sell it and dodge slashing.",
    doc: "§8.5",
    upgradeable: "no (minimal surface; holds no TON, no admin)",
    storage: [
      ["status", "uint4", "bit 0 = transfers locked (always set)"],
      ["balance", "coins", "1:1 with bonded TON"],
      ["owner", "address", "node operator"],
      ["master", "address", "the minting StakeVault"],
    ],
    getMethods: [
      ["get_wallet_data", "TEP-74 { balance, owner, minter, code }"],
      ["is_transfer_locked", "bool (always true)"],
      ["get_status", "int"],
    ],
    guards: [
      "JettonTransfer (0x0f8a7ea5) always throws RECEIPT_LOCKED (138)",
      "only master may credit (mint) or burn; balance never negative",
    ],
  },
  JobEscrow: {
    role: "Per-job non-custodial escrow with HTLC-style release keyed on the quorum result hash. Requester locks max bid B at deploy; settle is gated on the agreed hash and bounded by B (remainder refunded); refund-on-timeout returns everything. No platform key can take custody.",
    doc: "§4.2 · §6.2 · §13",
    upgradeable: "no — a live HTLC's guarantees can't be rewritten mid-flight",
    storage: [
      ["requester", "address", "funds B, receives refunds"],
      ["arbiter", "address", "quorum oracle/coordinator authorized to settle"],
      ["escrowAmount", "coins", "B, locked up front"],
      ["deadline", "uint32", "refund-on-timeout (unix secs)"],
      ["settled", "bool", "one-shot"],
      ["terms", "^EscrowTerms", "treasury, expectedHash (HTLC lock), paramsVersion"],
    ],
    getMethods: [
      ["get_escrow_state", "{ escrowAmount, deadline, settled, paramsVersion }"],
      ["get_expected_hash", "int (the HTLC lock = quorum result hash)"],
      ["get_params_version", "int"],
      ["get_requester", "address"],
    ],
    guards: [
      "settle: arbiter-only, once, resultHash == terms.expectedHash",
      "winnerAmount + platformFee + Σparticipants ≤ escrowAmount",
      "refund: permissionless once now ≥ deadline → entire balance to requester",
    ],
  },
  RecordAnchor: {
    role: "Per-epoch Merkle-root anchor + dispute contract. Permissionless keepers anchor a per-epoch root chained to the previous one, gated by a stake-weighted threshold. Anyone can open a bonded dispute with an inclusion proof; an authority resolves it (upheld → bond returned + slash on the vault; rejected → bond forfeited).",
    doc: "§7 · §7.2 · §11",
    upgradeable: "authority-gated SETCODE (no timelock; only bonds + chain)",
    storage: [
      ["currentEpoch", "uint32", "last anchored epoch (0 = none)"],
      ["lastRoot", "uint256", "prev_root for the next epoch (chains history)"],
      ["nextDisputeId", "uint32", "dispute counter"],
      ["disputes", "map<uint32,DisputeInfo>", "open/resolved disputes"],
      ["config", "^AnchorConfig", "minStakeWeight, disputeBondMin, verdictAuthority, treasury"],
      ["codeVersion", "uint32", "monotonic (0 = original)"],
    ],
    getMethods: [
      ["get_anchor_state", "{ currentEpoch, lastRoot, nextDisputeId }"],
      ["verify_inclusion(leaf,root,proof)", "bool"],
      ["compute_root(leaf,proof)", "int"],
      ["get_dispute(id)", "DisputeInfo? "],
      ["get_code_version", "int"],
    ],
    guards: [
      "submit: epoch == currentEpoch+1, prevRoot == lastRoot, stakeWeight ≥ min",
      "dispute requires value ≥ disputeBondMin and a valid inclusion proof",
      "resolve: verdictAuthority-only; upheld → refund bond, rejected → forfeit to treasury",
    ],
  },
  GlobalParams: {
    role: "The single ecosystem-wide config contract (singleton). Holds the economic parameters every node/contract reads. Editable IN PLACE with a stable address (update_params mutates storage, bounds-validated on-chain) so others can hard-pin it; code is upgradable in place too.",
    doc: "§12 · §12.1",
    upgradeable: "admin-gated SETCODE (no timelock; no user funds)",
    storage: [
      ["admin", "address", "single key today; designed for multisig"],
      ["feeRecipient", "address", "platform fee sink"],
      ["params", "^EcoParams", "all bps + stake + windows + selection + weights"],
      ["blocklist", "map<uint256,uint1>", "governance blocklist (additive)"],
      ["paramsVersion", "uint32", "monotonic; bumped on every update"],
      ["codeVersion", "uint32", "monotonic; bumped on every SETCODE"],
    ],
    getMethods: [
      ["get_params", "{ admin, feeRecipient, params, paramsVersion }"],
      ["get_params_version", "int"],
      ["get_code_version", "int"],
      ["get_platform_fee_bps / get_participation_commission_bps / …", "scalar policy reads"],
      ["get_blocklisted(subject)", "bool"],
    ],
    guards: [
      "all mutators admin-only; validateEcoParams enforced on every update",
      "slash split sums to 10000; unbondingSecs ≥ challengeWindowSecs",
      "paramsVersion / codeVersion are contract-controlled (admin can't set directly)",
    ],
  },
};

const ENV_ADDR = {
  StakeVault: env.TON_TESTNET_VAULT_ADDR,
  JobEscrow: env.TON_TESTNET_ESCROW_ADDR,
  RecordAnchor: env.TON_TESTNET_ANCHOR_ADDR,
  GlobalParams: env.TON_TESTNET_GLOBAL_PARAMS_ADDR,
};

// ---- merge into snapshot --------------------------------------------------
const snap = JSON.parse(read(SNAP));
const computedOpcodes = snap.ton?.computed?.opcodes || {};

const contracts = Object.entries(schema).map(([name, s]) => ({
  name,
  ...s,
  testnetAddress: ENV_ADDR[name] || null,
  codeHash: buildHash[name]?.codeHash || null,
  bocBytes: buildHash[name]?.bocBytes || null,
  codeBoc64: buildHash[name]?.codeBoc64 || null,
  verify: parseVerify(name),
  opcodes: (computedOpcodes[name] || []).map((o) => ({ ...o, desc: opDesc[o.name] || "" })),
}));

snap.ton = {
  ...(snap.ton || {}),
  toolchain: "Acton 1.1.0 (Rust TON toolkit · Tolk + @stdlib/@acton)",
  network: env.TON_NETWORK || "testnet",
  rpc: env.TON_TESTNET_RPC || "https://testnet.toncenter.com/api/v2/",
  wallet: env.TON_TESTNET_WALLET || null,
  resultHash: env.TON_TESTNET_RESULT_HASH || null,
  bindingHash: env.TON_TESTNET_BINDING_HASH || null,
  contracts,
  deployments: {
    StakeVault: ENV_ADDR.StakeVault,
    JobEscrow: ENV_ADDR.JobEscrow,
    RecordAnchor: ENV_ADDR.RecordAnchor,
    GlobalParams: ENV_ADDR.GlobalParams,
  },
  deployLogs: {
    stake: parseDeploy("deploy_stake.log"),
    anchor: parseDeploy("deploy_anchor.log"),
    escrow: parseDeploy("deploy_escrow.log"),
    globalParams: parseDeploy("deploy_global_params.log"),
  },
  e2e,
  gas,
  economicsToml: econToml,
  // The canonical "verified live" showcase set + SETCODE proof + Duckton
  // (from docs/TESTNET.md — a separate, pinned demo deployment).
  canonical: {
    source: "docs/TESTNET.md",
    globalParams: "kQAagsi-ThkgbOxxVwXd0CHbSXpZwbLmdfjFAx76PL4bwOna",
    stakeVault: "kQD90f-cm-a4EKkUFV1q7khQgsBbxZpQL6NySZjXAbIVrrkp",
    ducktonHolder: "kQBPNBIv2op4Qd_na03As9YEc8FX3-8ng-RVseTEO9jEXR6r",
    recordAnchor: "kQDhHXS08DLghUFm_ofEI_NWM--4ICUIVv5IV_VQjjH78RK3",
    jobEscrow: "0:edbeca37fcf34549e5ffba5173bc7bbebce53f8089355e621b16e5ddddd4fe23",
    setcode: {
      address: "kQCyxFu9leXR4JqPQ56IcjmlUJMioNUM7f3B8wpyfXdkl7wF",
      from: "GlobalParams (codeVersion 1)",
      to: "GlobalParamsV2 (codeVersion 2)",
      addressStable: true,
      newGetter: "get_surcharge_bps",
      newGetterValue: 500,
      note: "SETCODE swapped code in place — address unchanged, paramsVersion/fee/admin preserved.",
    },
    duckton: { name: "Duckton", symbol: "DUCKTON", decimals: 9, balance: "0.2", transferLocked: true },
  },
  deployFlow: {
    build: ["acton build", "acton test", "acton test --coverage"],
    deploy: [
      "STAKE_BINDING_HASH=0x… acton script scripts/deploy_stake.tolk --net testnet",
      "ANCHOR_BOND_MIN=100000000 acton script scripts/deploy_anchor.tolk --net testnet",
      "ESCROW_AMOUNT=300000000 ESCROW_EXPECTED_HASH=0x… acton script scripts/deploy_escrow.tolk --net testnet",
      "GP_FEE_RECIPIENT=<addr> acton script scripts/deploy_global_params.tolk --net testnet",
    ],
    verify: [
      "acton verify GlobalParams --address <addr> --net testnet",
      "acton verify RecordAnchor --address <addr> --net testnet",
      "acton verify JobEscrow --address <addr> --net testnet",
    ],
    live: "cargo test -p p2p-settlement --features ton-live --test testnet_live -- --nocapture",
  },
};

writeFileSync(SNAP, JSON.stringify(snap, null, 2));
const verified = contracts.filter((c) => c.verify?.verified).length;
console.log(`    merged ton: ${contracts.length} contracts (${verified} verified), ${gas.length} gas opcodes, ${e2e.length} e2e checks`);
