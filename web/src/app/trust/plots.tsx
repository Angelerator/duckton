"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Histogram, PALETTE, Radar } from "@/components/common/plotly";
import { receipts, workers } from "@/lib/data";
import { ShieldCheck, Timer } from "lucide-react";

const honest = [...workers].filter((w) => w.behavior === "honest").sort((a, b) => b.trust - a.trust);
const top = honest[0];
const bad = workers.find((w) => w.behavior !== "honest");

const CATS = ["reputation", "age", "voucher", "stake", "success", "soft"];
const terms = (w: (typeof workers)[number]) => [
  w.reputationConfident,
  w.ageFactor,
  w.voucherTrust,
  w.stakeFactor,
  w.successRate,
  w.soft,
];

const latencies = receipts.filter((r) => r.verdict === "Correct").map((r) => r.latencyMs);

export function TrustPlots() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <ShieldCheck className="size-4 text-primary" /> Trust terms — radar (plotly)
          </CardTitle>
          <CardDescription>
            The real soft-score inputs for the top worker vs. a penalized node — the trust engine&apos;s actual
            per-term values.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Radar
            categories={CATS}
            series={[
              top ? { name: top.alias, values: terms(top), color: PALETTE.emerald } : null,
              bad ? { name: `${bad.alias} (${bad.behavior})`, values: terms(bad), color: PALETTE.red } : null,
            ].filter(Boolean) as { name: string; values: number[]; color?: string }[]}
            height={340}
          />
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Timer className="size-4 text-primary" /> Verified-result latency (plotly histogram)
          </CardTitle>
          <CardDescription>Commit-first latency across every Correct receipt this run.</CardDescription>
        </CardHeader>
        <CardContent>
          <Histogram values={latencies} xTitle="ms" color={PALETTE.teal} height={340} />
        </CardContent>
      </Card>
    </div>
  );
}
