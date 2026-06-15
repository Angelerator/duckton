"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { HBar, PALETTE } from "@/components/common/plotly";
import { ton } from "@/lib/data";
import { Gauge } from "lucide-react";

const top = ton.gas.slice(0, 10).reverse(); // reverse → largest on top in HBar

export function GasPlot() {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Gauge className="size-4 text-primary" /> Gas baseline (measured, plotly)
        </CardTitle>
        <CardDescription>Average TVM gas per opcode from the Tolk emulator suite.</CardDescription>
      </CardHeader>
      <CardContent>
        <HBar y={top.map((g) => g.op)} x={top.map((g) => g.avgGas)} color={PALETTE.amber} xTitle="avg gas" height={360} />
      </CardContent>
    </Card>
  );
}
