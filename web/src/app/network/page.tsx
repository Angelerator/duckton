import type { Metadata } from "next";
import {
  Antenna,
  Cable,
  Cpu,
  Fingerprint,
  Lock,
  Radio,
  Route,
  Scale,
  Server,
  ShieldCheck,
  Signal,
  Split,
  Waypoints,
  Wifi,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
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
  Dot,
  KV,
  PageHeader,
  ScoreBar,
  SectionTitle,
  Stat,
} from "@/components/common/atoms";
import { CopyId } from "@/components/common/copy";
import { Explainer } from "@/components/common/explain";
import { config, nodes } from "@/lib/data";
import { bytes, num } from "@/lib/format";
import { CommGraphCard } from "./comm-graph";

export const metadata: Metadata = {
  title: "Network",
  description: "Kademlia DHT + gossip discovery overlay, NAT traversal stack, and live swarm node directory.",
};

/* ---- real swarm stats over the in-process loopback nodes ---------------- */
const online = nodes.filter((n) => n.online).length;
const l2 = nodes.filter((n) => n.attestation === "L2").length;

/* ---- real discovery config (the live GridConfig [discovery] section) ----- */
type DiscoveryCfg = {
  mode?: string;
  bootstrap?: string[];
  candidate_sample_size?: number;
  listen_addrs?: string[];
  kademlia?: {
    query_parallelism?: number;
    record_ttl_secs?: number;
    replication_factor?: number;
  };
  gossip?: {
    topic?: string;
    fanout?: number;
    heartbeat_ms?: number;
    capability_ttl_secs?: number;
  };
  nat?: {
    autonat?: boolean;
    dcutr?: boolean;
    relay_client?: boolean;
    act_as_relay?: boolean;
    mdns?: boolean;
    max_relays?: number;
    mdns_query_interval_secs?: number;
    external_addresses?: string[];
    relays?: string[];
    relay_limits?: Record<string, number>;
  };
};

const discovery = (config.value.discovery ?? {}) as DiscoveryCfg;
const nat = discovery.nat ?? {};
const discoveryMode = discovery.mode ?? "kademlia+gossip";

/* ---- closure posture + routing/failover (real GridConfig) ---------------- */
const cfgStr = (v: unknown, d = ""): string => (typeof v === "string" ? v : d);
const cfgNum = (v: unknown, d = 0): number => (typeof v === "number" ? v : d);
const cfgBool = (v: unknown): boolean => v === true;

const security = (config.value.security ?? {}) as Record<string, unknown>;
const membership = (config.value.membership ?? {}) as Record<string, unknown>;
const securityMode = cfgStr(security.mode, "public");
const isPrivate = securityMode === "private";
const groupEnforcement = cfgStr(membership.group_enforcement, "soft");
const memberNetworks = Array.isArray(membership.networks)
  ? (membership.networks as unknown[]).filter((x): x is string => typeof x === "string")
  : [];

const planner = (config.value.planner ?? {}) as Record<string, unknown>;
const scheduler = (config.value.scheduler ?? {}) as Record<string, unknown>;
const plannerPrefer = cfgStr(planner.prefer, "auto");
const sizeThresholdBytes = cfgNum(planner.size_threshold_bytes);
const ramFraction = cfgNum(planner.ram_fraction);
const localExecution = cfgBool(planner.local_execution_enabled);
const maxRetries = cfgNum(scheduler.max_retries);

/* a real worker whose gossiped capability record we render verbatim */
const sample = nodes.find((n) => n.attestation === "L2") ?? nodes[0];

/* the wire shape a worker publishes on the capacity topic, real values */
const capabilityRecord: { k: string; v: string }[] = [
  { k: "node_id", v: sample.id },
  { k: "free_mem", v: bytes(sample.freeMemBytes) },
  { k: "free_cores", v: String(sample.freeThreads) },
  { k: "max_jobs", v: String(sample.maxJobs) },
  { k: "attestation_level", v: sample.attestation },
  { k: "price", v: `${sample.price} /unit` },
  { k: "recent_receipts_root", v: sample.id },
];

