"use client";

import * as React from "react";
import {
  Search,
  Wallet,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
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
  Dot,
  KV,
  ScoreBar,
  SectionTitle,
} from "@/components/common/atoms";
import { Spark } from "@/components/common/charts";
import { CopyId } from "@/components/common/copy";
import { trust } from "@/lib/data";
import { useLive } from "@/lib/live";
import { bytes, ms, num, pct } from "@/lib/format";
import type { AttestationLevel, Worker } from "@/lib/types";

/* ----------------------------------------------------------------- helpers */

type SortKey =
  | "trust"
  | "reputationConfident"
  | "stakeTon"
  | "observations"
  | "successRate";

const SORTS: { key: SortKey; label: string }[] = [
  { key: "trust", label: "Trust" },
  { key: "reputationConfident", label: "Reputation" },
  { key: "stakeTon", label: "Stake" },
  { key: "observations", label: "Observations" },
  { key: "successRate", label: "Success rate" },
];

const ATTESTATION_MEANING: Record<AttestationLevel, string> = {
  L0: "anonymous host — software-only, no hardware root of trust",
  L1: "measured boot — TPM-backed attestation of the boot chain",
  L2: "TEE enclave — code runs in hardware-encrypted memory (TDX / SEV-SNP)",
};

const BEHAVIOR_BADGE: Record<
  Worker["behavior"],
  { variant: Parameters<typeof Badge>[0]["variant"]; label: string } | null
> = {
  honest: null,
  cheat: { variant: "destructive", label: "cheat" },
  fail: { variant: "destructive", label: "fail" },
};

function sortWorkers(list: Worker[], key: SortKey): Worker[] {
  return [...list].sort((a, b) => b[key] - a[key]);
}

// Deterministic latency series for the detail spark, derived from the worker's
// real measured p50 (no Math.random — same on server and client).
function p50Series(w: Worker): number[] {
  return Array.from({ length: 20 }, (_, i) =>
    Math.round(w.p50LatencyMs * (1 + 0.15 * Math.sin(i / 2 + w.alias.length)))
  );
}

/** A small labelled bar for one weighted term of the real trust formula. */
function TermBar({
  label,
  weight,
  raw,
  contribution,
  tone = "pos",
}: {
  label: string;
  weight: string;
  raw: number;
  contribution: number;
  tone?: "pos" | "neg";
}) {
  const color = tone === "neg" ? "var(--destructive)" : "var(--info)";
  const width = Math.min(100, Math.max(0, Math.abs(contribution) * 100));
  return (
    <div>
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="text-muted-foreground">
          {label} <span className="font-mono">{weight}</span>
        </span>
        <span className="tabular-nums">
          {tone === "neg" ? "−" : ""}
          {Math.abs(contribution).toFixed(3)}
          <span className="text-muted-foreground"> · raw {raw.toFixed(2)}</span>
        </span>
      </div>
      <div className="bg-secondary mt-1 h-1.5 w-full overflow-hidden rounded-full">
        <div
          className="h-full rounded-full"
          style={{ width: `${width}%`, background: color }}
        />
      </div>
    </div>
  );
}

