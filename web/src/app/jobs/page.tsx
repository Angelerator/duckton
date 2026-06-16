import type { Metadata } from "next";
import { Ban, Coins, ListChecks, ShieldCheck, Timer } from "lucide-react";
import { PageHeader, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { jobs, meta } from "@/lib/data";
import { ms } from "@/lib/format";
import { JobsClient } from "./jobs-client";

export const metadata: Metadata = {
  title: "Jobs",
  description: "Inspect every job dispatched to the Duckton grid: hedged execution, quorum verification, and settlement.",
};

export default function JobsPage() {
  const total = jobs.length;
  const done = jobs.filter(
    (j) => j.status === "verified" || j.status === "settled"
  ).length;
  const failed = jobs.filter((j) => j.status === "failed").length;
  const paid = jobs.filter((j) => j.paid).length;
  const withLatency = jobs.filter((j) => j.latencyMs > 0);
  const avgLatency =
    withLatency.length === 0
      ? 0
      : withLatency.reduce((a, j) => a + j.latencyMs, 0) / withLatency.length;

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<ListChecks />}
        title="Jobs"
        description="Every job is dispatched redundantly to k workers; the first result that reaches quorum wins, agreeing losers are RESET, and divergent commits are caught."
      />

      <Explainer
        what="Every query becomes a job sent to several workers at once (hedged execution). Each commits a fingerprint of its result first; the fastest answer that reaches quorum wins, and the slower copies are cancelled."
        impact="Racing many workers hides slow or dead ones (speed) and requiring agreement catches wrong ones (correctness) — with no central coordinator."
      />

      <p className="text-muted-foreground -mt-4 text-xs">
        Real jobs executed over loopback QUIC against the in-process Duckton
        grid (engine{" "}
        <span className="text-foreground font-mono">{meta.engineVersion}</span>) —
        no hand-authored runs.
      </p>

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-5">
        <Stat
          label="Total jobs"
          value={total}
          sub="free + paid pool"
          icon={<ListChecks />}
          accent="primary"
        />
        <Stat
          label="Verified · settled"
          value={done}
          sub="quorum-agreed"
          icon={<ShieldCheck />}
          accent="ok"
          hint="Jobs where enough workers returned the same result (quorum), so the answer was accepted and paid out."
        />
        <Stat
          label="Failed"
          value={failed}
          sub="no quorum / timeout"
          icon={<Ban />}
          accent="destructive"
          hint="Jobs that never got enough matching answers or ran out of time — no result was accepted."
        />
        <Stat
          label="Paid"
          value={paid}
          sub="settled with escrow"
          icon={<Coins />}
          accent="warn"
        />
        <Stat
          label="Avg latency"
          value={ms(avgLatency)}
          sub="commit-first, over jobs"
          icon={<Timer />}
          accent="info"
          hint="Wall-clock time from dispatch to an accepted result."
        />
      </div>

      {/* Interactive filter + master/detail — client island */}
      <JobsClient />
    </div>
  );
}
