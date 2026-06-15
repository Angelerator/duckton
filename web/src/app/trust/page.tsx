import type { Metadata } from "next";
import {
  BadgeCheck,
  Ban,
  Eye,
  Fingerprint,
  GitCommit,
  Hash,
  Lock,
  ScrollText,
  ShieldCheck,
  Users,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  AttestationBadge,
  DataClassBadge,
  Dot,
  KV,
  PageHeader,
  ScoreBar,
  SectionTitle,
  Stat,
  VerdictBadge,
} from "@/components/common/atoms";
import { CopyId } from "@/components/common/copy";
import { Explainer, InfoHint } from "@/components/common/explain";
import { flagged, receipts, trust, workers } from "@/lib/data";
import { ago, durationSecs, pct } from "@/lib/format";
import { TrustPlots } from "./plots";
import type { AttestationLevel, FaultClass } from "@/lib/types";

export const metadata: Metadata = {
  title: "Trust & Attestation",
  description: "How the duckdb-p2p grid scores untrusted hosts: attestation tiers, reputation from signed receipts, quorum verification, and flagged providers.",
};

/* -------------------------------------------------------------- derivations */

const { weights } = trust; // real α/β/γ/δ from the running trust engine

const faultBadge: Record<FaultClass, { v: "destructive" | "warn" | "muted"; t: string }> = {
  provider: { v: "destructive", t: "provider" },
  requester: { v: "warn", t: "requester" },
  neutral: { v: "muted", t: "—" },
};

/* ------------------------------------------------------------ static panels */

const tiers = [
  {
    level: "L0" as const,
    name: "Anonymous",
    evidence: "Pinned node key",
    proves: "Identity continuity only — same key across sessions.",
    hw: "Any laptop",
  },
  {
    level: "L1" as const,
    name: "Measured boot",
    evidence: "TPM quote (PCRs) + signed event log",
    proves: "A known-good OS / agent image booted (no plaintext-RAM guarantee).",
    hw: "Modern laptops w/ TPM 2.0",
  },
  {
    level: "L2" as const,
    name: "Confidential TEE",
    evidence:
      "HW attestation quote — Intel TDX / AMD SEV-SNP / AWS Nitro — verified vs. allowlisted enclave measurement",
    proves:
      "DuckDB runs in hardware-encrypted memory; the host root user cannot read plaintext.",
    hw: "Confidential cloud VMs",
  },
];

const policyRows = [
  {
    cls: "Public" as const,
    minLevel: "L0" as const,
    minTrust: "0.70",
    quorum: "2–3",
    notes: "Moderate redundancy; cheapest pool, laptops welcome.",
  },
  {
    cls: "Internal" as const,
    minLevel: "L1" as const,
    minTrust: "0.85",
    quorum: "3",
    notes: "Scoped credentials mandatory; measured-boot floor.",
  },
  {
    cls: "Sensitive" as const,
    minLevel: "L2" as const,
    minTrust: "allowlist",
    quorum: "optional",
    notes:
      "Attested enclave or permissioned allowlist; hardware enforces confidentiality.",
  },
];

/* ------------------------------------------------------------------- page */

