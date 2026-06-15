"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Heatmap, Histogram, PALETTE } from "@/components/common/plotly";
import { jobs, receipts } from "@/lib/data";
import { Activity, Grid3x3 } from "lucide-react";

const latencies = receipts.filter((r) => r.verdict === "Correct").map((r) => r.latencyMs);

// Worker × job commit-latency heatmap (real, from detailed jobs).
const detailed = jobs.filter((j) => j.candidates.length > 0).slice(0, 6);
const aliases = Array.from(new Set(detailed.flatMap((j) => j.candidates.map((c) => c.alias)))).sort();
const z = aliases.map((a) =>
  detailed.map((j) => {
    const c = j.candidates.find((cc) => cc.alias === a);
    return c && c.commitLatencyMs > 0 ? c.commitLatencyMs : null;
  })
) as unknown as number[][];
const x = detailed.map((j) => j.id.replace("job_", ""));

export function OverviewPlots() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Activity className="size-4 text-primary" /> Result latency (plotly histogram)
          </CardTitle>
          <CardDescription>Distribution of every Correct receipt&apos;s commit latency this run.</CardDescription>
        </CardHeader>
        <CardContent>
          <Histogram values={latencies} xTitle="ms" color={PALETTE.blue} height={300} />
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Grid3x3 className="size-4 text-primary" /> Worker × job latency (plotly heatmap)
          </CardTitle>
          <CardDescription>Per-candidate commit latency (ms) — gaps = not selected for that job.</CardDescription>
        </CardHeader>
        <CardContent>
          <Heatmap z={z} x={x} y={aliases} height={300} colorscale="Viridis" />
        </CardContent>
      </Card>
    </div>
  );
}
