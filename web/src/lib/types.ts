// Domain model — these mirror the JSON produced by the Rust snapshot exporter
// (crates/node/tests/console_export.rs), which runs the REAL duckdb-p2p system.

export type AttestationLevel = "L0" | "L1" | "L2";
export type DataClass = "Public" | "Internal" | "Sensitive";
export type VerifyMode = "Fast" | "Quorum";
export type Verdict =
  | "Correct"
  | "Incorrect"
  | "Timeout"
  | "Malformed"
  | "ResourceExceeded"
  | "Infeasible"
  | "Inconclusive";
export type FaultClass = "provider" | "requester" | "neutral";

export interface NavItem {
  title: string;
  href: string;
  icon: string;
  badge?: string;
  group: string;
  description: string;
}

export interface Worker {
  id: string;
  alias: string;
  attestation: AttestationLevel;
  behavior: "honest" | "cheat" | "fail";
  trust: number;
  soft: number;
  reputation: number | null;
  reputationConfident: number;
  observations: number;
  ageFactor: number;
  voucherTrust: number;
  stakeFactor: number;
  penalty: number;
  explorationBonus: number;
  stakeTon: number;
  stakeNanoton: string;
  /** advertised unit price (whole TON) the host bids on paid jobs. */
  priceTon?: number;
  /** Donated compute budget (bytes) the host OFFERS — NOT the machine's physical
   * RAM (see `systemProfile.physicalRamBytes`). Renamed from `totalMemBytes`. */
  donatedMemBytes: number;
  totalThreads: number;
  maxJobs: number;
  /** Non-GDPR machine-class metadata (analytics/routing HINT only). */
  systemProfile?: SystemProfileView;
  jobsParticipated: number;
  correct: number;
  faults: number;
  successRate: number;
  p50LatencyMs: number;
  delayMs: number;
  online: boolean;
  engineVersion: string;
  wallet: string | null;
}

/** Non-GDPR machine-class metadata for a worker (self-reported HINT only). */
export interface SystemProfileView {
  physicalRamBytes: number;
  ramAvailableBytes: number;
  donatedMemBytes: number;
  donatedThreads: number;
  cpuArch: string;
  cpuModel: string;
  cpuPhysicalCores: number;
  cpuLogicalCores: number;
  cpuFeatures: string[];
  diskKind: string;
  diskTotalBytes: number;
  osName: string;
  osVersion: string;
  kernelVersion: string;
  virtHint: string;
  numaNodes: number;
}

export type CandidateState =
  | "won"
  | "committed"
  | "reset"
  | "dispatched"
  | "bidding"
  | "rejected";

export interface JobCandidate {
  workerId: string;
  alias: string;
  attestation: AttestationLevel;
  state: CandidateState;
  verdict: Verdict;
  etaMs: number;
  price: number;
  progressPct: number;
  committedHash: string | null;
  commitLatencyMs: number;
}

export interface TimelineEvent {
  tMs: number;
  stage: string;
  label: string;
  detail?: string | null;
}

export type JobStatus = "verified" | "settled" | "failed" | "running" | "queued";

export interface ResultPreview {
  columns: string[];
  rows: string[][];
}

export interface Job {
  id: string;
  sql: string;
  fn: "p2p_query" | "p2p_join" | "p2p_share";
  dataClass: DataClass;
  verifyMode: VerifyMode;
  quorum: number;
  k: number;
  status: JobStatus;
  paid: boolean;
  requester: string;
  createdAtMs: number;
  rowCount: number;
  resultHash: string | null;
  latencyMs: number;
  /** escrow cap B for a paid job, in TON (0 for free jobs). */
  escrowTon: number;
  /** actual amount settled out of escrow (paid jobs); 0 for free jobs. */
  settledTon?: number;
  /** the escrow cap B that bounded settlement. */
  escrowCapTon?: number;
  /** refunded to the requester (`escrowCapTon − settledTon`). */
  refundedTon?: number;
  /** computed job cost before the cap was applied. */
  costTon?: number;
  winner: string | null;
  winnerId: string | null;
  source: string;
  candidates: JobCandidate[];
  timeline: TimelineEvent[];
  result: ResultPreview;
  /** Reason a dispatch failed (e.g. no hosts met the data-class policy). Only
   * present on failed jobs from the live grid. */
  error?: string;
}

