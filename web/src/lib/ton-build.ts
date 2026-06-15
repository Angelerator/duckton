// Build real on-chain deploy artifacts in the browser with @ton/core.
//
// Cell layouts mirror the working-tree Tolk contracts in ton/contracts/* (field
// order, bit widths, refs). The EcoParams encoding is VERIFIED at runtime against
// the hash the Rust settlement crate computed (ecoEncoderOk). Empty HashmapE
// (map) is a single 0 bit, encoded with storeBit(false).
import {
  Address,
  beginCell,
  Cell,
  contractAddress,
  storeStateInit,
  type StateInit,
} from "@ton/core";
import { ton } from "@/lib/data";

const NANO = 1_000_000_000n;
const tonToNano = (t: number) => BigInt(Math.round(t)) * NANO;

/* ------------------------------------------------------------- code cells */

export function codeCellOf(name: string): Cell | null {
  const c = ton.contracts.find((x) => x.name === name);
  if (!c?.codeBoc64) return null;
  try {
    return Cell.fromBase64(c.codeBoc64);
  } catch {
    return null;
  }
}

/** Verify the artifact code BoC reproduces its recorded code hash. */
export function codeHashOk(name: string): boolean {
  const c = ton.contracts.find((x) => x.name === name);
  const cell = codeCellOf(name);
  if (!c?.codeHash || !cell) return false;
  return cell.hash().toString("hex").toLowerCase() === c.codeHash.toLowerCase();
}

/* ------------------------------------------------------- GlobalParams config */

export interface GpConfig {
  platformFeeBps: number;
  surchargeBps: number;
  participationCommissionBps: number;
  slashWrongBps: number;
  slashCheatBps: number;
  slashDowntimeBps: number;
  slashEquivocationBps: number;
  splitChallengerBps: number;
  splitRedundancyBps: number;
  splitBurnBps: number;
  splitTreasuryBps: number;
  minStakeTon: number;
  minStakeInternalTon: number;
  minStakeSensitiveTon: number;
  stakeCapTon: number;
  unbondingSecs: number;
  challengeWindowSecs: number;
  nPublic: number;
  nDefault: number;
  nMax: number;
  quorum: number;
  checksumMin: number;
  wQualityBps: number;
  wStakeBps: number;
  wPriceBps: number;
  slashFailedCommitmentBps: number;
  attemptDeadlineMs: number;
  progressIntervalMs: number;
  progressStallMult: number;
}

export const GP_DEFAULT: GpConfig = (() => {
  const g = ton.computed.globalParams;
  const n = (k: string) => Number(g[k as keyof typeof g] ?? 0);
  return {
    platformFeeBps: n("platformFeeBps"), surchargeBps: n("surchargeBps"),
    participationCommissionBps: n("participationCommissionBps"),
    slashWrongBps: n("slashWrongBps"), slashCheatBps: n("slashCheatBps"),
    slashDowntimeBps: n("slashDowntimeBps"), slashEquivocationBps: n("slashEquivocationBps"),
    splitChallengerBps: n("splitChallengerBps"), splitRedundancyBps: n("splitRedundancyBps"),
    splitBurnBps: n("splitBurnBps"), splitTreasuryBps: n("splitTreasuryBps"),
    minStakeTon: n("minStakeTon"), minStakeInternalTon: n("minStakeInternalTon"),
    minStakeSensitiveTon: n("minStakeSensitiveTon"), stakeCapTon: n("stakeCapTon"),
    unbondingSecs: n("unbondingSecs"), challengeWindowSecs: n("challengeWindowSecs"),
    nPublic: n("nPublic"), nDefault: n("nDefault"), nMax: n("nMax"),
    quorum: n("quorum"), checksumMin: n("checksumMin"),
    wQualityBps: n("wQualityBps"), wStakeBps: n("wStakeBps"), wPriceBps: n("wPriceBps"),
    slashFailedCommitmentBps: n("slashFailedCommitmentBps"),
    attemptDeadlineMs: n("attemptDeadlineMs"), progressIntervalMs: n("progressIntervalMs"),
    progressStallMult: n("progressStallMult"),
  };
})();

