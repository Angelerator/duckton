// Real data layer. Everything here is read from `snapshot.json`, which is
// produced by the Rust snapshot exporter running the actual duckdb-p2p system
// (real loopback-QUIC grid, real trust engine, real settlement, real config).
// Regenerate with: web/scripts/generate-data.sh
import raw from "@/data/snapshot.json";
import type { Job, Snapshot, Worker } from "./types";

export const snapshot = raw as unknown as Snapshot;

export const meta = snapshot.meta;
export const overview = snapshot.overview;
export const workers: Worker[] = snapshot.workers;
export const receipts = snapshot.receipts;
export const trust = snapshot.trust;
export const settlement = snapshot.settlement;
export const transport = snapshot.transport;
export const protocol = snapshot.protocol;
export const config = snapshot.config;
export const ton = snapshot.ton;

/** Free + paid jobs, newest first by creation time. */
export const jobs: Job[] = [...snapshot.jobs, ...snapshot.paidJobs].sort(
  (a, b) => b.createdAtMs - a.createdAtMs
);

export const workerById = (id: string) => workers.find((w) => w.id === id);
export const jobById = (id: string) => jobs.find((j) => j.id === id);
export const aliasOf = (id: string | null) =>
  id ? (workers.find((w) => w.id === id)?.alias ?? id) : "—";

/** Overview chart helpers (real measurements). */
export const series = overview.series;
export const latencyHistogram = overview.latencyHistogram;
export const attestationMix = overview.attestationMix;

/** Workers the trust engine flagged via real provider-fault verdicts. */
export const flagged = workers.filter((w) => w.faults > 0 || w.behavior !== "honest");

/** Real stake accounts (from the on-grid stake registry). */
export const stakeAccounts = settlement.stakeTable;

/** The single real epoch we anchored during the run. */
export const epochs = [
  {
    epoch: 1,
    merkleRoot: settlement.epochRootHex,
    jobs: settlement.anchorRecords,
    tsMs: meta.generatedAtMs,
    stakeWeight: settlement.stakeTable.reduce((a, s) => a + s.stakeFactor, 0) /
      Math.max(1, settlement.stakeTable.length),
    disputed: false,
    inclusionVerified: settlement.inclusionProof?.verified ?? false,
  },
];

/** Real swarm nodes for the network view (no geography on a single host). */
export const nodes = workers.map((w) => ({
  id: w.id,
  alias: w.alias,
  attestation: w.attestation,
  online: w.online,
  freeMemBytes: w.donatedMemBytes,
  freeThreads: w.totalThreads,
  maxJobs: w.maxJobs,
  trust: w.trust,
  behavior: w.behavior,
  price: 0,
  via: "loopback" as const,
}));

// ---- Node communication graph (real: derived from receipts + quorum) ------

export type CommGroup = "worker" | "cheat" | "fail" | "requester";
export interface CommNode {
  id: string;
  label: string;
  group: CommGroup;
  degree: number;
  trust: number;
}
export interface CommEdge {
  source: string;
  target: string;
  weight: number;
  kind: "dispatch" | "quorum";
}

function buildCommGraph(): { nodes: CommNode[]; edges: CommEdge[] } {
  const nodes = new Map<string, CommNode>();
  const ensure = (id: string, label: string, group: CommGroup, trust: number) => {
    if (!nodes.has(id)) nodes.set(id, { id, label, group, degree: 0, trust });
  };
  for (const w of workers) {
    ensure(w.id, w.alias, w.behavior === "honest" ? "worker" : w.behavior, w.trust);
  }

  const edges = new Map<string, CommEdge>();
  const bump = (source: string, target: string, kind: CommEdge["kind"]) => {
    const key = kind === "quorum" && source > target
      ? `${target}|${source}|${kind}`
      : `${source}|${target}|${kind}`;
    const e = edges.get(key);
    if (e) e.weight += 1;
    else edges.set(key, { source, target, weight: 1, kind });
  };

  // Dispatch edges: requester → worker, one per signed receipt.
  let reqIdx = 0;
  const reqLabel = new Map<string, string>();
  for (const r of receipts) {
    if (!reqLabel.has(r.requesterId)) reqLabel.set(r.requesterId, `requester-${++reqIdx}`);
    ensure(r.requesterId, reqLabel.get(r.requesterId)!, "requester", 1);
    bump(r.requesterId, r.workerId, "dispatch");
  }

  // Quorum edges: workers that returned the agreed (Correct) hash on the same job.
  for (const j of jobs) {
    const agree = j.candidates.filter((c) => c.verdict === "Correct").map((c) => c.workerId);
    for (let a = 0; a < agree.length; a++)
      for (let b = a + 1; b < agree.length; b++) bump(agree[a], agree[b], "quorum");
  }

  for (const e of edges.values()) {
    if (nodes.has(e.source)) nodes.get(e.source)!.degree += e.weight;
    if (nodes.has(e.target)) nodes.get(e.target)!.degree += e.weight;
  }
  return { nodes: [...nodes.values()], edges: [...edges.values()] };
}

export const commGraph = buildCommGraph();

// ---- Earnings model (the REAL off-chain split the coordinator computes) ----

export interface EarningTerm {
  key: string;
  label: string;
  value: number;
  note: string;
  kind?: "pos" | "neg" | "out";
}

const split0 = settlement.splits[0];

export const earningExample = {
  /** escrowed max bid B (whole TON) */
  B: settlement.escrowMaxBidTon,
  /** platform fee fraction φ */
  phi: settlement.fees.platformFeePct,
  /** per-verifier participation commission fraction κ */
  kappa: settlement.fees.participationCommissionFrac,
  /** number of agreeing non-winners that each receive κ·B */
  participants: split0?.participants.length ?? 0,
  /** on-chain perf-split design params (drive the JobEscrow bonus on TON) */
  rho: settlement.fees.bonusAggressiveness,
  lambdaQ: settlement.fees.lambdaQuality,
  lambdaS: settlement.fees.lambdaSpeed,
  /** the actual amounts the run settled */
  winnerTon: split0?.winnerTon ?? 0,
  platformFeeTon: split0?.platformFeeTon ?? 0,
  commissionEachTon: split0?.participants[0]?.amountTon ?? 0,
  totalTon: split0?.totalTon ?? 0,
};

/**
 * Reproduces the REAL off-chain settlement split:
 *   fee            = φ · B
 *   commissionEach = κ · B  (to each agreeing non-winner)
 *   winner         = B − fee − commissionEach · participants
 * (All bonuses are bounded by the escrow B; nothing exceeds it.)
 */
export function computeEarning(e: typeof earningExample): EarningTerm[] {
  const fee = e.phi * e.B;
  const commissionEach = e.kappa * e.B;
  const totalCommission = commissionEach * e.participants;
  const winner = e.B - fee - totalCommission;
  return [
    { key: "escrow", label: "escrow B (max bid)", value: e.B, note: "requester locks this up front", kind: "pos" },
    { key: "fee", label: "platform fee  φ·B", value: -fee, note: `φ = ${(e.phi * 100).toFixed(1)}%`, kind: "neg" },
    { key: "commission", label: `participation κ·B × ${e.participants}`, value: -totalCommission, note: `κ = ${(e.kappa * 100).toFixed(1)}% to each agreeing verifier`, kind: "neg" },
    { key: "winner", label: "winner payout", value: winner, note: "base + perf bonus = escrow remainder", kind: "out" },
    { key: "commissionEach", label: "commission per verifier", value: commissionEach, note: "flat, contract-fixed cut", kind: "out" },
  ];
}
