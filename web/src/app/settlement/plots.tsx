"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Lines, PALETTE, Waterfall } from "@/components/common/plotly";
import { settlement } from "@/lib/data";
import { Coins, TrendingDown } from "lucide-react";

const split = settlement.splits[0];
const commTotal = split ? split.participants.reduce((a, p) => a + p.amountTon, 0) : 0;
const curve = settlement.stakeCurve;

export function SettlementPlots() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Coins className="size-4 text-primary" /> Escrow split (plotly waterfall)
          </CardTitle>
          <CardDescription>
            The real settled paid job — escrow B flows to fee, verifier commissions, and the winner.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {split ? (
            <Waterfall
              labels={["escrow B", "− platform fee", "− commissions", "winner payout"]}
              values={[split.totalTon, -split.platformFeeTon, -commTotal, split.winnerTon]}
              measures={["relative", "relative", "relative", "total"]}
              height={300}
            />
          ) : null}
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <TrendingDown className="size-4 text-primary" /> Stake factor curve (plotly)
          </CardTitle>
          <CardDescription>
            Real diminishing &amp; capped <span className="font-mono">stake_factor</span> — log-scaled between
            min stake and the cap.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Lines
            x={curve.map((p) => p.stakeTon)}
            series={[{ name: "stake_factor", y: curve.map((p) => p.factor), color: PALETTE.violet }]}
            yTitle="factor (0–1)"
            height={300}
          />
        </CardContent>
      </Card>
    </div>
  );
}
