"use client";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { NetworkGraph, PALETTE } from "@/components/common/plotly";
import { commGraph } from "@/lib/data";
import { Waypoints } from "lucide-react";

const dispatch = commGraph.edges.filter((e) => e.kind === "dispatch").length;
const quorum = commGraph.edges.filter((e) => e.kind === "quorum").length;

const LEGEND: { c: string; t: string }[] = [
  { c: PALETTE.blue, t: "requester" },
  { c: PALETTE.emerald, t: "honest worker" },
  { c: PALETTE.amber, t: "failing node" },
  { c: PALETTE.red, t: "cheating node" },
];

export function CommGraphCard() {
  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-3">
          <div>
            <CardTitle className="flex items-center gap-2">
              <Waypoints className="size-4 text-primary" /> Node communications (circular)
            </CardTitle>
            <CardDescription>
              Real message graph from this run — requesters dispatch to workers (blue edges); workers that
              returned the agreed quorum hash on the same job are linked (green edges). Node size = link count.
            </CardDescription>
          </div>
          <div className="hidden gap-4 text-right sm:flex">
            <div>
              <div className="text-muted-foreground text-xs">nodes</div>
              <div className="text-lg font-semibold tabular-nums">{commGraph.nodes.length}</div>
            </div>
            <div>
              <div className="text-muted-foreground text-xs">dispatch</div>
              <div className="text-lg font-semibold tabular-nums text-[var(--info)]">{dispatch}</div>
            </div>
            <div>
              <div className="text-muted-foreground text-xs">quorum</div>
              <div className="text-lg font-semibold tabular-nums text-[var(--ok)]">{quorum}</div>
            </div>
          </div>
        </div>
      </CardHeader>
      <CardContent>
        <NetworkGraph nodes={commGraph.nodes} edges={commGraph.edges} height={480} />
        <div className="mt-2 flex flex-wrap items-center justify-center gap-x-4 gap-y-1">
          {LEGEND.map((l) => (
            <span key={l.t} className="flex items-center gap-1.5 text-xs">
              <span className="size-2.5 rounded-full" style={{ background: l.c }} />
              <span className="text-muted-foreground">{l.t}</span>
            </span>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