/** EcoParams child cell — exact order/width of global_params_types.tolk::EcoParams. */
export function buildEcoParamsCell(c: GpConfig): Cell {
  return beginCell()
    .storeUint(c.platformFeeBps, 16)
    .storeUint(c.surchargeBps, 16)
    .storeUint(c.participationCommissionBps, 16)
    .storeUint(c.slashWrongBps, 16)
    .storeUint(c.slashCheatBps, 16)
    .storeUint(c.slashDowntimeBps, 16)
    .storeUint(c.slashEquivocationBps, 16)
    .storeUint(c.splitChallengerBps, 16)
    .storeUint(c.splitRedundancyBps, 16)
    .storeUint(c.splitBurnBps, 16)
    .storeUint(c.splitTreasuryBps, 16)
    .storeCoins(tonToNano(c.minStakeTon))
    .storeCoins(tonToNano(c.minStakeInternalTon))
    .storeCoins(tonToNano(c.minStakeSensitiveTon))
    .storeCoins(tonToNano(c.stakeCapTon))
    .storeUint(c.unbondingSecs, 32)
    .storeUint(c.challengeWindowSecs, 32)
    .storeUint(c.nPublic, 8)
    .storeUint(c.nDefault, 8)
    .storeUint(c.nMax, 8)
    .storeUint(c.quorum, 8)
    .storeUint(c.checksumMin, 8)
    .storeUint(c.wQualityBps, 16)
    .storeUint(c.wStakeBps, 16)
    .storeUint(c.wPriceBps, 16)
    .storeUint(c.slashFailedCommitmentBps, 16)
    .storeUint(c.attemptDeadlineMs, 32)
    .storeUint(c.progressIntervalMs, 32)
    .storeUint(c.progressStallMult, 8)
    .endCell();
}

/** True when our JS EcoParams encoding reproduces the Rust-computed reference hash. */
export function ecoEncoderOk(): boolean {
  try {
    return (
      buildEcoParamsCell(GP_DEFAULT).hash().toString("hex") ===
      String(ton.computed.globalParams.ecoParamsCellHash)
    );
  } catch {
    return false;
  }
}

export function validateGp(c: GpConfig): string[] {
  const e: string[] = [];
  const splits = c.splitChallengerBps + c.splitRedundancyBps + c.splitBurnBps + c.splitTreasuryBps;
  if (splits !== 10000) e.push(`slash split must sum to 10000 bps (got ${splits})`);
  if (c.participationCommissionBps > 1000) e.push("participation κ must be ≤ 1000 bps (10%)");
  for (const [k, v] of Object.entries(c))
    if (k.endsWith("Bps") && (v as number) > 10000) e.push(`${k} must be ≤ 10000 bps`);
  if (c.unbondingSecs < c.challengeWindowSecs) e.push("unbonding must be ≥ challenge window");
  if (!(c.minStakeSensitiveTon >= c.minStakeInternalTon && c.minStakeInternalTon >= c.minStakeTon))
    e.push("min stake tiers must be ordered (sensitive ≥ internal ≥ public)");
  if (c.stakeCapTon < c.minStakeTon) e.push("stake cap must be ≥ min stake");
  if (!(c.checksumMin >= 1 && c.nPublic >= c.checksumMin && c.nDefault >= c.nPublic && c.nMax >= c.nDefault))
    e.push("selection sizes: nMax ≥ nDefault ≥ nPublic ≥ checksumMin ≥ 1");
  if (!(c.quorum >= 1 && c.quorum <= c.nDefault)) e.push("quorum must be in 1..nDefault");
  if (c.progressIntervalMs < 1 || c.progressStallMult < 1) e.push("progress interval/mult must be ≥ 1");
  return e;
}

/* ---------------------------------------------------------------- deploy out */

export interface DeployArtifact {
  ok: boolean;
  error?: string;
  address?: string; // friendly, network-flavored
  raw?: string; // 0:hex
  stateInitBoc?: string; // base64 for TON Connect
  detail?: Record<string, string>;
}

function finish(code: Cell, data: Cell, testnet: boolean, detail?: Record<string, string>): DeployArtifact {
  const init: StateInit = { code, data };
  const addr = contractAddress(0, init);
  const stateInitBoc = beginCell().store(storeStateInit(init)).endCell().toBoc().toString("base64");
  return {
    ok: true,
    address: addr.toString({ testOnly: testnet, bounceable: false, urlSafe: true }),
    raw: addr.toRawString(),
    stateInitBoc,
    detail,
  };
}

const addr = (s: string) => Address.parse(s.trim());
const u256 = (hex: string) => {
  const h = (hex || "0").trim().replace(/^0x/i, "");
  return h ? BigInt("0x" + h) : 0n;
};