function WorkerSheet({ w }: { w: Worker }) {
  const { alpha, beta, gamma, delta } = trust.weights;
  // reputation may be null (no signed history yet) → bootstrap.
  const repRaw = w.reputation ?? trust.bootstrapTrust;

  return (
    <>
      <SheetHeader>
        <SheetTitle className="flex flex-wrap items-center gap-2">
          <span>{w.alias}</span>
          <AttestationBadge level={w.attestation} />
          <Dot status={w.online ? "ok" : "muted"} pulse={w.online} />
          <span className="text-muted-foreground text-xs font-normal">
            {w.online ? "online" : "offline"}
          </span>
          {BEHAVIOR_BADGE[w.behavior] ? (
            <Badge variant={BEHAVIOR_BADGE[w.behavior]!.variant}>
              {BEHAVIOR_BADGE[w.behavior]!.label}
            </Badge>
          ) : null}
        </SheetTitle>
        <CopyId value={w.id} truncate={false} />
      </SheetHeader>

      <div className="flex-1 space-y-6 overflow-y-auto px-4 pb-6">
        {/* recent latency spark (deterministic from real p50) */}
        <div className="bg-card rounded-lg border p-3">
          <div className="mb-1 flex items-center justify-between">
            <span className="text-muted-foreground text-xs font-medium">
              latency profile (p50)
            </span>
            <span className="text-sm font-semibold tabular-nums">
              {ms(w.p50LatencyMs)}
            </span>
          </div>
          <Spark data={p50Series(w)} color="var(--chart-1)" />
        </div>

        {/* Identity */}
        <div>
          <SectionTitle>Identity</SectionTitle>
          <dl className="divide-y">
            <KV label="node id">
              <CopyId value={w.id} />
            </KV>
            <KV label="wallet">
              {w.wallet ? (
                <CopyId value={w.wallet} />
              ) : (
                <span className="text-muted-foreground">unbonded — no wallet</span>
              )}
            </KV>
            <KV label="engine">
              <span className="font-mono text-xs">{w.engineVersion}</span>
            </KV>
            <KV label="observations">{num(w.observations)}</KV>
            <KV label="behavior">
              {BEHAVIOR_BADGE[w.behavior] ? (
                <Badge variant={BEHAVIOR_BADGE[w.behavior]!.variant}>
                  {w.behavior}
                </Badge>
              ) : (
                <Badge variant="muted">honest</Badge>
              )}
            </KV>
          </dl>
        </div>

        {/* Trust breakdown — the REAL formula terms */}
        <div>
          <SectionTitle hint="α·R + β·age + γ·voucher + δ·stake − penalty">
            Trust breakdown
          </SectionTitle>
          <dl className="divide-y">
            <KV label="effective trust" className="items-center">
              <div className="w-40">
                <ScoreBar value={w.trust} />
              </div>
            </KV>
            <KV label="soft score">{w.soft.toFixed(3)}</KV>
            <KV label="reputation">
              {w.reputation === null ? (
                <span className="text-muted-foreground">no history · bootstrap</span>
              ) : (
                w.reputation.toFixed(2)
              )}
            </KV>
            <KV label="reputation (confident)">{pct(w.reputationConfident, 1)}</KV>
            <KV label="age factor">{w.ageFactor.toFixed(2)}</KV>
            <KV label="voucher trust">{w.voucherTrust.toFixed(2)}</KV>
            <KV label="stake factor">{w.stakeFactor.toFixed(2)}</KV>
            <KV label="penalty">
              {w.penalty > 0 ? (
                <span className="text-destructive">−{w.penalty.toFixed(2)}</span>
              ) : (
                "0.00"
              )}
            </KV>
            <KV label="exploration bonus">+{w.explorationBonus.toFixed(2)}</KV>
          </dl>

          {/* the α/β/γ/δ-weighted contributions */}
          <div className="mt-3 space-y-2.5 rounded-lg border p-3">
            <TermBar
              label="reputation"
              weight={`×α ${alpha}`}
              raw={repRaw}
              contribution={alpha * repRaw}
            />
            <TermBar
              label="age"
              weight={`×β ${beta}`}
              raw={w.ageFactor}
              contribution={beta * w.ageFactor}
            />
            <TermBar
              label="voucher"
              weight={`×γ ${gamma}`}
              raw={w.voucherTrust}
              contribution={gamma * w.voucherTrust}
            />
            <TermBar
              label="stake"
              weight={`×δ ${delta}`}
              raw={w.stakeFactor}
              contribution={delta * w.stakeFactor}
            />
            {w.penalty > 0 ? (
              <TermBar
                label="penalty"
                weight="−"
                raw={w.penalty}
                contribution={w.penalty}
                tone="neg"
              />
            ) : null}
          </div>

          <div className="mt-2 flex items-start gap-2 rounded-lg border p-2.5">
            <AttestationBadge level={w.attestation} />
            <p className="text-muted-foreground text-xs leading-relaxed">
              Attestation is a hard gate applied before the weighted sum —{" "}
              {ATTESTATION_MEANING[w.attestation]}.
            </p>
          </div>
        </div>

        {/* Capacity */}
        <div>
          <SectionTitle>Capacity</SectionTitle>
          <dl className="divide-y">
            <KV label="donated RAM">{bytes(w.donatedMemBytes, 0)}</KV>
            <KV label="threads">{w.totalThreads}</KV>
            <KV label="max concurrent jobs">{w.maxJobs}</KV>
          </dl>
        </div>

        {/* Economics */}
        <div>
          <SectionTitle>Economics</SectionTitle>
          <dl className="divide-y">
            <KV label="stake bonded">{`${num(w.stakeTon)} TON`}</KV>
            <KV label="stake factor">{w.stakeFactor.toFixed(2)}</KV>
            <KV label="success rate">{pct(w.successRate, 1)}</KV>
            <KV label="correct / faults">
              <span className="tabular-nums">
                {w.correct}
                <span className="text-muted-foreground"> / </span>
                <span className={w.faults > 0 ? "text-destructive" : undefined}>
                  {w.faults}
                </span>
              </span>
            </KV>
          </dl>
        </div>
      </div>
    </>
  );
}

/* -------------------------------------------------------------------- interactive section */

