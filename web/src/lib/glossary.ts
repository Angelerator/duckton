// Plain-language glossary of the terms used across the console. Each entry says
// what the thing IS in simple words and how it IMPACTS the system. Faithful to
// the project docs, but written for newcomers.

export type GlossaryGroup =
  | "Core"
  | "Trust"
  | "Transport"
  | "Storage"
  | "Network"
  | "Economics";

export interface Term {
  term: string;
  what: string;
  impact: string;
  group: GlossaryGroup;
}

export const GLOSSARY: Term[] = [
  // ---- Core ----
  {
    term: "Grid",
    what: "Many ordinary machines that each run DuckDB and donate a slice of their RAM/CPU to run queries for others.",
    impact: "You get database compute from a shared pool instead of paying for one big server.",
    group: "Core",
  },
  {
    term: "Requester",
    what: "The node that wants a query run. It broadcasts the job and picks which workers to use.",
    impact: "Anyone can ask the grid to compute something; no central service sits in the middle.",
    group: "Core",
  },
  {
    term: "Worker (host)",
    what: "A machine that accepts a job and actually runs the SQL.",
    impact: "More online workers = more capacity and more redundancy for cross-checking results.",
    group: "Core",
  },
  {
    term: "Job",
    what: "One query execution. The same job is usually sent to several workers at once.",
    impact: "Running it in several places is what makes a result both fast and verifiable.",
    group: "Core",
  },
  {
    term: "Hedged execution",
    what: "Sending the same job to k workers in a race instead of trusting one. The first acceptable answer wins; the slower copies are cancelled (RESET).",
    impact: "Cuts tail latency (a slow/dead worker can't stall you) and provides copies to compare.",
    group: "Core",
  },
  {
    term: "Quorum",
    what: "The number of workers whose results must match before an answer is accepted (e.g. 3 of 5).",
    impact: "A wrong answer from one machine can't win — enough independent machines must agree.",
    group: "Core",
  },
  {
    term: "Verify mode",
    what: "Fast = return the first result and check agreement in the background; Quorum = wait for enough matching results before returning.",
    impact: "Lets you trade a little latency for a stronger correctness guarantee per query.",
    group: "Core",
  },
  {
    term: "Data class",
    what: "How sensitive the data is: Public, Internal, or Sensitive.",
    impact: "Higher sensitivity automatically routes work to more-trusted, hardware-attested machines.",
    group: "Core",
  },

  // ---- Trust ----
  {
    term: "Trust score",
    what: "A 0–1 number a requester computes for each worker from its history, vouches, stake, and hardware tier.",
    impact: "Requesters prefer high-trust workers and skip low-trust ones — so good behavior is rewarded with more work.",
    group: "Trust",
  },
  {
    term: "Reputation (R)",
    what: "A recency-weighted record of how often a worker returned correct results, built from signed receipts.",
    impact: "Cheating or failing drops R fast; the worker then gets selected less and earns less.",
    group: "Trust",
  },
  {
    term: "Attestation tier (L0/L1/L2)",
    what: "Hardware-trust level. L0 = anonymous laptop, L1 = verified boot (TPM), L2 = confidential enclave (TEE) whose memory even the owner can't read.",
    impact: "Sensitive data is only sent to L2 hardware; the tier is a hard gate, not just a bonus.",
    group: "Trust",
  },
  {
    term: "Receipt",
    what: "A small signed statement about a finished job's outcome (correct / wrong / timeout).",
    impact: "Receipts are the portable, tamper-evident history that reputation is built from.",
    group: "Trust",
  },
  {
    term: "Canonical hash",
    what: "A stable BLAKE3 fingerprint of a result, computed so row order and number formatting don't change it.",
    impact: "Lets two machines prove they got the identical answer by comparing one short string.",
    group: "Trust",
  },
  {
    term: "Commit-first",
    what: "Workers send the fingerprint (hash) of their result before sending the full data.",
    impact: "Stops a worker from copying others' answers — it must commit to its own result up front.",
    group: "Trust",
  },
  {
    term: "Canary audit",
    what: "The requester secretly slips in a query whose answer it already knows.",
    impact: "Catches cheaters even on non-redundant jobs; a worker that fails is marked wrong and slashed.",
    group: "Trust",
  },
  {
    term: "Sybil resistance",
    what: "Making it costly to spin up many fake identities (via proof-of-work, vouches, and stake).",
    impact: "Stops an attacker from flooding the grid with fake workers to outvote honest ones.",
    group: "Trust",
  },

  // ---- Network ----
  {
    term: "DHT (Kademlia)",
    what: "A distributed phone book: nodes find each other by ID with no central directory.",
    impact: "The grid scales to many peers and keeps working as machines come and go.",
    group: "Network",
  },
  {
    term: "Gossip",
    what: "Nodes periodically broadcast a small 'capability record' (free RAM, price, attestation) to the network.",
    impact: "Requesters can shop for suitable workers locally without asking a central server.",
    group: "Network",
  },
  {
    term: "NAT traversal",
    what: "Tricks (AutoNAT, DCUtR hole-punching, relays) that let machines behind home/office routers connect directly.",
    impact: "Ordinary computers on different networks can join with no port-forwarding or fixed IP.",
    group: "Network",
  },
  {
    term: "Bootstrap peer",
    what: "Any reachable node you contact once just to enter the swarm.",
    impact: "Not a central server — it holds no data, is never in the query path, and is freely replaceable.",
    group: "Network",
  },

  // ---- Transport ----
  {
    term: "QUIC",
    what: "A fast, encrypted internet transport (over UDP) with TLS 1.3 built in and mutual authentication.",
    impact: "Everything on the wire is private and pinned to each node's identity; nothing is readable in transit.",
    group: "Transport",
  },
  {
    term: "BDP window",
    what: "Bandwidth-Delay Product — how much data should be 'in flight' = bandwidth × round-trip time.",
    impact: "Sizing the flow-control window to the BDP is what lets a high-latency link reach full speed.",
    group: "Transport",
  },
  {
    term: "Result-stream parallelism",
    what: "Splitting a large result across several QUIC streams at once.",
    impact: "Throughput jumps (often roughly doubles) before plateauing — fewer, slower transfers otherwise.",
    group: "Transport",
  },
  {
    term: "Compression (lz4/zstd)",
    what: "Optionally squeezing result bytes before sending.",
    impact: "Saves bandwidth on compressible data; usually left off on a fast local network.",
    group: "Transport",
  },

  // ---- Storage ----
  {
    term: "Object storage",
    what: "Cloud buckets (S3 / Azure / GCS) where the actual data files live, encrypted at rest.",
    impact: "Workers are pure compute — they never own your data, so a host can't keep it.",
    group: "Storage",
  },
  {
    term: "Scoped credential",
    what: "A short-lived, read-only key that only unlocks the exact data prefix one job needs.",
    impact: "Even a malicious worker can read only that slice, briefly — never your whole bucket or long-term keys.",
    group: "Storage",
  },
  {
    term: "Parquet Modular Encryption",
    what: "Encrypting the data files themselves so the stored bytes are meaningless without the per-job key.",
    impact: "At-rest data is safe even if the storage account is exposed.",
    group: "Storage",
  },

  // ---- Economics / On-chain ----
  {
    term: "Stake",
    what: "Money a worker bonds (locks up) as a deposit before doing paid work.",
    impact: "It's collateral — cheat and you lose it, so honesty is the profitable choice.",
    group: "Economics",
  },
  {
    term: "stake_factor",
    what: "A 0–1 score from the stake amount, log-scaled and capped so whales don't dominate.",
    impact: "More stake slightly raises selection odds, but with diminishing returns (anti-centralization).",
    group: "Economics",
  },
  {
    term: "Slashing",
    what: "Automatically taking part of a worker's stake when it provably misbehaves (wrong result, cheating, downtime).",
    impact: "Turns bad behavior into a direct financial loss; the penalty is split to challenger/burn/treasury.",
    group: "Economics",
  },
  {
    term: "Escrow",
    what: "The requester locks the maximum price up front in a per-job contract.",
    impact: "The worker is guaranteed it can be paid, and the requester can never be over-charged.",
    group: "Economics",
  },
  {
    term: "HTLC release",
    what: "The escrow only pays out when someone presents the agreed quorum result hash.",
    impact: "Money is released by the correct answer itself — no trusted operator decides who gets paid.",
    group: "Economics",
  },
  {
    term: "Participation commission (κ)",
    what: "A small fixed cut paid to each agreeing non-winner.",
    impact: "Pays the workers whose matching results formed the quorum — so honest verification is worth doing.",
    group: "Economics",
  },
  {
    term: "RecordAnchor / epoch root",
    what: "Once per epoch, a Merkle root summarizing many off-chain receipts is written on-chain, chained to the previous one.",
    impact: "Cheap tamper-proof history: rewriting any old record would change every later root.",
    group: "Economics",
  },
  {
    term: "GlobalParams",
    what: "A single on-chain contract holding the economic settings (fees, slashing %, stake tiers) every node reads.",
    impact: "One source of truth that can be updated in place — no node ships hard-coded economics.",
    group: "Economics",
  },
  {
    term: "Stake-receipt jetton (Duckton)",
    what: "A token minted 1:1 when you stake, as on-chain proof of your bond — but transfer-locked.",
    impact: "You can't sell it to dodge slashing; it's an accountability badge, not a tradeable asset.",
    group: "Economics",
  },
  {
    term: "Unbonding cooldown",
    what: "A waiting period before staked funds can be withdrawn, during which they're still slashable.",
    impact: "Stops a worker from cheating and instantly cashing out before it gets caught.",
    group: "Economics",
  },
  {
    term: "ton_proof binding",
    what: "A two-way signature linking a node's identity to a TON wallet (each signs the other).",
    impact: "Makes collusion visible (linked wallets) and ties payouts/penalties to a real account.",
    group: "Economics",
  },
  {
    term: "Testnet vs mainnet",
    what: "Testnet uses play-money for trying things; mainnet uses real funds.",
    impact: "Mainnet is guarded behind an explicit opt-in so you can't spend real money by accident.",
    group: "Economics",
  },
  {
    term: "Free vs paid path",
    what: "Public jobs can run entirely off-chain for free; only paid jobs touch the blockchain.",
    impact: "You only pay (and wait on chain) when you actually want economic guarantees.",
    group: "Economics",
  },
];

export const GLOSSARY_GROUPS: GlossaryGroup[] = [
  "Core",
  "Trust",
  "Network",
  "Transport",
  "Storage",
  "Economics",
];

const BY_TERM = new Map(GLOSSARY.map((t) => [t.term.toLowerCase(), t]));
/** Look up a term (case-insensitive) for inline hints. */
export function lookupTerm(term: string): Term | undefined {
  return BY_TERM.get(term.toLowerCase());
}
