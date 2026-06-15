"use client";

import * as React from "react";
import {
  Cpu,
  Gauge,
  Layers,
  Lock,
  Repeat,
  Shield,
  Terminal,
  TrendingUp,
  Zap,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Slider } from "@/components/ui/slider";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { PageHeader, SectionTitle, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { BarMini } from "@/components/common/charts";
import { meta, transport } from "@/lib/data";
import { bytes, ms, num } from "@/lib/format";
import { TransportPlots } from "./plots";

type Cc = "bbr" | "cubic" | "newreno";
type Compression = "none" | "lz4" | "zstd";

/** compression effective-throughput multiplier for compressible result sets */
const COMP_MULT: Record<Compression, number> = {
  none: 1.0,
  lz4: 1.18,
  zstd: 1.34,
};

/** diminishing-returns throughput model, capped by link bandwidth */
function estimateThroughput(
  bandwidthMbps: number,
  parallelism: number,
  comp: Compression
): number {
  const scale = 1 - Math.pow(0.6, parallelism); // 0.4 → 0.99 across 1..8
  const raw = bandwidthMbps * scale * COMP_MULT[comp];
  return Math.min(bandwidthMbps * COMP_MULT[comp], raw);
}

const TUNABLES: { name: string; field: string; desc: string }[] = [
  {
    name: "Wire compression",
    field: "compression.algorithm",
    desc: "none / lz4 / zstd — trade CPU for bytes on the wire (over min_size)",
  },
  {
    name: "Congestion control",
    field: "quic.congestion",
    desc: "bbr · cubic · newreno, with optional packet pacing",
  },
  {
    name: "GSO / GRO offload",
    field: "quic.gso · quic.gro",
    desc: "batch UDP datagrams through the NIC to cut per-packet syscall cost",
  },
  {
    name: "Result-stream parallelism",
    field: "result.parallelism",
    desc: "fan results across multiple uni-streams once over parallel_min_bytes",
  },
  {
    name: "BDP flow-control target",
    field: "quic.bdp",
    desc: "derive send/receive windows from bandwidth × RTT instead of fixed bytes",
  },
  {
    name: "0-RTT / session resumption",
    field: "quic.enable_0rtt",
    desc: "skip a full handshake RTT on reconnect to a known peer",
  },
];

export default function TransportPage() {
  const q = transport.quic;

  // ---- interactive knobs, SEEDED FROM THE REAL [transport] CONFIG ----------
  const [cc, setCc] = React.useState<Cc>(q.congestion as Cc);
  const [comp, setComp] = React.useState<Compression>(
    transport.compression.algorithm as Compression
  );
  const [gso, setGso] = React.useState(q.gso);
  const [gro, setGro] = React.useState(q.gro);
  const [pacing, setPacing] = React.useState(q.pacing);
  const [zeroRtt, setZeroRtt] = React.useState(q.enable_0rtt);
  const [parallelism, setParallelism] = React.useState(
    transport.result.parallelism
  );
  const [bdpOn, setBdpOn] = React.useState(q.bdp.enabled);
  const [bandwidth, setBandwidth] = React.useState(q.bdp.bandwidth_mbps); // Mbps
  const [rtt, setRtt] = React.useState(q.bdp.rtt_ms); // ms

  // BDP flow-control window = bandwidth(bytes/s) × rtt(s)
  const bdpBytes = (bandwidth * 1_000_000 * (rtt / 1000)) / 8;
  // when BDP is off the node uses a fixed send window from config
  const windowBytes = bdpOn ? bdpBytes : q.send_window_bytes;
  const estMbps = estimateThroughput(bandwidth, parallelism, comp);

  // real measured loopback sweep
  const sweep = transport.bench.sweep;
  const best = transport.bench.best;

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Gauge />}
        title="Transport"
        description="QUIC (Quinn + rustls), TLS 1.3 mandatory with mutual auth pinned to Ed25519 node identities. Nothing is readable on the wire. Every figure below is the node's real [transport] config or a real loopback measurement."
      />

      <Explainer
        what="How data actually moves between machines — QUIC, a fast encrypted internet transport (TLS 1.3, mutual auth) — and the knobs that trade latency for throughput."
        impact="Results stream quickly and privately; right-sizing the flow-control window and using parallel streams is what lets a long-distance link reach full speed."
      />

      {/* Stat row — real transport config + measured best */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-6">
        <Stat label="Transport" value="QUIC" sub="Quinn + rustls" icon={<Zap />} accent="primary" />
        <Stat label="Security" value="TLS 1.3" sub="mTLS · Ed25519" icon={<Lock />} accent="ok" />
        <Stat
          label="Congestion"
          value={q.congestion}
          sub="control algorithm"
          icon={<TrendingUp />}
          accent="info"
          hint="The algorithm that decides how fast to send without overwhelming the link (bbr/cubic/newreno)."
        />
        <Stat
          label="0-RTT"
          value={q.enable_0rtt ? "enabled" : "disabled"}
          sub="session resumption"
          icon={<Repeat />}
          accent={q.enable_0rtt ? "ok" : "warn"}
          hint="Resume a known connection with zero extra round-trips — faster reconnects."
        />
        <Stat
          label="Best throughput"
          value={`${best.mbPerSec} MB/s`}
          sub="measured loopback"
          icon={<Gauge />}
          accent="ok"
        />
        <Stat
          label="Best parallelism"
          value={`×${best.parallelism}`}
          sub={`${num(best.rowsPerSec)} rows/s`}
          icon={<Layers />}
          accent="info"
          hint="Splitting a result across several streams — throughput jumps then plateaus."
        />
      </div>

      {/* Measured loopback benchmark — THE REAL DATA */}
      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <CardTitle className="flex items-center gap-2">
                <Gauge className="size-4 text-primary" /> Measured loopback benchmark
              </CardTitle>
              <CardDescription>
                Real result-stream throughput vs. parallelism — {num(transport.bench.rows)}{" "}
                rows, single host, no network. Throughput jumps from parallelism
                1→2 then plateaus: real bandwidth-delay-product behavior on
                loopback.
              </CardDescription>
            </div>
            <div className="flex gap-4 text-right">
              <div>
                <div className="text-muted-foreground text-xs">best p50</div>
                <div className="text-lg font-semibold tabular-nums">{ms(best.p50Ms)}</div>
              </div>
              <div>
                <div className="text-muted-foreground text-xs">peak</div>
                <div className="text-lg font-semibold tabular-nums text-[var(--ok)]">
                  {best.mbPerSec} MB/s
                </div>
              </div>
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <BarMini data={sweep} xKey="parallelism" yKey="mbPerSec" color="var(--chart-2)" />
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Parallelism</TableHead>
                <TableHead className="text-right">Rows / sec</TableHead>
                <TableHead className="text-right">MB / sec</TableHead>
                <TableHead className="pr-6 text-right">p50</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sweep.map((r) => {
                const isBest = r.parallelism === best.parallelism;
                return (
                  <TableRow key={r.parallelism}>
                    <TableCell className="pl-6 font-mono text-xs">
                      ×{r.parallelism}
                      {isBest ? (
                        <Badge variant="ok" className="ml-2">best</Badge>
                      ) : null}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {num(r.rowsPerSec)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {r.mbPerSec}
                    </TableCell>
                    <TableCell className="pr-6 text-right tabular-nums">
                      {ms(r.p50Ms)}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
          <div>
            <SectionTitle
              className="mb-2"
              hint="reproduce locally"
              info="The exact command and env knobs to re-run this throughput-vs-parallelism benchmark on your own machine."
            >
              <span className="flex items-center gap-1.5">
                <Terminal className="size-3.5" /> Bench command
              </span>
            </SectionTitle>
            <div className="bg-muted/40 overflow-x-auto rounded-lg border p-3 font-mono text-xs">
              {transport.bench.command}
            </div>
            <div className="mt-2 flex flex-wrap gap-2">
              {transport.bench.envKnobs.map((k) => (
                <Badge key={k} variant="muted" className="font-mono">
                  {k}
                </Badge>
              ))}
            </div>
            <p className="text-muted-foreground mt-3 text-xs">
              Real measurements from{" "}
              <span className="font-mono">cargo test --test benches</span> on this
              machine (engine {meta.engineVersion}).
            </p>
          </div>
        </CardContent>
      </Card>

      {/* Tuning + estimate */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Cpu className="size-4 text-primary" /> Tuning
            </CardTitle>
            <CardDescription>
              Per-link transport knobs under <span className="font-mono text-xs">[transport]</span>.
              Every control is seeded from this node&apos;s real configured
              defaults; changes are previewed live below.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-6">
            {/* selects row */}
            <div className="grid gap-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label>Congestion control</Label>
                <Select value={cc} onValueChange={(v) => setCc(v as Cc)}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="bbr">bbr</SelectItem>
                    <SelectItem value="cubic">cubic</SelectItem>
                    <SelectItem value="newreno">newreno</SelectItem>
                  </SelectContent>
                </Select>
                <p className="text-muted-foreground text-xs">
                  config default: <span className="font-mono">{q.congestion}</span>
                </p>
              </div>
              <div className="space-y-2">
                <Label>Wire compression</Label>
                <Tabs value={comp} onValueChange={(v) => setComp(v as Compression)}>
                  <TabsList className="w-full">
                    <TabsTrigger value="none">none</TabsTrigger>
                    <TabsTrigger value="lz4">lz4</TabsTrigger>
                    <TabsTrigger value="zstd">zstd</TabsTrigger>
                  </TabsList>
                </Tabs>
                <p className="text-muted-foreground text-xs">
                  config default:{" "}
                  <span className="font-mono">{transport.compression.algorithm}</span>{" "}
                  · min {bytes(transport.compression.min_size_bytes)}
                </p>
              </div>
            </div>

            {/* switches */}
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
              <div className="bg-card flex items-center justify-between rounded-lg border p-3">
                <Label htmlFor="gso" className="text-xs">GSO offload</Label>
                <Switch id="gso" checked={gso} onCheckedChange={setGso} />
              </div>
              <div className="bg-card flex items-center justify-between rounded-lg border p-3">
                <Label htmlFor="gro" className="text-xs">GRO offload</Label>
                <Switch id="gro" checked={gro} onCheckedChange={setGro} />
              </div>
              <div className="bg-card flex items-center justify-between rounded-lg border p-3">
                <Label htmlFor="pacing" className="text-xs">Pacing</Label>
                <Switch id="pacing" checked={pacing} onCheckedChange={setPacing} />
              </div>
              <div className="bg-card flex items-center justify-between rounded-lg border p-3">
                <Label htmlFor="zerortt" className="text-xs">0-RTT resumption</Label>
                <Switch id="zerortt" checked={zeroRtt} onCheckedChange={setZeroRtt} />
              </div>
            </div>

            {/* static config readout (non-interactive real values) */}
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="bg-muted/40 flex items-center justify-between rounded-lg border p-3">
                <span className="text-muted-foreground text-xs">max uni-streams</span>
                <span className="font-mono text-xs tabular-nums">
                  {num(q.max_concurrent_uni_streams)}
                </span>
              </div>
              <div className="bg-muted/40 flex items-center justify-between rounded-lg border p-3">
                <span className="text-muted-foreground text-xs">send window</span>
                <span className="font-mono text-xs tabular-nums">
                  {bytes(q.send_window_bytes)}
                </span>
              </div>
            </div>

            {/* BDP toggle */}
            <div className="bg-card flex items-center justify-between rounded-lg border p-3">
              <div>
                <Label htmlFor="bdp" className="text-xs">BDP-derived flow control</Label>
                <p className="text-muted-foreground mt-0.5 text-xs">
                  config default: {q.bdp.enabled ? "on" : "off"} — size windows from
                  bandwidth × RTT
                </p>
              </div>
              <Switch id="bdp" checked={bdpOn} onCheckedChange={setBdpOn} />
            </div>

            {/* sliders */}
            <div className="space-y-5">
              <div>
                <div className="mb-2 flex items-center justify-between">
                  <Label>Result-stream parallelism</Label>
                  <span className="text-sm font-medium tabular-nums">
                    {parallelism} {parallelism === 1 ? "stream" : "streams"}
                  </span>
                </div>
                <Slider
                  min={1}
                  max={8}
                  step={1}
                  value={[parallelism]}
                  onValueChange={(v) => setParallelism(v[0])}
                />
                <p className="text-muted-foreground mt-1.5 text-xs">
                  concurrent unidirectional QUIC streams per call · config default{" "}
                  ×{transport.result.parallelism}
                </p>
              </div>

              <div>
                <div className="mb-2 flex items-center justify-between">
                  <Label>BDP bandwidth</Label>
                  <span className="text-sm font-medium tabular-nums">
                    {num(bandwidth)} Mbps
                  </span>
                </div>
                <Slider
                  min={50}
                  max={2000}
                  step={50}
                  value={[bandwidth]}
                  onValueChange={(v) => setBandwidth(v[0])}
                />
              </div>

              <div>
                <div className="mb-2 flex items-center justify-between">
                  <Label>BDP RTT</Label>
                  <span className="text-sm font-medium tabular-nums">{ms(rtt)}</span>
                </div>
                <Slider
                  min={5}
                  max={300}
                  step={1}
                  value={[rtt]}
                  onValueChange={(v) => setRtt(v[0])}
                />
              </div>
            </div>
          </CardContent>
        </Card>

        {/* live estimate */}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <TrendingUp className="size-4 text-primary" /> Estimate
            </CardTitle>
            <CardDescription>derived from the config on the left</CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="bg-card rounded-xl border p-4">
              <div className="text-muted-foreground text-xs font-medium">
                Estimated max throughput
              </div>
              <div className="mt-1 text-3xl font-semibold tracking-tight tabular-nums text-[var(--ok)]">
                {(estMbps / 1000).toFixed(2)}
                <span className="text-muted-foreground ml-1 text-base font-normal">
                  Gbps
                </span>
              </div>
              <div className="text-muted-foreground mt-1 text-xs tabular-nums">
                {num(Math.round(estMbps))} Mbps · {parallelism} streams ·{" "}
                {comp === "none" ? "no compression" : comp}
              </div>
            </div>

            <div className="bg-card rounded-xl border p-4">
              <div className="text-muted-foreground text-xs font-medium">
                Flow-control window {bdpOn ? "(BDP target)" : "(fixed send window)"}
              </div>
              <div className="mt-1 text-2xl font-semibold tabular-nums">
                {bytes(windowBytes)}
              </div>
              <div className="text-muted-foreground mt-1 text-xs tabular-nums">
                {bdpOn
                  ? `${num(bandwidth)} Mbps × ${ms(rtt)}`
                  : "send_window_bytes (BDP off)"}
              </div>
            </div>

            <div className="text-muted-foreground bg-muted/40 rounded-lg border p-3 font-mono text-xs leading-relaxed">
              cc=<span className="text-foreground">{cc}</span> comp=
              <span className="text-foreground">{comp}</span> streams=
              <span className="text-foreground">{parallelism}</span>
              <br />
              gso=<span className="text-foreground">{gso ? "on" : "off"}</span>{" "}
              gro=<span className="text-foreground">{gro ? "on" : "off"}</span>{" "}
              pacing=<span className="text-foreground">{pacing ? "on" : "off"}</span>
              <br />
              0rtt=<span className="text-foreground">{zeroRtt ? "on" : "off"}</span>{" "}
              bdp=<span className="text-foreground">{bdpOn ? "on" : "off"}</span>
            </div>

            <p className="text-muted-foreground text-xs">
              Sim seeded from the node&apos;s real configured defaults. Measured
              throughput is the loopback benchmark above ({best.mbPerSec} MB/s);
              this estimate models a WAN link.
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Tunable reference */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Shield className="size-4 text-primary" /> What&apos;s tunable under{" "}
            <span className="font-mono text-sm">[transport]</span>
          </CardTitle>
          <CardDescription>
            Every knob is config-driven and most are overridable per call.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Knob</TableHead>
                <TableHead>Config field</TableHead>
                <TableHead className="pr-6">Description</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {TUNABLES.map((t) => (
                <TableRow key={t.name}>
                  <TableCell className="pl-6 font-medium whitespace-nowrap">
                    {t.name}
                  </TableCell>
                  <TableCell className="font-mono text-xs whitespace-nowrap">
                    {t.field}
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-sm">
                    {t.desc}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <TransportPlots />
    </div>
  );
}