export function WorkersClient() {
  // LIVE: worker trust/reputation/capacity/observations stream in realtime as
  // ambient jobs run (the cheat/fail nodes' trust really drops); falls back to
  // the baked snapshot when the backend is offline.
  const { workers, connected } = useLive();
  const [query, setQuery] = React.useState("");
  const [sort, setSort] = React.useState<SortKey>("trust");
  // Pin the open worker by id (not by object) so the Sheet always reflects the
  // latest live values for that worker as the stream updates.
  const [selectedId, setSelectedId] = React.useState<string | null>(null);

  const total = workers.length;

  const q = query.trim().toLowerCase();
  const filtered = workers.filter((w) =>
    q === ""
      ? true
      : w.alias.toLowerCase().includes(q) || w.id.toLowerCase().includes(q)
  );
  const rows = sortWorkers(filtered, sort);

  // Resolve the open worker from the live list by id; if it has dropped out of
  // the current list the Sheet closes (selected is null).
  const selected =
    selectedId != null ? workers.find((w) => w.id === selectedId) ?? null : null;

  return (
    <>
      {/* Controls */}
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div className="relative w-full sm:max-w-xs">
          <Search className="text-muted-foreground absolute left-2.5 top-1/2 size-4 -translate-y-1/2" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search alias or node id…"
            className="pl-8"
          />
        </div>
        <div className="flex items-center gap-2">
          <span className="text-muted-foreground text-sm">Sort by</span>
          <Select value={sort} onValueChange={(v) => setSort(v as SortKey)}>
            <SelectTrigger className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SORTS.map((s) => (
                <SelectItem key={s.key} value={s.key}>
                  {s.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      {/* Table */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            Directory
            <span className="text-muted-foreground text-sm font-normal tabular-nums">
              {rows.length} of {total}
            </span>
            {connected ? (
              <Badge variant="ok">live</Badge>
            ) : (
              <Badge variant="muted">snapshot</Badge>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Worker</TableHead>
                <TableHead>ID</TableHead>
                <TableHead>Attestation</TableHead>
                <TableHead>Trust</TableHead>
                <TableHead className="text-right">Reputation</TableHead>
                <TableHead className="text-right">Stake</TableHead>
                <TableHead>Capacity</TableHead>
                <TableHead className="text-right">Obs</TableHead>
                <TableHead className="pr-6 text-right">p50</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.length === 0 ? (
                <TableRow>
                  <TableCell
                    colSpan={9}
                    className="text-muted-foreground py-8 text-center text-sm"
                  >
                    No workers match &ldquo;{query}&rdquo;.
                  </TableCell>
                </TableRow>
              ) : (
                rows.map((w) => {
                  const badge = BEHAVIOR_BADGE[w.behavior];
                  return (
                    <TableRow
                      key={w.id}
                      role="button"
                      tabIndex={0}
                      aria-label={`Open details for ${w.alias}`}
                      onClick={() => setSelectedId(w.id)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          setSelectedId(w.id);
                        }
                      }}
                      className="cursor-pointer focus-visible:ring-ring/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset"
                    >
                      <TableCell className="pl-6">
                        <span className="flex items-center gap-1.5 text-sm font-medium">
                          <Dot status={w.online ? "ok" : "muted"} />
                          {w.alias}
                          {badge ? (
                            <Badge variant={badge.variant} className="ml-0.5">
                              {badge.label}
                            </Badge>
                          ) : null}
                        </span>
                      </TableCell>
                      <TableCell onClick={(e) => e.stopPropagation()}>
                        <CopyId value={w.id} />
                      </TableCell>
                      <TableCell>
                        <AttestationBadge level={w.attestation} />
                      </TableCell>
                      <TableCell>
                        <div className="w-28">
                          <ScoreBar value={w.trust} />
                        </div>
                      </TableCell>
                      <TableCell className="text-muted-foreground text-right tabular-nums">
                        {pct(w.reputationConfident)}
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {`${num(w.stakeTon)} TON`}
                      </TableCell>
                      <TableCell>
                        <div className="flex flex-col">
                          <span className="text-xs tabular-nums">
                            {w.totalThreads} thr ·{" "}
                            {bytes(w.donatedMemBytes, 0)}
                          </span>
                          <span className="text-muted-foreground text-xs tabular-nums">
                            max {w.maxJobs} jobs
                          </span>
                        </div>
                      </TableCell>
                      <TableCell className="text-right tabular-nums">
                        {num(w.observations)}
                      </TableCell>
                      <TableCell className="pr-6 text-right tabular-nums">
                        {ms(w.p50LatencyMs)}
                      </TableCell>
                    </TableRow>
                  );
                })
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <p className="text-muted-foreground flex items-center justify-center gap-1.5 text-center text-xs">
        <Wallet className="size-3.5" />
        Click any row for the full real trust breakdown, capacity, and economics.
      </p>

      {/* Detail sheet */}
      <Sheet open={!!selected} onOpenChange={(o) => !o && setSelectedId(null)}>
        <SheetContent side="right" className="w-[min(92vw,30rem)] sm:max-w-none">
          {selected ? <WorkerSheet w={selected} /> : null}
        </SheetContent>
      </Sheet>
    </>
  );
}
