"use client";

import * as React from "react";
import {
  Coins,
  Database,
  Table2,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
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
  KV,
  SectionTitle,
  StatusBadge,
  VerdictBadge,
} from "@/components/common/atoms";
import { CopyId } from "@/components/common/copy";
import { jobs } from "@/lib/data";
import { ago, ms, num, ton } from "@/lib/format";
import type { CandidateState, Job, JobCandidate } from "@/lib/types";

/* ----------------------------------------------------------------- helpers */

type Filter = "all" | "verified" | "settled" | "failed";

const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "All" },
  { key: "verified", label: "Verified" },
  { key: "settled", label: "Settled" },
  { key: "failed", label: "Failed" },
];

// Per-candidate-state visual treatment for the hedged-run cards. These map the
// REAL terminal states the coordinator recorded for each redundant worker.
const CAND_STATE: Record<
  CandidateState,
  { variant: Parameters<typeof Badge>[0]["variant"]; label: string; note: string }
> = {
  won: { variant: "ok", label: "won", note: "first to commit the agreed hash" },
  committed: {
    variant: "info",
    label: "committed",
    note: "committed a hash (see verdict)",
  },
  reset: { variant: "muted", label: "reset", note: "agreed but lost the race — RESET" },
  dispatched: {
    variant: "muted",
    label: "dispatched",
    note: "no usable commit in time",
  },
  bidding: { variant: "muted", label: "bidding", note: "bid pending" },
  rejected: { variant: "destructive", label: "rejected", note: "bid rejected" },
};

// Map a timeline stage to a badge variant so the rail reads at a glance.
const STAGE_VARIANT: Record<string, Parameters<typeof Badge>[0]["variant"]> = {
  offer: "muted",
  bidding: "muted",
  dispatch: "info",
  executing: "info",
  commit: "secondary",
  verify: "ok",
  settle: "ok",
  anchor: "secondary",
};

function CandidateCard({ c, isWinner }: { c: JobCandidate; isWinner: boolean }) {
  const meta = CAND_STATE[c.state];
  // A divergent commit (the real cheater) is "committed" but verdict !== Correct.
  const divergent = c.state === "committed" && c.verdict !== "Correct";
  const dimmed = c.state === "reset" || c.state === "dispatched";
  return (
    <div
      className={cn(
        "rounded-lg border p-3 transition-colors",
        isWinner
          ? "border-[var(--ok)]/40 bg-[var(--ok)]/5 ring-1 ring-[var(--ok)]/30"
          : divergent
            ? "border-destructive/30 bg-destructive/5"
            : "bg-card",
        dimmed && "opacity-70"
      )}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span
              className={cn(
                "truncate text-sm font-medium",
                c.state === "reset" && "text-muted-foreground"
              )}
            >
              {c.alias}
            </span>
            <AttestationBadge level={c.attestation} />
            <VerdictBadge verdict={c.verdict} />
          </div>
          <div className="text-muted-foreground mt-1.5 text-xs">{meta.note}</div>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          <Badge variant={meta.variant} className={cn(isWinner && "font-semibold")}>
            {isWinner ? "★ " : ""}
            {meta.label}
          </Badge>
          <div className="text-muted-foreground text-xs tabular-nums">
            ETA {ms(c.etaMs)} · {c.price === 0 ? "free" : `${num(c.price)} TON`}
          </div>
        </div>
      </div>

      <div className="mt-2.5">
        <Progress
          value={c.progressPct}
          indicatorClassName={
            isWinner
              ? "bg-[var(--ok)]"
              : divergent
                ? "bg-[var(--destructive)]"
                : undefined
          }
        />
        <div className="text-muted-foreground mt-1 flex items-center justify-between text-xs tabular-nums">
          <span>{c.progressPct}% scanned</span>
          {c.committedHash ? (
            <span className="flex items-center gap-2">
              <CopyId value={c.committedHash} />
              {c.commitLatencyMs ? <span>· {ms(c.commitLatencyMs)}</span> : null}
            </span>
          ) : (
            <span>no commit</span>
          )}
        </div>
      </div>
    </div>
  );
}