/* libp2p NAT-traversal stack — booleans/values pulled from the real config */
function onOff(v: boolean | undefined): { variant: Parameters<typeof Badge>[0]["variant"]; t: string } {
  return v ? { variant: "ok", t: "enabled" } : { variant: "muted", t: "off" };
}

const techniques: {
  name: string;
  detail: string;
  badge: { variant: Parameters<typeof Badge>[0]["variant"]; t: string };
}[] = [
  {
    name: "identify",
    detail: "exchange peer keys, listen addrs & supported protocols",
    badge: { variant: "ok", t: "always on" },
  },
  {
    name: "AutoNAT",
    detail: "peers probe you to learn your external addr & reachability",
    badge: onOff(nat.autonat),
  },
  {
    name: "DCUtR hole punching",
    detail: "coordinated simultaneous-open over QUIC/UDP → direct path",
    badge: onOff(nat.dcutr),
  },
  {
    name: "Circuit Relay v2 (client)",
    detail: `reserve slots on volunteer relays when behind symmetric NAT · max_relays ${nat.max_relays ?? 0}`,
    badge: onOff(nat.relay_client),
  },
  {
    name: "AutoRelay (act as relay)",
    detail: "volunteer this node as a relay for others",
    badge: onOff(nat.act_as_relay),
  },
  {
    name: "mDNS",
    detail: `zero-config peer discovery on the local LAN · every ${nat.mdns_query_interval_secs ?? 0}s`,
    badge: onOff(nat.mdns),
  },
  {
    name: "DHT routing (Kademlia)",
    detail: `O(log n) FIND_NODE · repl ${discovery.kademlia?.replication_factor ?? "—"} · par ${discovery.kademlia?.query_parallelism ?? "—"}`,
    badge: { variant: "ok", t: "enabled" },
  },
];

