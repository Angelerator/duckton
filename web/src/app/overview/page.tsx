"use client";

import Link from "next/link";
import {
  Activity,
  ArrowUpRight,
  Coins,
  Gauge,
  Landmark,
  Network,
  Server,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { PageHeader, Stat } from "@/components/common/atoms";
import { CopyId } from "@/components/common/copy";
import { useRealNet, shortId } from "@/lib/real-net";
import { useOnchain } from "@/lib/onchain";
import { num } from "@/lib/format";

const SEED = "seed.duckton.com:9494";

export default function OverviewPage() {
  const net = useRealNet();
  const { data: chain } = useOnchain();
  const live = net !== null && net.onlineHosts > 0;
  const recent = net?.recent ?? [];

  const fee = chain?.platformFeeBps != null ? `${(chain.platformFeeBps / 100).toFixed(1)}%` : "—";
  const kappa = chain?.participationBps != null ? `${(chain.participationBps / 100).toFixed(1)}%` : "—";
  const bal = chain?.balanceTon != null ? `${chain.balanceTon.toFixed(3)} TON` : "—";

  return (
    <div className="space-y-8">
      <PageHeader
        title="Network"
        description="Live status of the Duckton grid — independent nodes running verified distributed queries over QUIC, settled on TON. Everything here is read live; nothing is simulated."
        icon={<Network />}
      >
        <Badge variant={live ? "ok" : "muted"}>{live ? "live" : "connecting…"}</Badge>
      </PageHeader>

      {/* Live real-network stats */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat label="Online nodes" value={`${net?.onlineHosts ?? "—"}`} sub="independent hosts" icon={<Server />} accent="ok" />
        <Stat label="Jobs executed" value={net ? num(net.realJobsRun) : "—"} sub="distributed + verified" icon={<Activity />} accent="info" />
        <Stat
          label="Verified"
          value={net?.verifiedRatePct != null ? `${net.verifiedRatePct}%` : "—"}
          sub="quorum-agreed"
          icon={<ShieldCheck />}
          accent="primary"
          hint="Share of jobs where a quorum of nodes returned the same byte-for-byte result."
        />
        <Stat label="Avg latency" value={net?.avgLatencyMs != null ? `${net.avgLatencyMs} ms` : "—"} sub="cross-node commit" icon={<Gauge />} accent="info" />
      </div>

      {/* Host nodes + recent jobs */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Server className="text-primary size-4" /> Host nodes
            </CardTitle>
            <CardDescription>Independent node processes (Ed25519 identities) currently serving jobs.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {(net?.hosts ?? []).map((h) => (
              <div key={h} className="flex items-center gap-2">
                <span className="size-1.5 rounded-full bg-[var(--ok)]" />
                <CopyId value={h} display={shortId(h)} />
              </div>
            ))}
            {!net || net.hosts.length === 0 ? <p className="text-muted-foreground text-sm">connecting…</p> : null}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Activity className="text-primary size-4" /> Recent jobs
            </CardTitle>
            <CardDescription>Live distributed queries executed across the nodes.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1.5">
            {recent.slice(0, 6).map((j, i) => (
              <div key={i} className="flex items-center justify-between gap-3 font-mono text-xs">
                <span className="text-muted-foreground truncate">query #{j.queryHash}</span>
                <span className="shrink-0">
                  <span className="text-primary">{shortId(j.winner)}</span> · {j.latencyMs}ms · q{j.participants}
                </span>
              </div>
            ))}
            {recent.length === 0 ? <p className="text-muted-foreground text-sm">connecting…</p> : null}
          </CardContent>
        </Card>
      </div>

      {/* On-chain settlement (live mainnet) */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Landmark className="text-primary size-4" /> On-chain settlement — TON mainnet
            {chain?.status === "active" ? <Badge variant="ok">active</Badge> : <Badge variant="muted">reading…</Badge>}
          </CardTitle>
          <CardDescription>
            Paid jobs settle through the platform-wide <span className="font-mono">GlobalParams</span> contract — read
            live from TON.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
            <Stat label="Platform fee φ" value={fee} sub="of escrow" icon={<Coins />} accent="primary" />
            <Stat label="Participation κ" value={kappa} sub="per verifier" icon={<ShieldCheck />} accent="info" />
            <Stat label="Params version" value={chain?.paramsVersion != null ? `v${chain.paramsVersion}` : "—"} sub="on-chain" icon={<Activity />} accent="info" />
            <Stat label="Contract balance" value={bal} sub="GlobalParams" icon={<Landmark />} accent="ok" />
          </div>
          {chain ? (
            <div className="flex items-center gap-2 text-xs">
              <CopyId value={chain.address} display={shortId(chain.address)} />
              <a
                href={chain.explorer}
                target="_blank"
                rel="noreferrer"
                className="text-muted-foreground hover:text-primary inline-flex items-center gap-1"
              >
                Tonviewer <ArrowUpRight className="size-3" />
              </a>
            </div>
          ) : null}
        </CardContent>
      </Card>

      {/* Connect quickstart */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Terminal className="text-primary size-4" /> Connect
          </CardTitle>
          <CardDescription>Join the public network and run a verified query — all in plain SQL.</CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="bg-muted/50 overflow-x-auto rounded-lg border p-4 text-xs leading-relaxed">
            <code>{`INSTALL duckton FROM community;
LOAD duckton;
CALL p2p_join(bootstrap => ['${SEED}']);
SELECT * FROM p2p_query('SELECT 42 AS x');`}</code>
          </pre>
          <div className="mt-3 flex flex-wrap gap-2">
            <Button asChild variant="outline" size="sm">
              <Link href="/connect">
                More examples <ArrowUpRight />
              </Link>
            </Button>
            <Button asChild variant="outline" size="sm">
              <a href="https://docs.duckton.com" target="_blank" rel="noreferrer">
                Docs <ArrowUpRight />
              </a>
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