export default function TrustPage() {
  const avgTrust = workers.reduce((a, w) => a + w.trust, 0) / workers.length;
  const l2Count = workers.filter((w) => w.attestation === "L2").length;
  const tierCount = (lvl: AttestationLevel) =>
    workers.filter((w) => w.attestation === lvl).length;
  const correctReceipts = receipts.filter((r) => r.verdict === "Correct").length;
  const correctRate = receipts.length === 0 ? 0 : correctReceipts / receipts.length;

  // Worked breakdown for one REAL honest, mid-trust host (L1, gate passes).
  const w = workers[3]; // amber-mole · L1 · honest
  const gatePass = w.behavior === "honest" && w.faults === 0; // attestation gate held
  const minLevelForHost = "L1";

  // The real terms the trust engine stored for this worker. effective_trust =
  // gate · clamp(α·R + β·age + γ·voucher + δ·stake − penalty) + exploration.
  const softTerms = [
    {
      label: "R — reputationConfident",
      weight: weights.alpha,
      value: w.reputationConfident,
      contrib: weights.alpha * w.reputationConfident,
      hint: `recency-weighted correctness over ${w.observations} obs`,
    },
    {
      label: "ageFactor",
      weight: weights.beta,
      value: w.ageFactor,
      contrib: weights.beta * w.ageFactor,
      hint: "observation history → maturity",
    },
    {
      label: "voucherTrust",
      weight: weights.gamma,
      value: w.voucherTrust,
      contrib: weights.gamma * w.voucherTrust,
      hint: "trust lent by peers (none here)",
    },
    {
      label: "stakeFactor",
      weight: weights.delta,
      value: w.stakeFactor,
      contrib: weights.delta * w.stakeFactor,
      hint: `log-scaled from ${w.stakeTon} TON staked`,
    },
  ];

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<ShieldCheck />}
        title="Trust & Attestation"
        description="How a requester reasons about which untrusted hosts to trust: identity + attestation tiers + reputation from signed receipts + verification. Every number here is from the real loopback-grid trust engine."
      />

      <Explainer
        what="How the grid decides which untrusted machines to believe: a reputation built from signed receipts, hardware-attestation tiers, and checking that independent results agree (quorum + canary audits)."
        impact="It can safely use strangers' computers — cheaters are caught (wrong fingerprint or a failed secret audit), lose reputation and staked money, and stop getting work."
      />

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-5">
        <Stat
          label="Avg trust"
          value={avgTrust.toFixed(2)}
          sub="over all workers"
          icon={<ShieldCheck />}
          accent="primary"
        />
        <Stat
          label="L2 hosts"
          value={l2Count}
          sub="attested TEE enclaves"
          icon={<Lock />}
          accent="ok"
          hint="Machines running in confidential hardware (TEE) whose operator cannot read the data in memory."
        />
        <Stat
          label="Receipts"
          value={receipts.length}
          sub="signed verdict trail"
          icon={<ScrollText />}
          accent="info"
          hint="Signed records of each job outcome — the portable history reputation is built from."
        />
        <Stat
          label="Correct rate"
          value={pct(correctRate, 0)}
          sub={`${correctReceipts}/${receipts.length} verdicts`}
          icon={<Eye />}
          accent={correctRate >= 0.85 ? "ok" : "warn"}
          hint="Share of jobs whose result passed verification (matched quorum or a canary audit)."
        />
        <Stat
          label="Flagged"
          value={flagged.length}
          sub="providers caught + deselected"
          icon={<Ban />}
          accent="destructive"
        />
      </div>

      {/* Trust score formula + worked breakdown */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Fingerprint className="size-4 text-primary" /> Trust score
          </CardTitle>
          <CardDescription>
            A hard attestation gate multiplied by a clamped blend of soft, signed signals, plus an
            exploration bonus for under-observed hosts.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-5">
          <div className="bg-muted/40 overflow-x-auto rounded-lg border p-4 font-mono text-xs leading-relaxed sm:text-sm">
            {trust.formula}
          </div>

          <div className="flex flex-wrap items-center gap-2 text-xs">
            <Badge variant="outline" className="font-mono">
              α={weights.alpha}
            </Badge>
            <Badge variant="outline" className="font-mono">
              β={weights.beta}
            </Badge>
            <Badge variant="outline" className="font-mono">
              γ={weights.gamma}
            </Badge>
            <Badge variant="outline" className="font-mono">
              δ={weights.delta}
            </Badge>
            <span className="text-muted-foreground">
              The attestation level is a <span className="text-foreground">hard gate</span>{" "}
              (boolean), not just another score input — fail it and effective_trust is 0
              regardless of reputation.
            </span>
          </div>

          <div className="flex flex-wrap gap-2 text-xs">
            <Badge variant="muted" className="font-mono">
              min_trust = {trust.minTrust}
            </Badge>
            <Badge variant="muted" className="font-mono">
              bootstrap_trust = {trust.bootstrapTrust}
            </Badge>
            <Badge variant="muted" className="font-mono">
              half_life = {durationSecs(trust.halfLifeSecs)}
            </Badge>
          </div>

          <Separator />

          <div>
            <div className="mb-3 flex items-center justify-between">
              <SectionTitle className="mb-0" hint="worked example · real terms">
                Sample host
              </SectionTitle>
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium">{w.alias}</span>
                <AttestationBadge level={w.attestation} />
                <CopyId value={w.id} />
              </div>
            </div>

            <div className="space-y-3">
              {softTerms.map((t) => (
                <div key={t.label} className="grid grid-cols-12 items-center gap-3">
                  <div className="col-span-12 sm:col-span-4">
                    <div className="text-sm font-medium">{t.label}</div>
                    <div className="text-muted-foreground text-xs">{t.hint}</div>
                  </div>
                  <div className="col-span-8 sm:col-span-5">
                    <ScoreBar value={t.value} />
                  </div>
                  <div className="text-muted-foreground col-span-4 text-right font-mono text-xs sm:col-span-3">
                    × {t.weight} = {t.contrib.toFixed(3)}
                  </div>
                </div>
              ))}

              <div className="grid grid-cols-12 items-center gap-3">
                <div className="col-span-12 sm:col-span-4">
                  <div className="text-destructive text-sm font-medium">− penalty</div>
                  <div className="text-muted-foreground text-xs">recent faults / disputes</div>
                </div>
                <div className="col-span-8 sm:col-span-5">
                  <div className="bg-secondary h-1.5 w-full overflow-hidden rounded-full">
                    <div
                      className="h-full rounded-full"
                      style={{
                        width: `${Math.round(w.penalty * 100)}%`,
                        background: "var(--destructive)",
                      }}
                    />
                  </div>
                </div>
                <div className="text-destructive col-span-4 text-right font-mono text-xs sm:col-span-3">
                  −{w.penalty.toFixed(3)}
                </div>
              </div>

              <div className="grid grid-cols-12 items-center gap-3">
                <div className="col-span-12 sm:col-span-4">
                  <div className="text-sm font-medium text-[var(--info)]">
                    + explorationBonus
                  </div>
                  <div className="text-muted-foreground text-xs">
                    nudge to sample under-observed hosts
                  </div>
                </div>
                <div className="col-span-8 sm:col-span-5">
                  <div className="bg-secondary h-1.5 w-full overflow-hidden rounded-full">
                    <div
                      className="h-full rounded-full"
                      style={{
                        width: `${Math.round(w.explorationBonus * 100)}%`,
                        background: "var(--info)",
                      }}
                    />
                  </div>
                </div>
                <div className="col-span-4 text-right font-mono text-xs text-[var(--info)] sm:col-span-3">
                  +{w.explorationBonus.toFixed(3)}
                </div>
              </div>
            </div>

            <Separator className="my-4" />

            <dl className="grid gap-x-8 gap-y-1 sm:grid-cols-2">
              <KV label="Attestation gate (hard)">
                <span className="inline-flex items-center gap-1.5">
                  <Dot status={gatePass ? "ok" : "destructive"} />
                  {w.attestation} ≥ {minLevelForHost}{" "}
                  <Badge variant={gatePass ? "ok" : "destructive"}>
                    {gatePass ? "PASS" : "BLOCK"}
                  </Badge>
                </span>
              </KV>
              <KV label="soft (clamped blend)">{w.soft.toFixed(3)}</KV>
              <KV label="reputation (raw)">
                {w.reputation === null ? "—" : w.reputation.toFixed(2)}
              </KV>
              <KV label="effective trust = gate · soft + explore">
                <span className="text-base font-semibold text-[var(--ok)]">
                  {w.trust.toFixed(3)}
                </span>
              </KV>
            </dl>
          </div>
        </CardContent>
      </Card>

      {/* Attestation tiers */}
      <div>
        <SectionTitle
          hint="hardware trust ladder"
          info="L0 anonymous, L1 verified-boot (TPM), L2 confidential enclave (TEE); a hard gate, not just a bonus."
        >
          Attestation tiers
        </SectionTitle>
        <div className="grid gap-4 lg:grid-cols-3">
          {tiers.map((t) => (
            <Card key={t.level}>
              <CardHeader>
                <div className="flex items-center justify-between">
                  <CardTitle className="flex items-center gap-2 text-base">
                    <AttestationBadge level={t.level} />
                    {t.name}
                  </CardTitle>
                  <span className="text-muted-foreground text-xs tabular-nums">
                    {tierCount(t.level)} hosts
                  </span>
                </div>
              </CardHeader>
              <CardContent>
                <dl className="space-y-2.5 text-sm">
                  <div>
                    <dt className="text-muted-foreground text-xs uppercase tracking-wide">
                      Evidence
                    </dt>
                    <dd className="mt-0.5">{t.evidence}</dd>
                  </div>
                  <div>
                    <dt className="text-muted-foreground text-xs uppercase tracking-wide">
                      Proves
                    </dt>
                    <dd className="mt-0.5">{t.proves}</dd>
                  </div>
                  <div>
                    <dt className="text-muted-foreground text-xs uppercase tracking-wide">
                      Hardware
                    </dt>
                    <dd className="mt-0.5">{t.hw}</dd>
                  </div>
                </dl>
              </CardContent>
            </Card>
          ))}
        </div>
        <p className="text-muted-foreground mt-3 text-xs">
          Commodity laptops cap at <span className="font-mono">L0</span>/
          <span className="font-mono">L1</span>; a true &ldquo;the operator can&apos;t read
          RAM&rdquo; guarantee needs <span className="font-mono">L2</span> confidential hardware.
        </p>
      </div>

      {/* Selection policy by data class */}
      <Card>
        <CardHeader>
          <CardTitle>Selection policy by data class</CardTitle>
          <CardDescription>
            What a requester demands of candidate hosts before dispatching, per data class.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Data class</TableHead>
                <TableHead>min_level</TableHead>
                <TableHead className="text-right">min_trust</TableHead>
                <TableHead className="text-right">quorum</TableHead>
                <TableHead className="pr-6">Notes</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {policyRows.map((r) => (
                <TableRow key={r.cls}>
                  <TableCell className="pl-6">
                    <DataClassBadge value={r.cls} />
                  </TableCell>
                  <TableCell>
                    <AttestationBadge level={r.minLevel} />
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {r.minTrust}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {r.quorum}
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-xs whitespace-normal">
                    {r.notes}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Verification */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <BadgeCheck className="size-4 text-primary" /> Verification
          </CardTitle>
          <CardDescription>
            How an answer from an untrusted host is checked before it&apos;s trusted or paid. The
            hash and quorum panels below are the actual results computed during the run.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid gap-4 sm:grid-cols-2">
            {/* 1 — REAL canonical BLAKE3 hash (order-independent proof) */}
            <div className="bg-card flex flex-col gap-3 rounded-lg border p-4">
              <div className="flex items-start gap-3">
                <div className="bg-primary/10 text-primary flex size-8 shrink-0 items-center justify-center rounded-lg [&_svg]:size-4">
                  <Hash />
                </div>
                <div>
                  <div className="flex items-center gap-2 text-sm font-semibold">
                    <span className="text-muted-foreground/60 font-mono text-xs">1</span>
                    Canonical BLAKE3 result hash
                    <InfoHint text="A stable fingerprint of a result so two machines can prove they got the same answer." />
                  </div>
                  <p className="text-muted-foreground mt-1 text-xs leading-relaxed">
                    Order-independent per-row hashing + normalized numeric/NULL form, then BLAKE3.
                    Re-hashing the same {trust.canonical.columns.length} columns ×{" "}
                    {trust.canonical.rows.length} rows in a shuffled row order yields the{" "}
                    <span className="text-foreground">identical</span> digest.
                  </p>
                </div>
              </div>
              <div className="overflow-x-auto rounded-md border">
                <table className="w-full text-left font-mono text-[11px]">
                  <thead className="bg-muted/40 text-muted-foreground">
                    <tr>
                      {trust.canonical.columns.map((c) => (
                        <th key={c} className="px-2 py-1 font-medium">
                          {c}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {trust.canonical.rows.map((row, i) => (
                      <tr key={i} className="border-t">
                        {row.map((cell, j) => (
                          <td key={j} className="px-2 py-1 tabular-nums">
                            {cell}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <dl className="text-xs">
                <KV label="hash" className="py-1">
                  <CopyId value={trust.canonical.hash} />
                </KV>
                <KV label="reordered hash" className="py-1">
                  <CopyId value={trust.canonical.reorderedHash} />
                </KV>
                <KV label="order_independent" className="py-1">
                  <Badge variant={trust.canonical.orderIndependent ? "ok" : "destructive"}>
                    {String(trust.canonical.orderIndependent)}
                  </Badge>
                </KV>
              </dl>
            </div>

            {/* 2 — Commit-first */}
            <div className="bg-card flex gap-3 rounded-lg border p-4">
              <div className="bg-primary/10 text-primary flex size-8 shrink-0 items-center justify-center rounded-lg [&_svg]:size-4">
                <GitCommit />
              </div>
              <div>
                <div className="flex items-center gap-2 text-sm font-semibold">
                  <span className="text-muted-foreground/60 font-mono text-xs">2</span>
                  Commit-first
                </div>
                <p className="text-muted-foreground mt-1 text-xs leading-relaxed">
                  Workers send result_hash before streaming any rows. Committing the answer up front
                  prevents a host from adapting its result to match peers it observes.
                </p>
              </div>
            </div>

            {/* 3 — REAL quorum (evaluate_quorum result) */}
            <div className="bg-card flex flex-col gap-3 rounded-lg border p-4">
              <div className="flex items-start gap-3">
                <div className="bg-primary/10 text-primary flex size-8 shrink-0 items-center justify-center rounded-lg [&_svg]:size-4">
                  <Users />
                </div>
                <div>
                  <div className="flex items-center gap-2 text-sm font-semibold">
                    <span className="text-muted-foreground/60 font-mono text-xs">3</span>
                    Quorum / redundant execution
                  </div>
                  <p className="text-muted-foreground mt-1 text-xs leading-relaxed">
                    Run on k hosts, require ≥ q matching hashes. The fastest agreeing host streams
                    the data; the losers RESET their in-flight streams. Below is the real
                    evaluate_quorum outcome over {trust.quorum.hashes.length} committed hashes.
                  </p>
                </div>
              </div>
              <div className="flex flex-wrap gap-1.5">
                {trust.quorum.hashes.map((h, i) => {
                  const agreed = h === trust.quorum.hashes[0];
                  return (
                    <Badge
                      key={i}
                      variant={agreed ? "ok" : "destructive"}
                      className="font-mono"
                    >
                      {h}
                    </Badge>
                  );
                })}
              </div>
              <dl className="text-xs">
                <KV label="agreement / quorum" className="py-1">
                  <span className="font-mono">
                    {trust.quorum.agreement} / {trust.quorum.quorum}
                  </span>
                </KV>
                <KV label="reached" className="py-1">
                  <Badge variant={trust.quorum.reached ? "ok" : "destructive"}>
                    {String(trust.quorum.reached)}
                  </Badge>
                </KV>
              </dl>
            </div>

            {/* 4 — Canary auditing */}
            <div className="bg-card flex gap-3 rounded-lg border p-4">
              <div className="bg-primary/10 text-primary flex size-8 shrink-0 items-center justify-center rounded-lg [&_svg]:size-4">
                <Eye />
              </div>
              <div>
                <div className="flex items-center gap-2 text-sm font-semibold">
                  <span className="text-muted-foreground/60 font-mono text-xs">4</span>
                  Canary auditing
                  <InfoHint text="A query whose answer the requester already knows, slipped in to catch cheaters." />
                </div>
                <p className="text-muted-foreground mt-1 text-xs leading-relaxed">
                  Inject queries whose answer is already known. A worker that returns the wrong hash
                  is marked Incorrect and slashed — exactly what happened to the flagged providers
                  below.
                </p>
              </div>
            </div>
          </div>
          <div className="bg-muted/40 text-muted-foreground rounded-lg border p-3 text-xs">
            <span className="text-foreground font-medium">Honest-limit note —</span> quorum
            assumes an honest majority among the chosen k. That assumption is why it is combined
            with reputation, attestation gating, Sybil cost (stake), and canaries rather than used
            alone.
          </div>
        </CardContent>
      </Card>

      {/* Receipts */}
      <Card>
        <CardHeader>
          <CardTitle>Receipts</CardTitle>
          <CardDescription>
            Signed verdicts that feed reputation. Recency-weighted{" "}
            <span className="font-mono">
              R = Σ wᵢ·correctᵢ / Σ wᵢ, &nbsp;wᵢ = decayᵗ · job_weight
            </span>
            . Receipts are gossiped / DHT-stored independently, so hiding a bad receipt is caught
            (anti-omission ⇒ treat hidden as low trust).
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Job</TableHead>
                <TableHead>Worker</TableHead>
                <TableHead>Verdict</TableHead>
                <TableHead>Fault</TableHead>
                <TableHead className="text-right">Latency</TableHead>
                <TableHead>Verified</TableHead>
                <TableHead>Sig</TableHead>
                <TableHead className="pr-6 text-right">When</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {receipts.map((r) => {
                const fb = faultBadge[r.fault];
                return (
                  <TableRow key={`${r.jobId}-${r.workerId}`}>
                    <TableCell className="pl-6 font-mono text-xs">
                      <CopyId value={r.jobId} />
                    </TableCell>
                    <TableCell className="text-sm">{r.workerAlias}</TableCell>
                    <TableCell>
                      <VerdictBadge verdict={r.verdict} />
                    </TableCell>
                    <TableCell>
                      <Badge variant={fb.v}>{fb.t}</Badge>
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{r.latencyMs}ms</TableCell>
                    <TableCell>
                      {r.verified ? (
                        <span className="inline-flex items-center gap-1.5 text-xs">
                          <Dot status="ok" /> yes
                        </span>
                      ) : (
                        <Badge variant="warn">no</Badge>
                      )}
                    </TableCell>
                    <TableCell className="font-mono text-xs">{r.sig}</TableCell>
                    <TableCell className="text-muted-foreground pr-6 text-right text-xs">
                      {ago(r.tsMs)}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Flagged providers (caught by verification) */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Ban className="size-4 text-destructive" /> Flagged providers · caught by verification
          </CardTitle>
          <CardDescription>
            Real nodes the trust engine penalized this run. They returned a divergent hash
            (Incorrect) or never committed (Timeout), were verified against the quorum, and the
            engine drove their reputation and trust to ~0 — so the scheduler stops selecting them.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Worker</TableHead>
                <TableHead>Attestation</TableHead>
                <TableHead>Behavior</TableHead>
                <TableHead className="text-right">correct / faults</TableHead>
                <TableHead className="text-right">reputation</TableHead>
                <TableHead className="pr-6 text-right">trust</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {flagged.map((f) => (
                <TableRow key={f.id}>
                  <TableCell className="pl-6 text-sm">{f.alias}</TableCell>
                  <TableCell>
                    <AttestationBadge level={f.attestation} />
                  </TableCell>
                  <TableCell>
                    <Badge variant="destructive">{f.behavior}</Badge>
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {f.correct}/{f.faults}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs tabular-nums">
                    {f.reputation === null ? "—" : f.reputation.toFixed(2)}
                  </TableCell>
                  <TableCell className="pr-6 text-right font-mono text-xs tabular-nums">
                    {f.trust.toFixed(2)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
        <CardContent className="text-muted-foreground text-xs">
          There is no central authority here — each node independently verifies a verdict against
          the committed hashes and decides on its own whether to down-weight or drop a peer.
        </CardContent>
      </Card>

      <TrustPlots />
    </div>
  );
}