export interface Receipt {
  jobId: string;
  workerId: string;
  workerAlias: string;
  requesterId: string;
  verdict: Verdict;
  fault: FaultClass;
  latencyMs: number;
  /** Per-job MEASURED magnitude. `observedInputBytes` is the estimator's
   * scanned-bytes estimate; `0` = unknown. */
  observedInputBytes?: number;
  observedResultRows?: number;
  observedResultBytes?: number;
  tsMs: number;
  resultHash: string;
  sig: string;
  verified: boolean;
  gossiped: boolean;
}

export interface StakeRow {
  alias: string;
  workerId: string;
  wallet: string;
  stakeTon: number;
  stakeFactor: number;
  eligiblePublic: boolean;
  eligibleInternal: boolean;
  eligibleSensitive: boolean;
}

// ---- TON on-chain layer ---------------------------------------------------

export interface TonOpcode {
  name: string;
  hex: string;
  value: number;
  desc?: string;
}

export interface TonContract {
  name: string;
  role: string;
  doc: string;
  upgradeable: string;
  storage: [string, string, string][];
  getMethods: [string, string][];
  guards: string[];
  testnetAddress: string | null;
  codeHash: string | null;
  bocBytes: number | null;
  codeBoc64: string | null;
  verify: {
    codeHash: string | null;
    url: string | null;
    verified: boolean;
    alreadyVerified: boolean;
    failed: boolean;
  } | null;
  opcodes: TonOpcode[];
}

export interface TonSection {
  toolchain: string;
  network: string;
  rpc: string;
  wallet: string | null;
  resultHash: string | null;
  bindingHash: string | null;
  contracts: TonContract[];
  deployments: Record<string, string | null>;
  deployLogs: Record<string, string[] | null>;
  e2e: string[];
  gas: { op: string; minGas: number; maxGas: number; avgGas: number; samples: number }[];
  economicsToml: string;
  canonical: {
    source: string;
    globalParams: string;
    stakeVault: string;
    ducktonHolder: string;
    recordAnchor: string;
    jobEscrow: string;
    setcode: {
      address: string;
      from: string;
      to: string;
      addressStable: boolean;
      newGetter: string;
      newGetterValue: number;
      note: string;
    };
    duckton: { name: string; symbol: string; decimals: number; balance: string; transferLocked: boolean };
  };
  deployFlow: { build: string[]; deploy: string[]; verify: string[]; live: string };
  computed: {
    globalParams: Record<string, number | string>;
    opcodes: Record<string, TonOpcode[]>;
    escrow: {
      address: string;
      codeHash: string;
      termsCellHash: string;
      expectedHashHex: string;
      paramsVersion: number;
      escrowTon: number;
      deterministic: string;
    } | null;
    messages: {
      label: string;
      opcodeHex: string;
      opcode: number;
      cellHash: string;
      bocBase64: string;
      bits: number;
    }[];
  };
}

// ---- The full snapshot shape (cast target for the imported JSON) ----------