/* GlobalParamsStorage: admin, feeRecipient, ^EcoParams, blocklist(0), paramsVersion=1,
   codeVersion=0, upgradeDelay, pendingCodeHash=0, pendingCodeAt=0  (working-tree struct). */
export function buildGlobalParamsDeploy(
  cfg: GpConfig,
  adminRaw: string,
  feeRecipientRaw: string,
  upgradeDelaySecs: number,
  testnet: boolean
): DeployArtifact {
  try {
    const code = codeCellOf("GlobalParams");
    if (!code) return { ok: false, error: "GlobalParams code BoC missing" };
    const eco = buildEcoParamsCell(cfg);
    const data = beginCell()
      .storeAddress(addr(adminRaw))
      .storeAddress(addr(feeRecipientRaw))
      .storeRef(eco)
      .storeBit(false) // empty blocklist HashmapE
      .storeUint(1, 32) // paramsVersion
      .storeUint(0, 32) // codeVersion
      .storeUint(Math.max(1, Math.round(upgradeDelaySecs)), 32) // upgradeDelay (> 0)
      .storeUint(0, 256) // pendingCodeHash
      .storeUint(0, 32) // pendingCodeAt
      .endCell();
    return finish(code, data, testnet, { ecoCellHash: eco.hash().toString("hex") });
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/* JobEscrow: requester, arbiter, escrowAmount, deadline, settled=0,
   ^EscrowTerms{treasury, expectedHash, candidatesHash, paramsVersion}, pending(0). */
export function buildJobEscrowDeploy(p: {
  requester: string;
  arbiter: string;
  treasury: string;
  escrowAmountTon: number;
  deadlineUnix: number;
  expectedHashHex: string;
  candidatesHashHex?: string;
  paramsVersion: number;
  testnet: boolean;
}): DeployArtifact {
  try {
    const code = codeCellOf("JobEscrow");
    if (!code) return { ok: false, error: "JobEscrow code BoC missing" };
    const terms = beginCell()
      .storeAddress(addr(p.treasury))
      .storeUint(u256(p.expectedHashHex), 256)
      .storeUint(u256(p.candidatesHashHex ?? "0"), 256)
      .storeUint(p.paramsVersion, 32)
      .endCell();
    const data = beginCell()
      .storeAddress(addr(p.requester))
      .storeAddress(addr(p.arbiter))
      .storeCoins(tonToNano(p.escrowAmountTon))
      .storeUint(Math.round(p.deadlineUnix), 32)
      .storeBit(false) // settled
      .storeRef(terms)
      .storeBit(false) // empty pending
      .endCell();
    return finish(code, data, p.testnet);
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/* StakeVault: owner, staked=0, unbonding=0, unbondingAt=0, totalSupply=0, bindingHash,
   ^VaultConfig, ^VaultUpgradeState(fresh), pending(0). */
export function buildStakeVaultDeploy(p: {
  owner: string;
  slasher: string;
  upgradeAuthority: string;
  treasury: string;
  redundancyPool: string;
  minStakeTon: number;
  unbondingSecs: number;
  challengeWindowSecs: number;
  keeperGraceSecs: number;
  keeperBountyBps: number;
  splitChallengerBps: number;
  splitRedundancyBps: number;
  splitBurnBps: number;
  splitTreasuryBps: number;
  bindingHashHex?: string;
  testnet: boolean;
}): DeployArtifact {
  try {
    const code = codeCellOf("StakeVault");
    const receiptCode = codeCellOf("StakeReceiptWallet");
    if (!code || !receiptCode) return { ok: false, error: "StakeVault/receipt code BoC missing" };
    const slash = beginCell()
      .storeAddress(addr(p.treasury))
      .storeAddress(addr(p.redundancyPool))
      .storeUint(p.splitChallengerBps, 16)
      .storeUint(p.splitRedundancyBps, 16)
      .storeUint(p.splitBurnBps, 16)
      .storeUint(p.splitTreasuryBps, 16)
      .endCell();
    const config = beginCell()
      .storeCoins(tonToNano(p.minStakeTon))
      .storeUint(p.unbondingSecs, 32)
      .storeUint(p.challengeWindowSecs, 32)
      .storeUint(p.keeperGraceSecs, 32)
      .storeUint(p.keeperBountyBps, 16)
      .storeAddress(addr(p.slasher))
      .storeAddress(addr(p.upgradeAuthority))
      .storeRef(receiptCode)
      .storeRef(slash)
      .endCell();
    const upgrade = beginCell().storeUint(0, 32).storeUint(0, 256).storeUint(0, 32).endCell();
    const data = beginCell()
      .storeAddress(addr(p.owner))
      .storeCoins(0n) // staked
      .storeCoins(0n) // unbondingAmount
      .storeUint(0, 32) // unbondingAt
      .storeCoins(0n) // totalSupply
      .storeUint(u256(p.bindingHashHex ?? "0"), 256)
      .storeRef(config)
      .storeRef(upgrade)
      .storeBit(false) // empty pending
      .endCell();
    return finish(code, data, p.testnet);
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/* RecordAnchor: currentEpoch=0, lastRoot=0, nextDisputeId=0, disputes(0), roots(0),
   ^AnchorConfig{minStakeWeight, disputeBondMin, verdictAuthority, ^Authorities}, codeVersion=0. */
export function buildRecordAnchorDeploy(p: {
  verdictAuthority: string;
  treasury: string;
  keeper: string;
  upgradeAuthority: string;
  minStakeWeightTon: number;
  disputeBondMinTon: number;
  testnet: boolean;
}): DeployArtifact {
  try {
    const code = codeCellOf("RecordAnchor");
    if (!code) return { ok: false, error: "RecordAnchor code BoC missing" };
    const authorities = beginCell()
      .storeAddress(addr(p.treasury))
      .storeAddress(addr(p.keeper))
      .storeAddress(addr(p.upgradeAuthority))
      .endCell();
    const config = beginCell()
      .storeCoins(tonToNano(p.minStakeWeightTon))
      .storeCoins(tonToNano(p.disputeBondMinTon))
      .storeAddress(addr(p.verdictAuthority))
      .storeRef(authorities)
      .endCell();
    const data = beginCell()
      .storeUint(0, 32) // currentEpoch
      .storeUint(0, 256) // lastRoot
      .storeUint(0, 32) // nextDisputeId
      .storeBit(false) // disputes
      .storeBit(false) // roots
      .storeRef(config)
      .storeUint(0, 32) // codeVersion
      .endCell();
    return finish(code, data, p.testnet);
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Export the editable config as the `[economics]`-style TOML the node reads. */
export function gpToToml(c: GpConfig, network: "testnet" | "mainnet"): string {
  const f = (bps: number) => (bps / 10000).toString();
  return [
    "[economics]",
    "enabled         = true",
    'settlement      = "onchain"',
    `network         = "${network}"`,
    'default_payment = "paid"',
    "",
    "[economics.fees]",
    `platform_fee                  = ${f(c.platformFeeBps)}`,
    `verification_surcharge        = ${f(c.surchargeBps)}`,
    `participation_commission_frac = ${f(c.participationCommissionBps)}`,
    "",
    "[economics.stake]",
    `min_stake           = ${c.minStakeTon}`,
    `min_stake_internal  = ${c.minStakeInternalTon}`,
    `min_stake_sensitive = ${c.minStakeSensitiveTon}`,
    `stake_cap           = ${c.stakeCapTon}`,
    `unbonding_secs      = ${c.unbondingSecs}`,
    "",
    "[economics.slashing]",
    `slash_wrong_result_pct = ${f(c.slashWrongBps)}`,
    `slash_cheat_pct        = ${f(c.slashCheatBps)}`,
    `slash_downtime_pct     = ${f(c.slashDowntimeBps)}`,
    `slash_equivocation_pct = ${f(c.slashEquivocationBps)}`,
    `slash_to_challenger    = ${f(c.splitChallengerBps)}`,
    `slash_to_redundancy    = ${f(c.splitRedundancyBps)}`,
    `slash_to_burn          = ${f(c.splitBurnBps)}`,
    `slash_to_treasury      = ${f(c.splitTreasuryBps)}`,
    "",
    "[economics.selection]",
    `n_public  = ${c.nPublic}`,
    `n_default = ${c.nDefault}`,
    `n_max     = ${c.nMax}`,
    "",
    "[economics.ranking]",
    `w_quality = ${f(c.wQualityBps)}`,
    `w_stake   = ${f(c.wStakeBps)}`,
    `w_price   = ${f(c.wPriceBps)}`,
  ].join("\n");
}