function JobDetail({ job }: { job: Job }) {
  const committed = job.candidates.filter(
    (c) => c.state === "committed" || c.state === "won" || c.state === "reset"
  ).length;
  const quorumReached =
    job.status === "verified" || job.status === "settled" || committed >= job.quorum;
  const cols = job.result.columns;
  const previewRows = job.result.rows.slice(0, 8);

  return (
    <Card className="lg:sticky lg:top-20">
      <CardHeader>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <CardTitle className="font-mono text-base">{job.id}</CardTitle>
            <StatusBadge status={job.status} />
          </div>
          <CopyId value={job.id} />
        </div>
        <div className="mt-1 flex flex-wrap items-center gap-2 text-sm">
          <Badge variant="outline" className="font-mono">
            {job.fn}
          </Badge>
          <DataClassBadge value={job.dataClass} />
          <Badge variant={job.verifyMode === "Quorum" ? "info" : "muted"}>
            {job.verifyMode}
          </Badge>
          <span className="text-muted-foreground">
            by <span className="text-foreground font-medium">{job.requester}</span>
          </span>
          {job.paid ? (
            <span className="text-muted-foreground flex items-center gap-1">
              <Coins className="size-3.5 text-[var(--warn)]" />
              <span className="text-foreground tabular-nums">{ton(job.escrowTon)}</span>{" "}
              escrow
            </span>
          ) : (
            <Badge variant="muted">free tier</Badge>
          )}
          <span className="text-muted-foreground ml-auto text-xs">
            {ago(job.createdAtMs)}
          </span>
        </div>
      </CardHeader>

      <CardContent className="space-y-6">
        {/* SQL */}
        <div>
          <SectionTitle>Query</SectionTitle>
          <pre className="bg-muted/40 overflow-x-auto rounded-lg border p-3 font-mono text-xs leading-relaxed">
            {job.sql}
          </pre>
          <div className="text-muted-foreground mt-2 flex items-center gap-1.5 text-xs">
            <Database className="size-3.5" />
            <span className="font-mono">{job.source}</span>
          </div>
        </div>

        <Separator />

        {/* Hedged execution */}
        <div>
          <div className="mb-3 flex items-center justify-between gap-2">
            <SectionTitle className="mb-0">Hedged execution</SectionTitle>
            <div className="flex items-center gap-2 text-xs">
              <span className="text-muted-foreground tabular-nums">
                quorum {job.quorum} of {job.k}
              </span>
              <Badge variant={quorumReached ? "ok" : "destructive"}>
                {quorumReached ? "quorum reached" : "no quorum"}
              </Badge>
            </div>
          </div>
          {job.candidates.length > 0 ? (
            <div className="space-y-2">
              {job.candidates.map((c) => (
                <CandidateCard
                  key={c.workerId}
                  c={c}
                  isWinner={c.workerId === job.winnerId || c.state === "won"}
                />
              ))}
            </div>
          ) : (
            <div className="text-muted-foreground rounded-lg border border-dashed p-3 text-xs">
              Paid-pool job — settled directly to the escrow split; per-candidate
              hedging detail is captured in the receipts/settlement records.
            </div>
          )}
        </div>

        <Separator />

        {/* Timeline */}
        <div>
          <SectionTitle hint="ms offset from start">Timeline</SectionTitle>
          <ol className="relative ml-1 space-y-4 border-l pl-5">
            {job.timeline.map((e) => (
              <li key={`${e.tMs}-${e.stage}`} className="relative">
                <span className="bg-background absolute -left-[26px] top-0.5 flex size-3 items-center justify-center rounded-full border">
                  <span className="bg-primary size-1.5 rounded-full" />
                </span>
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-muted-foreground w-14 shrink-0 font-mono text-xs tabular-nums">
                    {ms(e.tMs)}
                  </span>
                  <Badge
                    variant={STAGE_VARIANT[e.stage] ?? "muted"}
                    className="font-mono"
                  >
                    {e.stage}
                  </Badge>
                  <span className="text-sm font-medium">{e.label}</span>
                </div>
                {e.detail ? (
                  <div className="text-muted-foreground mt-0.5 pl-16 text-xs break-all">
                    {e.detail}
                  </div>
                ) : null}
              </li>
            ))}
          </ol>
        </div>

        <Separator />

        {/* Result */}
        <div>
          <SectionTitle>Result</SectionTitle>
          <dl className="divide-y">
            <KV label="result_hash">
              {job.resultHash ? <CopyId value={job.resultHash} /> : "—"}
            </KV>
            <KV label="row_count">{num(job.rowCount)}</KV>
            <KV label="latency">{job.latencyMs ? ms(job.latencyMs) : "—"}</KV>
            <KV label="winner">{job.winner ?? "—"}</KV>
            <KV label="k (dispatched)">{job.k}</KV>
            <KV label="quorum">{job.quorum}</KV>
          </dl>

          {/* Real result preview (string[][] straight from the engine). */}
          {cols.length > 0 && previewRows.length > 0 ? (
            <div className="mt-3">
              <div className="text-muted-foreground mb-1.5 flex items-center gap-1.5 text-xs">
                <Table2 className="size-3.5" />
                Result preview
                <span className="tabular-nums">
                  · {previewRows.length} of {num(job.rowCount)} rows
                </span>
              </div>
              <div className="overflow-x-auto rounded-lg border">
                <table className="w-full text-xs">
                  <thead className="bg-muted/40">
                    <tr>
                      {cols.map((col) => (
                        <th
                          key={col}
                          className="text-muted-foreground px-3 py-1.5 text-left font-mono font-medium"
                        >
                          {col}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {previewRows.map((row, ri) => (
                      <tr key={ri} className="border-t">
                        {row.map((cell, ci) => (
                          <td
                            key={ci}
                            className="px-3 py-1.5 font-mono tabular-nums"
                          >
                            {cell}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          ) : null}
        </div>
      </CardContent>
    </Card>
  );
}

/* -------------------------------------------------------------------- interactive section */

export function JobsClient() {
  const [filter, setFilter] = React.useState<Filter>("all");
  const [selectedId, setSelectedId] = React.useState<string>(jobs[0].id);

  const filtered = jobs.filter((j) =>
    filter === "all" ? true : j.status === filter
  );

  // Keep selection valid for the active filter, but stay deterministic on first render.
  const selected =
    filtered.find((j) => j.id === selectedId) ??
    jobs.find((j) => j.id === selectedId) ??
    jobs[0];

  return (
    <>
      {/* Filter */}
      <div className="flex flex-wrap items-center gap-2">
        {FILTERS.map((f) => {
          const count =
            f.key === "all"
              ? jobs.length
              : jobs.filter((j) => j.status === f.key).length;
          return (
            <Button
              key={f.key}
              size="sm"
              variant={filter === f.key ? "default" : "outline"}
              onClick={() => setFilter(f.key)}
            >
              {f.label}
              <span
                className={cn(
                  "ml-1 tabular-nums",
                  filter === f.key
                    ? "text-primary-foreground/70"
                    : "text-muted-foreground"
                )}
              >
                {count}
              </span>
            </Button>
          );
        })}
      </div>

      {/* Master / detail */}
      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)]">
        {/* LEFT — list */}
        <Card className="self-start">
          <CardHeader>
            <CardTitle>Job queue</CardTitle>
            <CardDescription>
              {filtered.length} {filter === "all" ? "total" : filter} · select a row
              to inspect its hedged run
            </CardDescription>
          </CardHeader>
          <CardContent className="px-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-6">Job</TableHead>
                  <TableHead>Class</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="pr-6 text-right">When</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.length === 0 ? (
                  <TableRow>
                    <TableCell
                      colSpan={4}
                      className="text-muted-foreground py-8 text-center text-sm"
                    >
                      No {filter} jobs.
                    </TableCell>
                  </TableRow>
                ) : (
                  filtered.map((j) => {
                    const isSel = j.id === selected.id;
                    return (
                      <TableRow
                        key={j.id}
                        data-state={isSel ? "selected" : undefined}
                        onClick={() => setSelectedId(j.id)}
                        className={cn(
                          "cursor-pointer",
                          isSel && "border-l-primary border-l-2 bg-muted/60"
                        )}
                      >
                        <TableCell className="pl-6">
                          <div className="flex flex-col">
                            <span className="flex items-center gap-1.5 font-mono text-xs">
                              {j.paid ? (
                                <Coins className="size-3 text-[var(--warn)]" />
                              ) : null}
                              {j.id.slice(0, 12)}
                            </span>
                            <span className="text-muted-foreground text-xs">
                              {j.fn} · k={j.k} q={j.quorum}
                            </span>
                          </div>
                        </TableCell>
                        <TableCell>
                          <DataClassBadge value={j.dataClass} />
                        </TableCell>
                        <TableCell>
                          <StatusBadge status={j.status} />
                        </TableCell>
                        <TableCell className="text-muted-foreground pr-6 text-right text-xs">
                          {ago(j.createdAtMs)}
                        </TableCell>
                      </TableRow>
                    );
                  })
                )}
              </TableBody>
            </Table>
          </CardContent>
        </Card>

        {/* RIGHT — detail */}
        <JobDetail job={selected} />
      </div>
    </>
  );
}
