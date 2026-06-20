import type { Metadata } from "next";
import type { ReactNode } from "react";
import {
  Activity,
  BadgeCheck,
  Coins,
  Fingerprint,
  Gauge,
  Gavel,
  Hash,
  Landmark,
  Layers,
  Lock,
  LockKeyhole,
  Percent,
  Scale,
  ShieldCheck,
  Timer,
  Wallet,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { KV, PageHeader, SectionTitle, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { BarMini } from "@/components/common/charts";
import { CopyId } from "@/components/common/copy";
import { config, epochs, settlement, stakeAccounts } from "@/lib/data";
import { bytes, durationSecs, inFuture, ms, num, pct } from "@/lib/format";
import { EarningsCalculator } from "./earnings-calculator";
import { SettlementPlots } from "./plots";

export const metadata: Metadata = { title: "Settlement" };

/* ---------------------------------------------------------------- contracts */

const CONTRACTS: {
  name: string;
  icon: ReactNode;
  role: string;
  doc: string;
  points: string[];
}[] = [
  {
    name: "StakeVault",
    icon: <Landmark />,
    role: "Per-node bond custody (non-custodial).",
    doc: "§stake.vault",
    points: [
      "deposit + unbond with cooldown",
      "graduated slashing + split",
      "keeper-triggered withdraw",
      "also the receipt-jetton master",
    ],
  },
  {
    name: "StakeReceiptWallet",
    icon: <LockKeyhole />,
    role: "1:1 TEP-74 stake-receipt jetton.",
    doc: "§stake.receipt",
    points: [
      "minted 1:1 against bond",
      "transfer-LOCKED while bonded",
      "anti-exit / anti-wash proof",
      "burned on full unbond",
    ],
  },
  {
    name: "JobEscrow",
    icon: <Lock />,
    role: "Per-job non-custodial escrow.",
    doc: "§escrow.job",
    points: [
      "one contract instance per paid job",
      "HTLC-style release on quorum result hash",
      "refund-on-timeout to requester",
      "no funds pooled globally",
    ],
  },
  {
    name: "RecordAnchor",
    icon: <Layers />,
    role: "Per-epoch Merkle root (chained).",
    doc: "§anchor.epoch",
    points: [
      "one root per epoch, chained to parent",
      "stake-weighted acceptance",
      "permissionless keeper submission",
      "bonded dispute → verdict",
    ],
  },
];

/* ----------------------------------------------------------------- slashing */

const SLASH_ROWS: { condition: string; pct: number; severity: "low" | "medium" | "high" | "max" }[] = [
  { condition: "wrong_result", pct: settlement.slashing.wrongResultPct, severity: "high" },
  { condition: "provable_cheat", pct: settlement.slashing.cheatPct, severity: "max" },
  { condition: "equivocation", pct: settlement.slashing.equivocationPct, severity: "high" },
  { condition: "failed_commitment", pct: settlement.slashing.failedCommitmentPct, severity: "medium" },
  { condition: "downtime", pct: settlement.slashing.downtimePct, severity: "low" },
];

const SEVERITY_VARIANT = {
  low: "muted",
  medium: "warn",
  high: "destructive",
  max: "destructive",
} as const;

const SLASH_SPLIT: { label: string; frac: number }[] = [
  { label: "to challenger", frac: settlement.slashing.toChallenger },
  { label: "to redundancy", frac: settlement.slashing.toRedundancy },
  { label: "to burn", frac: settlement.slashing.toBurn },
  { label: "to treasury", frac: settlement.slashing.toTreasury },
];

/* --------------------------------------------- pricing & ranking (real cfg) */

// The live `economics.pricing` / `economics.ranking` sections of this node's
// GridConfig (from the snapshot), read defensively.
const eco = (config.value.economics ?? {}) as Record<string, unknown>;
const pricingCfg = (eco.pricing ?? {}) as Record<string, unknown>;
const rankingCfg = (eco.ranking ?? {}) as Record<string, unknown>;
const numOr = (v: unknown, d = 0): number => (typeof v === "number" ? v : d);
const strOr = (v: unknown, d = ""): string => (typeof v === "string" ? v : d);

const pricingModel = strOr(pricingCfg.model, "metered");
const capMultiplier = numOr(pricingCfg.cap_multiplier, 5);
const meteringTolerance = numOr(pricingCfg.metering_tolerance, 1.5);
const wStake = numOr(rankingCfg.w_stake);
const stakeReliabilityFloor = numOr(rankingCfg.stake_reliability_floor);

export default function SettlementPage() {
  const totalBonded = stakeAccounts.reduce((a, s) => a + s.stakeTon, 0);
  const settledCount = settlement.events.filter((e) => e.type === "Settled").length;
  const latestEpoch = epochs[0];
  const split = settlement.splits[0];
  const proof = settlement.inclusionProof;
  const stk = settlement.stake;
  const bind = settlement.binding;
  const ql = settlement.quality;

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Coins />}
        title="Settlement"
        description="An optional TON-based economic layer (design). Only PAID jobs touch chain; free jobs run fully off-chain and never reach any contract. The figures below are REAL outputs from this run's paid jobs — the settlement doubles opened, settled, and anchored them on-chain."
      >
        <Badge variant="info">design · TON</Badge>
        <a
          href="/ton"
          className="border-input hover:bg-accent inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm font-medium transition-colors"
        >
          On-chain contracts →
        </a>
      </PageHeader>

      <Explainer
        what="The optional 'who gets paid' layer. The requester escrows a maximum price up front; the winning worker is paid from it, each agreeing helper gets a small fixed cut, and the unused remainder is refunded."
        impact="Honest, fast work earns more and provably bad work loses staked money — so the economics push everyone toward correct results. Free public jobs skip all of this and never touch a blockchain."
      />

      {/* Stat row — real settlement outputs */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-5">
        <Stat
          label="Total bonded"
          value={`${num(totalBonded)} TON`}
          sub="stake at risk across nodes"
          icon={<Coins />}
          accent="warn"
          hint="Stake (collateral) locked by workers; they lose it if they cheat."
        />
        <Stat
          label="Escrow / paid job"
          value={`${num(settlement.escrowMaxBidTon)} TON`}
          sub="max bid locked per JobEscrow"
          icon={<Lock />}
          accent="info"
          hint="The max price the requester locks up front so payout is guaranteed and capped."
        />
        <Stat
          label="Paid jobs settled"
          value={settledCount}
          sub="real escrow open→settle"
          icon={<Coins />}
          accent="ok"
        />
        <Stat
          label="Epoch records"
          value={settlement.anchorRecords}
          sub="receipts anchored on-chain"
          icon={<Layers />}
          accent="primary"
        />
        <Stat
          label="Inclusion verified"
          value={proof?.verified ? "yes" : "no"}
          sub="proof checks against root"
          icon={<ShieldCheck />}
          accent={proof?.verified ? "ok" : "destructive"}
          hint="A cryptographic check that a single receipt really is part of the anchored on-chain summary."
        />
      </div>

      {/* Economics model — fees, time-based pricing, reliability-gated stake */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Percent className="size-4 text-[var(--warn)]" /> Fee split
            </CardTitle>
            <CardDescription>
              The admin defaults enforced on-chain in <span className="font-mono">GlobalParams</span>.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <dl>
              <KV label="platform fee φ">
                <span className="font-semibold">{pct(settlement.fees.platformFeePct, 0)}</span>
              </KV>
              <KV label="verifier commission κ">
                <span className="font-semibold">
                  {pct(settlement.fees.participationCommissionFrac, 0)}
                </span>
              </KV>
              <KV label="verification surcharge">
                {pct(settlement.fees.verificationSurchargePct, 0)}
              </KV>
            </dl>
            <p className="text-muted-foreground mt-2 text-xs">
              φ is taken once on the escrow; κ is a flat cut paid to <em>each</em> agreeing
              non-winner that helped form the quorum. The winner takes the bounded remainder.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Timer className="size-4 text-primary" /> Time-based pricing
            </CardTitle>
            <CardDescription>
              The default <span className="font-mono">{pricingModel}</span> cost model for paid jobs.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <dl>
              <KV label="formula">
                <span className="font-mono text-xs">rate × seconds</span>
              </KV>
              <KV label="cap multiplier">{capMultiplier}×</KV>
              <KV label="metering tolerance">{meteringTolerance.toFixed(1)}×</KV>
            </dl>
            <p className="text-muted-foreground mt-2 text-xs">
              Cost = per-second rate × processing seconds. The {capMultiplier}× cap is both the
              billing ceiling and a hard execution deadline; the escrow is sized to the worst case
              (an up-front coverage check), and the unused remainder is refunded.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Scale className="size-4 text-primary" /> Stake weighting
            </CardTitle>
            <CardDescription>
              How bonded stake influences a host&apos;s selection ranking.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <dl>
              <KV label="w_stake">{wStake.toFixed(2)}</KV>
              <KV label="reliability floor">{stakeReliabilityFloor.toFixed(2)}</KV>
            </dl>
            <p className="text-muted-foreground mt-2 text-xs">
              The stake ranking term is <span className="text-foreground">reliability-gated</span>:
              it only amplifies hosts whose verified-success rate already clears the floor, so extra
              stake can never rescue an unreliable node or buy its way to the top.
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Contracts */}
      <div>
        <SectionTitle
          hint="sharded · no global contract"
          info="The smart contracts that hold escrow, custody stake, and record results — split per job and per node, with no single global pot."
        >
          Contracts
        </SectionTitle>
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          {CONTRACTS.map((c) => (
            <Card key={c.name}>
              <CardHeader>
                <div className="flex items-start justify-between gap-2">
                  <div className="flex items-center gap-2">
                    <span className="text-primary [&_svg]:size-4">{c.icon}</span>
                    <CardTitle className="font-mono text-sm">{c.name}</CardTitle>
                  </div>
                  <Badge variant="muted" className="font-mono">
                    {c.doc}
                  </Badge>
                </div>
                <CardDescription>{c.role}</CardDescription>
              </CardHeader>
              <CardContent>
                <ul className="space-y-1.5 text-xs">
                  {c.points.map((p) => (
                    <li key={p} className="flex gap-2">
                      <span className="text-primary/70 mt-1.5 size-1 shrink-0 rounded-full bg-current" />
                      <span className="text-muted-foreground">{p}</span>
                    </li>
                  ))}
                </ul>
              </CardContent>
            </Card>
          ))}
        </div>
      </div>

      {/* Earnings model (interactive client island) */}
      <EarningsCalculator />

      {/* Analytics (plotly) */}
      <SettlementPlots />

      {/* Staking */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Coins className="size-4 text-[var(--warn)]" /> Staking
          </CardTitle>
          <CardDescription>
            Minimum stake is the primary Sybil cost — it augments proof-of-work
            and social vouching. Real thresholds: Public ≥{" "}
            <span className="font-mono">{num(stk.minStake)}</span>, Internal ≥{" "}
            <span className="font-mono">{num(stk.minStakeInternal)}</span>,
            Sensitive ≥ <span className="font-mono">{num(stk.minStakeSensitive)}</span>{" "}
            TON (cap {num(stk.stakeCap)}). Unbonding enters a{" "}
            <span className="font-mono">{durationSecs(stk.unbondingSecs)}</span>{" "}
            cooldown, and the receipt jetton is{" "}
            {stk.receiptJetton ? "minted" : "not minted"}
            {stk.receiptTransferLocked ? " and transfer-locked" : ""} as a
            non-transferable proof of an active bond.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Node</TableHead>
                <TableHead>Wallet</TableHead>
                <TableHead className="text-right">Stake</TableHead>
                <TableHead className="text-right">stake_factor</TableHead>
                <TableHead>Public</TableHead>
                <TableHead>Internal</TableHead>
                <TableHead className="pr-6">Sensitive</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {stakeAccounts.map((s) => (
                <TableRow key={s.workerId}>
                  <TableCell className="pl-6 font-medium">{s.alias}</TableCell>
                  <TableCell>
                    <CopyId value={s.wallet} />
                  </TableCell>
                  <TableCell className="text-right tabular-nums">
                    {num(s.stakeTon)} TON
                  </TableCell>
                  <TableCell className="text-right tabular-nums">
                    {s.stakeFactor.toFixed(2)}
                  </TableCell>
                  <TableCell>
                    <EligBadge ok={s.eligiblePublic} />
                  </TableCell>
                  <TableCell>
                    <EligBadge ok={s.eligibleInternal} />
                  </TableCell>
                  <TableCell className="pr-6">
                    <EligBadge ok={s.eligibleSensitive} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Stake-factor curve */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Scale className="size-4 text-primary" /> Stake-factor curve
          </CardTitle>
          <CardDescription>
            The real diminishing-returns curve mapping bonded TON →{" "}
            <span className="font-mono">stake_factor</span>. It rises steeply at
            first, then flattens and caps at 1.0 — so a single whale cannot buy
            unbounded trust, while a small bond still earns meaningful weight.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <BarMini
            data={settlement.stakeCurve}
            xKey="stakeTon"
            yKey="factor"
            color="var(--chart-3)"
          />
        </CardContent>
      </Card>

      {/* Slashing */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Gavel className="size-4 text-destructive" /> Slashing (graduated)
            </CardTitle>
            <CardDescription>
              Severity scales with intent — honest faults bleed slowly, provable
              cheating burns the whole bond. Each fraction below is the share of
              the bond slashed for that condition. Challenge window:{" "}
              <span className="font-mono">
                {durationSecs(settlement.slashing.challengeWindowSecs)}
              </span>
              .
            </CardDescription>
          </CardHeader>
          <CardContent className="px-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-6">Condition</TableHead>
                  <TableHead>Severity</TableHead>
                  <TableHead className="pr-6 text-right">Bond slashed</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {SLASH_ROWS.map((row) => (
                  <TableRow key={row.condition}>
                    <TableCell className="pl-6 font-mono text-xs">
                      {row.condition}
                    </TableCell>
                    <TableCell>
                      <Badge variant={SEVERITY_VARIANT[row.severity]}>
                        {row.severity}
                      </Badge>
                    </TableCell>
                    <TableCell className="pr-6 text-right tabular-nums">
                      {pct(row.pct)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Scale className="size-4 text-destructive" /> Slash proceeds split
            </CardTitle>
            <CardDescription>
              When a bond is slashed, the proceeds are split deterministically
              across these recipients — rewarding the challenger and redundancy
              providers, burning a portion, and funding the treasury.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <div className="space-y-3">
              {SLASH_SPLIT.map((s) => (
                <div key={s.label} className="space-y-1">
                  <div className="flex items-baseline justify-between gap-3">
                    <span className="text-sm">{s.label}</span>
                    <span className="text-sm font-semibold tabular-nums">
                      {pct(s.frac)}
                    </span>
                  </div>
                  <div className="bg-secondary h-1.5 w-full overflow-hidden rounded-full">
                    <div
                      className="bg-primary h-full rounded-full"
                      style={{ width: `${Math.round(s.frac * 100)}%` }}
                    />
                  </div>
                </div>
              ))}
            </div>
            <Separator className="my-4" />
            <p className="text-muted-foreground text-xs">
              Fractions sum to 1.0 — the entire slashed amount is distributed,
              never retained by any single party.
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Settlement events + payout split */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Coins className="size-4 text-[var(--ok)]" /> Settlement events + payout split
          </CardTitle>
          <CardDescription>
            The real escrow open/settle actions from the paid run, and the exact
            on-chain payout split for the first settled job. Each settled total
            equals the escrowed max bid B = {num(settlement.escrowMaxBidTon)} TON.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-6 lg:grid-cols-2">
          <div>
            <SectionTitle className="mb-2" hint="real escrow lifecycle">
              Events
            </SectionTitle>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-6">Type</TableHead>
                  <TableHead>Job</TableHead>
                  <TableHead className="pr-6 text-right">TON</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {settlement.events.map((e, i) => (
                  <TableRow key={`${e.type}-${e.job}-${i}`}>
                    <TableCell className="pl-6">
                      <Badge variant={e.type === "Settled" ? "ok" : "info"}>
                        {e.type}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <CopyId value={e.job} />
                    </TableCell>
                    <TableCell className="pr-6 text-right tabular-nums">
                      {num(e.totalTon ?? e.maxBidTon ?? 0)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>

          <div>
            <SectionTitle
              className="mb-2"
              hint="first settled job"
              info="How the escrow is divided: the winner is paid, a participation commission is a small fixed cut to each worker whose matching result helped form the quorum, and the rest is refunded."
            >
              Payout split
            </SectionTitle>
            {split ? (
              <div className="rounded-lg border px-4 py-1">
                <KV label="winner">
                  <span className="text-[var(--ok)]">{num(split.winnerTon)} TON</span>
                </KV>
                <KV label="platform fee">{num(split.platformFeeTon)} TON</KV>
                {split.participants.map((p, i) => (
                  <KV key={`${p.wallet}-${i}`} label={`commission · ${p.wallet}`}>
                    {num(p.amountTon)} TON
                  </KV>
                ))}
                <Separator className="my-1" />
                <KV label="total (= escrow B)">
                  <span className="font-semibold">{num(split.totalTon)} TON</span>
                </KV>
                <div className="flex items-center justify-between gap-4 py-1.5">
                  <dt className="text-muted-foreground text-sm">result hash</dt>
                  <dd>
                    <CopyId value={split.resultHashHex} />
                  </dd>
                </div>
              </div>
            ) : (
              <p className="text-muted-foreground text-sm">No split recorded.</p>
            )}
          </div>
        </CardContent>
      </Card>

      {/* Epoch anchoring */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Layers className="size-4 text-primary" /> Epoch anchoring
          </CardTitle>
          <CardDescription>
            High-volume receipts stay OFF-chain in a BLAKE3 Merkle tree and are
            anchored on-chain once per epoch. This inclusion proof really
            verifies a single receipt leaf against the anchored root.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-6 lg:grid-cols-2">
          <div className="rounded-lg border px-4 py-1">
            <div className="flex items-center justify-between gap-4 py-1.5">
              <dt className="text-muted-foreground text-sm">epoch</dt>
              <dd className="font-mono text-sm tabular-nums">#{latestEpoch.epoch}</dd>
            </div>
            <div className="flex items-center justify-between gap-4 py-1.5">
              <dt className="text-muted-foreground text-sm">merkle root</dt>
              <dd>
                <CopyId value={latestEpoch.merkleRoot} />
              </dd>
            </div>
            <KV label="records">{num(latestEpoch.jobs)}</KV>
            <KV label="stake-weight">{latestEpoch.stakeWeight.toFixed(2)}</KV>
            <div className="flex items-center justify-between gap-4 py-1.5">
              <dt className="text-muted-foreground text-sm">inclusion verified</dt>
              <dd>
                <Badge variant={latestEpoch.inclusionVerified ? "ok" : "destructive"}>
                  {latestEpoch.inclusionVerified ? "verified" : "unverified"}
                </Badge>
              </dd>
            </div>
          </div>

          <div className="rounded-lg border px-4 py-1">
            {proof ? (
              <>
                <div className="flex items-center justify-between gap-4 py-1.5">
                  <dt className="text-muted-foreground text-sm">leaf</dt>
                  <dd>
                    <CopyId value={proof.leafHex} />
                  </dd>
                </div>
                <KV label="siblings">{proof.siblings.length}</KV>
                <div className="flex items-center justify-between gap-4 py-1.5">
                  <dt className="text-muted-foreground text-sm">result</dt>
                  <dd>
                    <Badge variant={proof.verified ? "ok" : "destructive"} className="gap-1">
                      <BadgeCheck className="size-3" />
                      {proof.verified ? "verifies against root" : "failed"}
                    </Badge>
                  </dd>
                </div>
                <Separator className="my-2" />
                <p className="text-muted-foreground text-xs">
                  A BLAKE3 Merkle path of {proof.siblings.length} sibling hashes
                  reconstructs the anchored root from this leaf — proving the
                  receipt was included without revealing the rest of the tree.
                </p>
              </>
            ) : (
              <p className="text-muted-foreground text-sm">No inclusion proof.</p>
            )}
          </div>
        </CardContent>
      </Card>

      {/* Quality score + Wallet binding */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Gauge className="size-4 text-primary" /> Quality score
            </CardTitle>
            <CardDescription>
              Real per-node quality, blended from success ratio, latency, and
              verified throughput.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <div className="rounded-lg border px-4 py-1">
              <KV label="success ratio">{pct(ql.sample.successRatio, 1)}</KV>
              <KV label="latency">{ms(ql.sample.latencyMs)}</KV>
              <KV label="bytes verified">{bytes(ql.sample.bytesVerified)}</KV>
              <Separator className="my-1" />
              <KV label="throughput score">{ql.throughputScore.toFixed(2)}</KV>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground text-sm">quality score</dt>
                <dd className="text-[var(--ok)] text-sm font-semibold tabular-nums">
                  {ql.qualityScore.toFixed(3)}
                </dd>
              </div>
            </div>
          </CardContent>
        </Card>

        {/* Wallet <-> node binding */}
        <Card className="lg:col-span-2">
          <CardHeader>
            <div className="flex items-start justify-between gap-2">
              <div>
                <CardTitle className="flex items-center gap-2">
                  <Fingerprint className="size-4 text-primary" /> Wallet ↔ node binding
                </CardTitle>
                <CardDescription>
                  A real, mutual two-way{" "}
                  <span className="font-mono">ton_proof</span> handshake links a
                  TON wallet to a node identity: the node signs the wallet
                  payload (<span className="font-mono">sig_node</span>) and the
                  wallet counter-signs over the node id (
                  <span className="font-mono">ton_proof</span>), so neither side
                  can repudiate the binding. This binding{" "}
                  <span className="text-foreground font-medium">
                    cryptographically verifies
                  </span>
                  .
                </CardDescription>
              </div>
              {bind.verified ? (
                <Badge variant="ok" className="gap-1">
                  <BadgeCheck className="size-3" /> verified
                </Badge>
              ) : (
                <Badge variant="destructive">unverified</Badge>
              )}
            </div>
          </CardHeader>
          <CardContent className="grid gap-6 lg:grid-cols-2">
            <div className="rounded-lg border px-4 py-1">
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground text-sm">node id</dt>
                <dd>
                  <CopyId value={bind.nodeId} />
                </dd>
              </div>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground text-sm">wallet</dt>
                <dd>
                  <CopyId value={bind.walletAddress} />
                </dd>
              </div>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground text-sm">nonce</dt>
                <dd className="font-mono text-xs">{bind.nonceHex}</dd>
              </div>
              <KV label="expiry">{inFuture(bind.expiry * 1000)}</KV>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground text-sm">sig_node</dt>
                <dd className="font-mono text-xs">{bind.sigNodeHex}</dd>
              </div>
            </div>

            <div className="bg-muted/40 rounded-lg border px-4 py-1">
              <div className="text-muted-foreground flex items-center gap-1.5 pt-2 pb-1 text-xs font-semibold tracking-wide uppercase">
                <Wallet className="size-3.5" /> ton_proof
              </div>
              <KV label="domain">
                <span className="font-mono text-xs">{bind.tonProof.domain}</span>
              </KV>
              <KV label="timestamp">
                <span className="font-mono text-xs">{bind.tonProof.timestamp}</span>
              </KV>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground flex items-center gap-1.5 text-sm">
                  <Hash className="size-3.5" /> payload
                </dt>
                <dd className="font-mono text-xs">{bind.tonProof.payloadHex}</dd>
              </div>
              <div className="flex items-center justify-between gap-4 py-1.5">
                <dt className="text-muted-foreground flex items-center gap-1.5 text-sm">
                  <Activity className="size-3.5" /> signature
                </dt>
                <dd className="font-mono text-xs">{bind.tonProof.signatureHex}</dd>
              </div>
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

function EligBadge({ ok }: { ok: boolean }) {
  return ok ? (
    <Badge variant="ok">ok</Badge>
  ) : (
    <Badge variant="muted">—</Badge>
  );
}