export default function NetworkPage() {
  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Waypoints />}
        title="Network"
        description="A Kademlia DHT + gossip discovery overlay on top of the libp2p NAT-traversal stack. On this real snapshot the swarm runs in-process over loopback QUIC, so there is no geography, NAT or WAN RTT to measure — exercising hole-punching, relays and AutoNAT needs a multi-host WAN deployment."
      />

      <Explainer
        what="The peer-to-peer overlay that lets machines discover and reach each other directly — even from behind home or office routers — with no central server. This snapshot runs in-process over loopback, so geography and NAT are not applicable here."
        impact="The grid keeps working as nodes join and leave, and there is no single point that can fail, censor, or be taken down."
      />

      {/* Stat row — real swarm + discovery config */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-5">
        <Stat
          label="Nodes in swarm"
          value={nodes.length}
          sub="loopback peers"
          icon={<Server />}
          accent="primary"
        />
        <Stat
          label="Online"
          value={online}
          sub={`${nodes.length} known`}
          icon={<Signal />}
          accent="ok"
        />
        <Stat
          label="L2 nodes"
          value={l2}
          sub="TEE attested"
          icon={<Fingerprint />}
          accent="info"
          hint="Nodes running in a confidential enclave (TEE) whose memory even the machine's owner cannot read."
        />
        <Stat
          label="Transport"
          value="QUIC"
          sub="loopback"
          icon={<Cable />}
          accent="info"
          hint="The fast, encrypted internet transport (TLS 1.3) that carries all data between nodes."
        />
        <Stat
          label="Discovery"
          value={discoveryMode}
          sub="overlay mode"
          icon={<Radio />}
          accent="primary"
          hint="How nodes find each other with no central directory: a distributed hash table (DHT) plus gossip."
        />
      </div>

      {/* Circular node-communication graph (plotly) */}
      <CommGraphCard />

      {/* Closure posture + smart routing / failover */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <div className="flex items-start justify-between gap-2">
              <CardTitle className="flex items-center gap-2">
                <ShieldCheck className="size-4 text-primary" /> Closure posture
              </CardTitle>
              <Badge variant={isPrivate ? "warn" : "muted"} className="font-mono">
                {securityMode}
              </Badge>
            </div>
            <CardDescription>
              The single <span className="font-mono">[security].mode</span> switch between the open
              public grid and a fully closed company / enterprise grid.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <dl>
              <KV label="mode">
                <Badge variant={isPrivate ? "warn" : "muted"} className="font-mono">
                  {securityMode}
                </Badge>
              </KV>
              <KV label="group enforcement">
                <span className="font-mono">{groupEnforcement}</span>
              </KV>
              <KV label="networks">
                <span className="font-mono">{memberNetworks.join(", ") || "default"}</span>
              </KV>
            </dl>
            <div className="space-y-1.5 text-xs">
              {[
                "allowlist mTLS — outsiders refused at the transport layer",
                "cryptographic group tokens (never soft declared labels)",
                "fail-closed discovery — drop unknown-labeled peers",
                "default-deny requester roster — serve only roster members",
              ].map((line) => (
                <div key={line} className="flex items-start gap-2">
                  <Lock
                    className={`mt-0.5 size-3 shrink-0 ${
                      isPrivate ? "text-[var(--warn)]" : "text-muted-foreground"
                    }`}
                  />
                  <span className="text-muted-foreground">{line}</span>
                </div>
              ))}
            </div>
            <p className="text-muted-foreground border-t pt-2 text-xs">
              {isPrivate
                ? "PRIVATE: the closed pool is enforced; a misconfigured node fails to start (fail-closed)."
                : "PUBLIC (this run): zero-config grid. Setting mode = private requires an allowlist roster, token group enforcement, and an explicit non-default network — else the node refuses to start."}{" "}
              The requester↔TLS-peer identity binding (an offer&apos;s requester_id must equal the
              authenticated mTLS peer) is <span className="text-foreground">always on</span> in both
              modes.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Split className="size-4 text-primary" /> Smart routing &amp; failover
            </CardTitle>
            <CardDescription>
              A pre-flight, metadata-only size estimate picks local-vs-remote automatically, and
              over-capacity jobs reroute to bigger hosts instead of failing.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <dl>
              <KV label="prefer">
                <Badge variant="info" className="font-mono">
                  {plannerPrefer}
                </Badge>
              </KV>
              <KV label="local size threshold">{bytes(sizeThresholdBytes, 0)}</KV>
              <KV label="local RAM fraction">{Math.round(ramFraction * 100)}%</KV>
              <KV label="local execution">
                <Badge variant={localExecution ? "ok" : "muted"}>
                  {localExecution ? "enabled" : "remote-only"}
                </Badge>
              </KV>
              <KV label="re-dispatch retries">
                {maxRetries === 0 ? "unlimited" : maxRetries}
              </KV>
            </dl>
            <div className="space-y-1.5 text-xs">
              <div className="flex items-start gap-2">
                <Route className="text-primary mt-0.5 size-3 shrink-0" />
                <span className="text-muted-foreground">
                  Size-based routing: a job estimated to fit runs FREE in the node&apos;s own
                  locked-down DuckDB; one that exceeds the local threshold/headroom is dispatched to
                  the grid.
                </span>
              </div>
              <div className="flex items-start gap-2">
                <Scale className="text-primary mt-0.5 size-3 shrink-0" />
                <span className="text-muted-foreground">
                  Robust failover: a &ldquo;too big&rdquo; / OOM job reroutes to higher-capacity hosts
                  (excluding only failed nodes); only when none remain does it return{" "}
                  <span className="font-mono">ExceedsCapacity</span>.
                </span>
              </div>
            </div>
          </CardContent>
        </Card>
      </div>

      {/* Swarm */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Server className="size-4 text-primary" /> Swarm
          </CardTitle>
          <CardDescription>
            The real worker nodes from this run. Each advertises a signed
            capability record; all are wired over a single loopback transport (no
            geography on one host).
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Node</TableHead>
                <TableHead>ID</TableHead>
                <TableHead>Attestation</TableHead>
                <TableHead>Capability record</TableHead>
                <TableHead>Trust</TableHead>
                <TableHead className="pr-6 text-right">Transport</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {nodes.map((n) => (
                <TableRow key={n.id}>
                  <TableCell className="pl-6">
                    <div className="flex items-center gap-2">
                      <Dot status={n.online ? "ok" : "muted"} pulse={n.online} />
                      <span className="text-sm font-medium">{n.alias}</span>
                    </div>
                  </TableCell>
                  <TableCell>
                    <CopyId value={n.id} />
                  </TableCell>
                  <TableCell>
                    <AttestationBadge level={n.attestation} />
                  </TableCell>
                  <TableCell className="text-muted-foreground font-mono text-xs whitespace-nowrap">
                    {n.freeThreads} thr · {bytes(n.freeMemBytes)} · max {n.maxJobs}
                  </TableCell>
                  <TableCell className="w-40">
                    <ScoreBar value={n.trust} />
                  </TableCell>
                  <TableCell className="pr-6 text-right">
                    <Badge variant="muted" className="font-mono">
                      {n.via}
                    </Badge>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Discovery + capability record */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Route className="size-4 text-primary" /> Discovery &amp; NAT traversal
            </CardTitle>
            <CardDescription>
              The libp2p stack as configured in the live{" "}
              <span className="font-mono text-xs">[discovery]</span> /{" "}
              <span className="font-mono text-xs">[discovery.nat]</span> sections of
              this run&apos;s GridConfig. libp2p climbs from cheapest to most
              expensive path; relays are a last resort, never the default.
            </CardDescription>
          </CardHeader>
          <CardContent className="px-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-6">Technique</TableHead>
                  <TableHead>What it does</TableHead>
                  <TableHead className="pr-6 text-right">State</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {techniques.map((t) => (
                  <TableRow key={t.name}>
                    <TableCell className="pl-6 font-mono text-xs whitespace-nowrap">
                      {t.name}
                    </TableCell>
                    <TableCell className="text-muted-foreground text-xs">
                      {t.detail}
                    </TableCell>
                    <TableCell className="pr-6 text-right">
                      <Badge variant={t.badge.variant}>{t.badge.t}</Badge>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
            <div className="space-y-3 px-6 pt-4">
              <div className="border-[var(--info)]/25 bg-[var(--info)]/5 rounded-lg border p-3">
                <div className="text-foreground flex items-center gap-1.5 text-xs font-medium">
                  <Antenna className="size-3.5 text-[var(--info)]" /> Bootstrap
                  caveat
                </div>
                <p className="text-muted-foreground mt-1 text-xs">
                  A swarm needs ≥1 reachable entry point to join — but a bootstrap
                  peer is an ordinary node. It holds no job state, never sits in the
                  data path, and is freely replaceable: swap the address and the
                  swarm is unchanged. (This run&apos;s{" "}
                  <span className="font-mono">discovery.bootstrap</span> is empty —{" "}
                  {discovery.bootstrap?.length ?? 0} entries — since every node is
                  already in-process.)
                </p>
              </div>
              <div className="border-[var(--warn)]/25 bg-[var(--warn)]/5 rounded-lg border p-3">
                <div className="text-foreground flex items-center gap-1.5 text-xs font-medium">
                  <Wifi className="size-3.5 text-[var(--warn)]" /> Single-host caveat
                </div>
                <p className="text-muted-foreground mt-1 text-xs">
                  Hole-punching (DCUtR), Circuit Relay and AutoNAT are configured
                  and compiled in, but can only actually be exercised across real,
                  separate networks. On this loopback run there are no NATs to
                  traverse, so these paths are never triggered.
                </p>
              </div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Capability record (gossiped)</CardTitle>
            <CardDescription>signed, gossiped, locally filtered</CardDescription>
          </CardHeader>
          <CardContent>
            <div className="bg-muted/40 rounded-lg border p-3 font-mono text-xs">
              <span className="text-muted-foreground">{"{"}</span>
              <dl>
                {capabilityRecord.map((f) => (
                  <KV key={f.k} label={f.k} className="py-0.5">
                    <span className="text-foreground break-all">{f.v}</span>
                  </KV>
                ))}
              </dl>
              <span className="text-muted-foreground">{"}"}</span>
            </div>
            <p className="text-muted-foreground mt-3 text-xs">
              The real shape <span className="font-mono">{sample.alias}</span>{" "}
              publishes on topic{" "}
              <span className="font-mono">
                {discovery.gossip?.topic ?? "caps"}
              </span>{" "}
              — refreshed every{" "}
              {discovery.gossip?.heartbeat_ms
                ? `${discovery.gossip.heartbeat_ms / 1000}s`
                : "heartbeat"}
              , TTL {discovery.gossip?.capability_ttl_secs ?? "—"}s, signed with
              each node&apos;s Ed25519 identity.
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Discovery explainer */}
      <div>
        <SectionTitle
          hint="no registry, no coordinator"
          info="What each node gossips about itself (free RAM, threads, attestation, price) so requesters can shop for workers."
        >
          Discovery
        </SectionTitle>
        <div className="grid gap-4 lg:grid-cols-3">
          <Card className="lg:col-span-2">
            <CardHeader>
              <CardTitle className="flex items-center gap-2">
                <Fingerprint className="size-4 text-primary" /> How peers find work
              </CardTitle>
              <CardDescription>
                Lookup is structured; matchmaking is local. No node ever sees the
                whole graph.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <ol className="space-y-3 text-sm">
                <li className="flex gap-3">
                  <span className="text-muted-foreground/60 font-mono text-xs">1</span>
                  <span>
                    <span className="font-medium">Kademlia DHT.</span>{" "}
                    <span className="text-muted-foreground">
                      O(log n) peer lookup keyed by node id — replication factor{" "}
                      {discovery.kademlia?.replication_factor ?? "—"}, record TTL{" "}
                      {discovery.kademlia?.record_ttl_secs ?? "—"}s. Scales without a
                      central index.
                    </span>
                  </span>
                </li>
                <li className="flex gap-3">
                  <span className="text-muted-foreground/60 font-mono text-xs">2</span>
                  <span>
                    <span className="font-medium">Gossip / pubsub.</span>{" "}
                    <span className="text-muted-foreground">
                      Workers publish a signed{" "}
                      <span className="font-mono text-xs">capability record</span> to
                      topic{" "}
                      <span className="font-mono text-xs">
                        {discovery.gossip?.topic ?? "caps"}
                      </span>{" "}
                      (fanout {discovery.gossip?.fanout ?? "—"}) and refresh it as
                      load changes.
                    </span>
                  </span>
                </li>
                <li className="flex gap-3">
                  <span className="text-muted-foreground/60 font-mono text-xs">3</span>
                  <span>
                    <span className="font-medium">Local filtering.</span>{" "}
                    <span className="text-muted-foreground">
                      Requesters subscribe and filter the stream client-side by
                      capacity, trust and attestation (sampling up to{" "}
                      {discovery.candidate_sample_size ?? "—"} candidates) before
                      sending an offer.
                    </span>
                  </span>
                </li>
                <li className="flex gap-3">
                  <span className="text-muted-foreground/60 font-mono text-xs">4</span>
                  <span>
                    <span className="font-medium">Bootstrap seeds.</span>{" "}
                    <span className="text-muted-foreground">
                      Used once, only to enter the swarm — then dropped from the hot
                      path entirely.
                    </span>
                  </span>
                </li>
              </ol>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">Overlay config</CardTitle>
              <CardDescription>real [discovery] values</CardDescription>
            </CardHeader>
            <CardContent>
              <dl>
                <KV label="mode">
                  <Badge variant="info">{discoveryMode}</Badge>
                </KV>
                <KV label="candidate sample">
                  {discovery.candidate_sample_size ?? "—"}
                </KV>
                <KV label="kad replication">
                  {discovery.kademlia?.replication_factor ?? "—"}
                </KV>
                <KV label="kad query par">
                  {discovery.kademlia?.query_parallelism ?? "—"}
                </KV>
                <KV label="gossip fanout">
                  {discovery.gossip?.fanout ?? "—"}
                </KV>
                <KV label="cap TTL">
                  {discovery.gossip?.capability_ttl_secs ?? "—"}s
                </KV>
                <KV label="max relays">{nat.max_relays ?? "—"}</KV>
              </dl>
              <div className="mt-3 flex items-center gap-1.5 border-t pt-3">
                <Cpu className="text-muted-foreground size-3.5" />
                <span className="text-muted-foreground text-xs">
                  {num(online)} live capability records on the topic.
                </span>
              </div>
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  );
}
