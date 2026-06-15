import Link from "next/link";
import {
  Activity,
  ArrowRight,
  Coins,
  Cpu,
  Gauge,
  ServerCog,
  ShieldCheck,
  Zap,
} from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
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
  PageHeader,
  ScoreBar,
  SectionTitle,
  Stat,
  StatusBadge,
} from "@/components/common/atoms";
import { AreaTrend, BarMini, Donut } from "@/components/common/charts";
import { Explainer } from "@/components/common/explain";
import { CopyId } from "@/components/common/copy";
import { jobs, meta, overview, workers } from "@/lib/data";
import { OverviewPlots } from "./overview-plots";
import { ago, bytes, ms, num, pct } from "@/lib/format";

const lifecycle = [
  { stage: "Offer", note: "broadcast query_hash" },
  { stage: "Bid", note: "ETA + attestation" },
  { stage: "Dispatch", note: "SQL + scoped creds" },
  { stage: "Commit", note: "result_hash first" },
  { stage: "Verify", note: "quorum agreement" },
  { stage: "Settle", note: "pay + RESET losers" },
];

export default function OverviewPage() {
  const verifyRate = overview.jobsRun ? overview.verified / overview.jobsRun : 0;
  const topWorkers = [...workers].sort((a, b) => b.trust - a.trust).slice(0, 5);

  return (
    <div className="space-y-8">
      <PageHeader
        title="Overview"
        description="A decentralized, many-host DuckDB grid. Requesters broadcast a query; several hosts run it redundantly over QUIC; the first correct result that reaches quorum wins."
        icon={<Activity />}
      >
        <Button asChild variant="outline">
          <Link href="/jobs">
            View jobs <ArrowRight />
          </Link>
        </Button>
        <Button asChild>
          <Link href="/query">
            <Zap /> New query
          </Link>
        </Button>
      </PageHeader>

      <Explainer
        what="A live snapshot of the whole grid: how many machines are sharing compute right now, how many queries ran, how fast results came back, and how trustworthy the workers are."
        impact="A high verified-rate and average trust mean you can rely on results without trusting any single machine — the grid cross-checks itself."
      />

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-6">
        <Stat
          label="Workers online"
          value={`${overview.workersOnline}/${overview.workersTotal}`}
          sub="loopback grid hosts"
          icon={<ServerCog />}
          accent="ok"
        />
        <Stat
          label="Jobs run"
          value={num(overview.jobsRun)}
          sub="this grid run"
          icon={<Activity />}
          accent="info"
        />
        <Stat
          label="Verified"
          value={`${overview.verified}/${overview.jobsRun}`}
          sub={`${pct(verifyRate, 1)} quorum-agreed`}
          icon={<ShieldCheck />}
          accent="primary"
          hint="Share of jobs where enough workers returned the same answer (quorum) — your correctness signal."
        />
        <Stat
          label="Avg trust"
          value={overview.avgTrust.toFixed(2)}
          sub="effective_trust ∈ [0,1]"
          icon={<ShieldCheck />}
          accent="primary"
          hint="Average 0–1 trustworthiness the grid computes per worker from its history, stake and hardware tier."
        />
        <Stat
          label="Donated RAM"
          value={bytes(overview.freeMemBytes, 0)}
          sub="pooled across hosts"
          icon={<Cpu />}
          accent="info"
          hint="Memory volunteered across all hosts — the shared capacity your queries draw on."
        />
        <Stat
          label="Total staked"
          value={`${num(overview.totalStakeTon)} TON`}
          sub="stake at risk"
          icon={<Coins />}
          accent="warn"
        />
      </div>

      {/* Result latency per job */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div>
              <CardTitle className="flex items-center gap-2">
                <Gauge className="size-4 text-primary" /> Result latency per job (real loopback run)
              </CardTitle>
              <CardDescription>
                Real measured per-job result latency from the in-process grid run — commit-first
                timing of the winning result.
              </CardDescription>
            </div>
            <div className="hidden gap-4 text-right sm:flex">
              <div>
                <div className="text-muted-foreground text-xs">verified</div>
                <div className="text-lg font-semibold tabular-nums text-[var(--ok)]">
                  {overview.verified}
                </div>
              </div>
              <div>
                <div className="text-muted-foreground text-xs">failed</div>
                <div className="text-lg font-semibold tabular-nums text-destructive">
                  {overview.failed}
                </div>
              </div>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          <AreaTrend
            data={overview.series}
            series={[{ key: "latencyMs", color: "var(--chart-1)", label: "latency (ms)" }]}
            unit="ms"
          />
        </CardContent>
      </Card>

      {/* Two charts */}
      <div className="grid gap-4 lg:grid-cols-5">
        <Card className="lg:col-span-3">
          <CardHeader>
            <CardTitle>Result latency distribution</CardTitle>
            <CardDescription>per-result commit timing across the run</CardDescription>
          </CardHeader>
          <CardContent>
            <BarMini data={overview.latencyHistogram} xKey="bucket" yKey="count" />
          </CardContent>
        </Card>
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>Attestation mix</CardTitle>
            <CardDescription>worker hardware-trust tiers</CardDescription>
          </CardHeader>
          <CardContent>
            <Donut
              data={overview.attestationMix.map((a) => ({
                name: a.level,
                value: a.count,
                fill: a.fill,
              }))}
            />
            <div className="mt-2 space-y-1">
              {overview.attestationMix.map((a) => (
                <div key={a.level} className="flex items-center gap-2 text-xs">
                  <span className="size-2.5 rounded-full" style={{ background: a.fill }} />
                  <span className="text-muted-foreground">{a.level}</span>
                  <span className="ml-auto font-medium tabular-nums">{a.count}</span>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      </div>

      {/* Lifecycle strip */}
      <div>
        <SectionTitle
          hint="happy path"
          info="The stages a query passes through: workers bid, the SQL is dispatched, each commits a result fingerprint, the grid checks agreement, then pays the winner."
        >
          Request lifecycle
        </SectionTitle>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6">
          {lifecycle.map((s, i) => (
            <div key={s.stage} className="bg-card relative rounded-lg border p-3">
              <div className="text-muted-foreground/60 absolute right-2 top-2 font-mono text-xs">
                {i + 1}
              </div>
              <div className="text-primary text-sm font-semibold">{s.stage}</div>
              <div className="text-muted-foreground mt-0.5 text-xs">{s.note}</div>
            </div>
          ))}
        </div>
      </div>

      {/* Recent jobs + top workers */}
      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="xl:col-span-2">
          <CardHeader>
            <div className="flex items-center justify-between">
              <CardTitle>Recent jobs</CardTitle>
              <Button asChild variant="ghost" size="sm">
                <Link href="/jobs">
                  All jobs <ArrowRight />
                </Link>
              </Button>
            </div>
          </CardHeader>
          <CardContent className="px-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-6">Job</TableHead>
                  <TableHead>Class</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="text-right">Rows</TableHead>
                  <TableHead className="text-right">Latency</TableHead>
                  <TableHead className="pr-6 text-right">When</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {jobs.map((j) => (
                  <TableRow key={j.id}>
                    <TableCell className="pl-6">
                      <div className="flex flex-col">
                        <span className="font-mono text-xs">{j.id}</span>
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
                    <TableCell className="text-right tabular-nums">
                      {j.rowCount ? num(j.rowCount) : "—"}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {j.latencyMs ? ms(j.latencyMs) : "—"}
                    </TableCell>
                    <TableCell className="text-muted-foreground pr-6 text-right text-xs">
                      {ago(j.createdAtMs)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <div className="flex items-center justify-between">
              <CardTitle>Top workers</CardTitle>
              <Button asChild variant="ghost" size="sm">
                <Link href="/workers">
                  All <ArrowRight />
                </Link>
              </Button>
            </div>
          </CardHeader>
          <CardContent className="space-y-3">
            {topWorkers.map((w) => (
              <div key={w.id} className="flex items-center gap-3">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate text-sm font-medium">{w.alias}</span>
                    <AttestationBadge level={w.attestation} />
                  </div>
                  <CopyId value={w.id} className="mt-0.5" />
                </div>
                <div className="w-24">
                  <ScoreBar value={w.trust} />
                </div>
              </div>
            ))}
          </CardContent>
        </Card>
      </div>

      <OverviewPlots />

      <p className="text-muted-foreground flex flex-wrap items-center justify-center gap-x-2 gap-y-1 text-center text-xs">
        <Badge variant="ok" className="font-mono">
          real
        </Badge>
        <span>
          Real data from an in-process loopback grid run of the duckdb-p2p crates — no hand-authored
          values. Protocol {meta.protocolVersion} · engine {meta.engineVersion} · workspace{" "}
          {meta.workspaceVersion}.
        </span>
      </p>
    </div>
  );
}