export interface Snapshot {
  meta: {
    generatedAtMs: number;
    generatedNote: string;
    protocolVersion: string;
    minSupported: string;
    wireSchemaVersion: number;
    engineVersion: string;
    transport: string;
    tls: string;
    workspaceVersion: string;
    buildMs: number;
    jobsRun: number;
  };
  overview: {
    workersOnline: number;
    workersTotal: number;
    jobsRun: number;
    verified: number;
    failed: number;
    avgTrust: number;
    totalStakeTon: number;
    freeMemBytes: number;
    series: { label: string; latencyMs: number; verified: number }[];
    latencyHistogram: { bucket: string; count: number }[];
    attestationMix: { level: string; count: number; fill: string }[];
  };
  workers: Worker[];
  jobs: Job[];
  paidJobs: Job[];
  receipts: Receipt[];
  trust: {
    formula: string;
    weights: { alpha: number; beta: number; gamma: number; delta: number };
    minTrust: number;
    bootstrapTrust: number;
    halfLifeSecs: number;
    canonical: {
      columns: string[];
      rows: string[][];
      hash: string;
      reorderedHash: string;
      orderIndependent: boolean;
    };
    quorum: {
      hashes: string[];
      quorum: number;
      agreement: number;
      agreedHash: string | null;
      reached: boolean;
    };
  };
  settlement: {
    enabled: boolean;
    network: string;
    fees: {
      platformFeePct: number;
      participationCommissionFrac: number;
      bonusAggressiveness: number;
      lambdaQuality: number;
      lambdaSpeed: number;
      verificationSurchargePct: number;
    };
    stake: {
      minStake: number;
      minStakeInternal: number;
      minStakeSensitive: number;
      stakeCap: number;
      unbondingSecs: number;
      receiptJetton: boolean;
      receiptTransferLocked: boolean;
    };
    slashing: {
      wrongResultPct: number;
      cheatPct: number;
      downtimePct: number;
      equivocationPct: number;
      failedCommitmentPct: number;
      challengeWindowSecs: number;
      toChallenger: number;
      toRedundancy: number;
      toBurn: number;
      toTreasury: number;
    };
    escrowMaxBidTon: number;
    events: { type: string; job: string; maxBidTon?: number; totalTon?: number }[];
    splits: {
      winnerTon: number;
      platformFeeTon: number;
      participants: { wallet: string; amountTon: number }[];
      totalTon: number;
      resultHashHex: string;
    }[];
    epochRootHex: string;
    anchorRecords: number;
    inclusionProof: {
      leafHex: string;
      siblings: { right: boolean; hashHex: string }[];
      verified: boolean;
    } | null;
    stakeTable: StakeRow[];
    stakeCurve: { stakeTon: number; factor: number }[];
    quality: {
      sample: { successRatio: number; latencyMs: number; bytesVerified: number };
      throughputScore: number;
      qualityScore: number;
    };
    binding: {
      nodeId: string;
      walletAddress: string;
      nonceHex: string;
      expiry: number;
      sigNodeHex: string;
      tonProof: {
        domain: string;
        timestamp: number;
        payloadHex: string;
        signatureHex: string;
      };
      verified: boolean;
    };
  };
  transport: {
    compression: { algorithm: string; level: number; min_size_bytes: number };
    quic: {
      congestion: string;
      gso: boolean;
      gro: boolean;
      pacing: boolean;
      enable_0rtt: boolean;
      max_concurrent_uni_streams: number;
      send_window_bytes: number;
      session_ticket_lifetime_secs: number;
      stream_receive_window_bytes: number | null;
      connection_receive_window_bytes: number | null;
      bdp: { enabled: boolean; bandwidth_mbps: number; rtt_ms: number };
    };
    result: { parallelism: number; parallel_min_bytes: number; chunk_bytes: number | null };
    bench: {
      rows: number;
      sweep: { parallelism: number; rowsPerSec: number; mbPerSec: number; p50Ms: number }[];
      best: { parallelism: number; rowsPerSec: number; mbPerSec: number; p50Ms: number };
      command: string;
      envKnobs: string[];
    };
  };
  protocol: {
    wire: { variant: string; direction: string; purpose: string }[];
    verdicts: { verdict: string; fault: string }[];
    handshake: {
      wireSchemaVersion: number;
      version: string;
      minSupported: string;
      requireMatchingEngineVersion: boolean;
    };
    messages: Record<string, unknown>;
  };
  config: {
    value: Record<string, unknown>;
    exampleToml: string;
  };
  ton: TonSection;
}
