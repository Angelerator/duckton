"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Box, Lines, PALETTE } from "@/components/common/plotly";
import { receipts, transport, workers } from "@/lib/data";
import { Gauge, Timer } from "lucide-react";

// Per-worker correct-receipt latency distributions (real).
const latByWorker = workers
  .map((w) => ({
    name: w.alias,
    y: receipts.filter((r) => r.workerId === w.id && r.verdict === "Correct").map((r) => r.latencyMs),
    color: w.attestation === "L2" ? PALETTE.emerald : w.attestation === "L1" ? PALETTE.blue : PALETTE.slate,
  }))
  .filter((g) => g.y.length > 0);

const sweep = transport.bench.sweep;

export function TransportPlots() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Timer className="size-4 text-primary" /> Per-worker latency (plotly box)
          </CardTitle>
          <CardDescription>Distribution of commit latencies per worker across the run.</CardDescription>
        </CardHeader>
        <CardContent>
          <Box groups={latByWorker} yTitle="ms" height={320} />
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Gauge className="size-4 text-primary" /> Throughput vs parallelism (plotly)
          </CardTitle>
          <CardDescription>Measured loopback transfer — doubles 1→2, then plateaus (BDP).</CardDescription>
        </CardHeader>
        <CardContent>
          <Lines
            x={sweep.map((s) => s.parallelism)}
            series={[
              { name: "MB/s", y: sweep.map((s) => s.mbPerSec), color: PALETTE.emerald },
              { name: "p50 ms", y: sweep.map((s) => s.p50Ms), color: PALETTE.amber },
            ]}
            yTitle=""
            height={320}
          />
        </CardContent>
      </Card>
    </div>
  );
}
