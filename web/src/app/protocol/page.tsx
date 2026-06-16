import type { Metadata } from "next";
import {
  ArrowRight,
  Ban,
  Cable,
  CheckCircle2,
  Database,
  FileText,
  GitBranch,
  Hash,
  Layers,
  Network,
  Radio,
  Scale,
  ShieldCheck,
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
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from "@/components/ui/accordion";
import { KV, PageHeader, Stat } from "@/components/common/atoms";
import { Explainer, InfoHint } from "@/components/common/explain";
import { meta, protocol } from "@/lib/data";

export const metadata: Metadata = {
  title: "Protocol",
  description: "QUIC wire protocol, request lifecycle stages, message reference, and version negotiation for the Duckton grid.",
};

/* ------------------------------------------------------------- lifecycle */

type Role = "R" | "W";

const lifecycle: {
  stage: string;
  from: Role;
  to: Role;
  badge: Parameters<typeof Badge>[0]["variant"];
  desc: string;
}[] = [
  {
    stage: "Offer",
    from: "R",
    to: "W",
    badge: "info",
    desc: "Requester broadcasts query_hash + a fresh nonce. No SQL yet — workers bid blind on the hash and cost hint.",
  },
  {
    stage: "Bid",
    from: "W",
    to: "R",
    badge: "info",
    desc: "Worker replies Accept with an ETA, its attestation, and recent receipts. Requester selects k by trust + ETA.",
  },
  {
    stage: "Dispatch",
    from: "R",
    to: "W",
    badge: "info",
    desc: "Full SQL + a scoped credential, sealed to each node key. For Sensitive data a SealedKey is gated on attestation.",
  },
  {
    stage: "Progress",
    from: "W",
    to: "R",
    badge: "muted",
    desc: "Liveness heartbeat while executing. A stall past the deadline ⇒ the requester re-dispatches to a fresh host.",
  },
  {
    stage: "Commit",
    from: "W",
    to: "R",
    badge: "secondary",
    desc: "result_hash is sent FIRST — commit-first — binding the worker to an answer before any bytes stream.",
  },
  {
    stage: "Verify",
    from: "R",
    to: "R",
    badge: "secondary",
    desc: "Requester waits for quorum: q matching result hashes across the racing workers before accepting.",
  },
  {
    stage: "Stream",
    from: "W",
    to: "R",
    badge: "ok",
    desc: "Winner only sends a Manifest then Chunk/Part frames over parallel uni-streams. Losers never stream.",
  },
  {
    stage: "Cancel / RESET",
    from: "R",
    to: "W",
    badge: "muted",
    desc: "Losing racers are cancelled and RESET — their in-flight work is discarded; they incur no fault.",
  },
  {
    stage: "Receipt",
    from: "R",
    to: "R",
    badge: "ok",
    desc: "Requester emits a signed receipt per worker and gossips it into the reputation trail.",
  },
];

/* ------------------------------------------------------- message reference */

const messageNotes: Record<string, string> = {
  Offer: "R→W — broadcast solicitation: query_hash + nonce + cost hint, no SQL yet.",
  Bid: "W→R — accept (ETA + attestation + free capacity) or reject the offer.",
  Dispatch: "R→W — award the work: full SQL, scoped credential, budgets, verify mode.",
  ResultCommit: "W→R — commit-first: the canonical BLAKE3 result_hash before any bytes stream.",
  Receipt: "R — signed, gossiped verdict that feeds the reputation trail.",
};

const messageOrder = ["Offer", "Bid", "Dispatch", "ResultCommit", "Receipt"];

/* ------------------------------------------------------------- verdicts */

const faultBadge: Record<string, Parameters<typeof Badge>[0]["variant"]> = {
  provider: "destructive",
  requester: "warn",
  neutral: "muted",
};

const verdictNotes: Record<string, string> = {
  Correct: "result agreed with quorum",
  Incorrect: "diverged from the agreed hash",
  Timeout: "accepted then failed to commit",
  Malformed: "unparseable / protocol-violating reply",
  ResourceExceeded: "job exceeded its declared budget",
  Infeasible: "query could not be satisfied as posed",
  Inconclusive: "no quorum reached; no party penalised",
};

/* ------------------------------------------ plain-language stage tooltips */

const stageHints: Record<string, string> = {
  Commit:
    "Commit-first: workers send the result fingerprint before the data, so they can't copy each other's answers.",
  Verify: "Quorum: enough matching results must agree before an answer is accepted.",
};

/* ----------------------------------------------------- compatibility matrix */

const compat: { peer: string; result: "Accept" | "Reject"; why: string }[] = [
  { peer: meta.protocolVersion, result: "Accept", why: "current — full feature set" },
  { peer: "1.0.0", result: "Accept", why: "exactly min_supported" },
  { peer: "0.9.9", result: "Reject", why: "below min_supported ⇒ VersionReject" },
];

/* ----------------------------------------------------------------- page */

export default function ProtocolPage() {
  const hs = protocol.handshake;
  const messages = protocol.messages as Record<string, unknown>;

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Network />}
        title="Protocol"
        description="QUIC wire protocol, the request lifecycle, and version negotiation. Every wire variant, message body, and verdict below is the real serialized output of the Rust protocol types from this run."
      />

      <Explainer
        what="The exact messages machines exchange to run a job (Offer → Bid → Dispatch → Commit → Verify → Settle) and how they negotiate versions so different builds stay compatible."
        impact="Independent implementations can talk to each other and upgrade over time without splitting or breaking the network."
      />

      {/* Stat row — real handshake + engine */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat
          label="Wire schema"
          value={`v${hs.wireSchemaVersion}`}
          sub="tagged-enum envelope"
          icon={<Layers />}
          accent="primary"
          hint="The byte-level format of every message on the wire; both peers must agree on it to talk."
        />
        <Stat
          label="Protocol"
          value={hs.version}
          sub="semver, negotiated"
          icon={<Cable />}
          accent="info"
        />
        <Stat
          label="Min supported"
          value={hs.minSupported}
          sub="below ⇒ VersionReject"
          icon={<ShieldCheck />}
          accent="warn"
          hint="The oldest protocol version this node will talk to; older peers are cleanly rejected on connect."
        />
        <Stat
          label="Engine"
          value={meta.engineVersion}
          sub="drives quorum policy"
          icon={<Database />}
          accent="ok"
          hint="The query engine version; results are only compared for agreement across matching engines."
        />
      </div>

      {/* Request lifecycle */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Radio className="size-4 text-primary" /> Request lifecycle
          </CardTitle>
          <CardDescription>
            The happy path, end to end. R = Requester, W = Worker.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <ol className="relative space-y-5 border-l pl-6">
            {lifecycle.map((s, i) => (
              <li key={s.stage} className="relative">
                <span className="bg-background absolute top-0.5 -left-[2.05rem] flex size-6 items-center justify-center rounded-full border text-[11px] font-semibold tabular-nums">
                  {i + 1}
                </span>
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-semibold">{s.stage}</span>
                  <Badge variant={s.badge}>{STAGE_DIR(s.from, s.to)}</Badge>
                  {stageHints[s.stage] ? (
                    <InfoHint text={stageHints[s.stage]} />
                  ) : null}
                </div>
                <p className="text-muted-foreground mt-1 max-w-3xl text-sm">
                  {s.desc}
                </p>
              </li>
            ))}
          </ol>
        </CardContent>
      </Card>

      {/* Wire envelope */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Hash className="size-4 text-primary" /> Wire envelope
          </CardTitle>
          <CardDescription>
            The <code className="font-mono text-xs">Wire</code> tagged-enum
            variants carried over QUIC streams — the real registered set.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Variant</TableHead>
                <TableHead>Direction</TableHead>
                <TableHead className="pr-6">Purpose</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {protocol.wire.map((v) => (
                <TableRow key={v.variant}>
                  <TableCell className="pl-6">
                    <span className="font-mono text-xs font-medium">{v.variant}</span>
                  </TableCell>
                  <TableCell>
                    <Badge variant="muted" className="font-mono">
                      {v.direction}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-sm">
                    {v.purpose}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Message reference */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <FileText className="size-4 text-primary" /> Message reference
          </CardTitle>
          <CardDescription>
            Real wire instances, serialized by the Rust message types. These are
            actual bodies captured from the run — not hand-authored samples.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Accordion type="single" collapsible className="w-full">
            {messageOrder
              .filter((name) => messages[name] !== undefined)
              .map((name) => (
                <AccordionItem key={name} value={name}>
                  <AccordionTrigger>
                    <span className="flex items-baseline gap-3">
                      <span className="font-mono text-sm font-medium">{name}</span>
                      <span className="text-muted-foreground text-xs font-normal">
                        {messageNotes[name]}
                      </span>
                    </span>
                  </AccordionTrigger>
                  <AccordionContent>
                    <pre className="bg-muted/40 overflow-x-auto rounded-lg border p-3 font-mono text-xs leading-relaxed">
                      {JSON.stringify(messages[name], null, 2)}
                    </pre>
                  </AccordionContent>
                </AccordionItem>
              ))}
          </Accordion>
        </CardContent>
      </Card>

      {/* Verdict classes */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Scale className="size-4 text-primary" /> Verdict classes
          </CardTitle>
          <CardDescription>
            Every receipt carries a verdict that maps to a fault class — provider
            faults are penalised; requester/job faults carry zero provider
            penalty; neutral verdicts penalise no one.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Verdict</TableHead>
                <TableHead>Fault class</TableHead>
                <TableHead className="pr-6">Meaning</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {protocol.verdicts.map((v) => (
                <TableRow key={v.verdict}>
                  <TableCell className="pl-6">
                    <span className="font-mono text-xs font-medium">{v.verdict}</span>
                  </TableCell>
                  <TableCell>
                    <Badge variant={faultBadge[v.fault] ?? "muted"}>{v.fault}</Badge>
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-sm">
                    {verdictNotes[v.verdict] ?? ""}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Versioning & compatibility */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <GitBranch className="size-4 text-primary" /> Versioning &
            compatibility
          </CardTitle>
          <CardDescription>
            A single <code className="font-mono text-xs">Hello</code> handshake is
            exchanged per connection before any work.
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-6 lg:grid-cols-2">
          <div>
            <div className="text-muted-foreground mb-2 flex items-center text-sm font-semibold tracking-wide uppercase">
              Hello handshake
              <InfoHint
                className="ml-1.5"
                text="Version handshake: peers exchange versions on connect and reject incompatible ones cleanly."
              />
            </div>
            <dl className="rounded-lg border px-4 py-1">
              <KV label="wire_schema_version">
                <span className="font-mono text-xs">{hs.wireSchemaVersion}</span>
              </KV>
              <KV label="protocol_version">
                <span className="font-mono text-xs">{hs.version}</span>
              </KV>
              <KV label="min_supported">
                <span className="font-mono text-xs">{hs.minSupported}</span>
              </KV>
              <KV label="engine_version">
                <span className="font-mono text-xs">{meta.engineVersion}</span>
              </KV>
              <KV label="require_matching_engine_version">
                <Badge variant={hs.requireMatchingEngineVersion ? "warn" : "muted"}>
                  {hs.requireMatchingEngineVersion ? "true" : "false"}
                </Badge>
              </KV>
            </dl>
            <p className="text-muted-foreground mt-3 flex items-start gap-2 text-xs">
              <Ban className="mt-px size-3.5 shrink-0 text-[var(--warn)]" />
              <span>
                If a peer&apos;s protocol_version &lt; our min_supported (
                <span className="font-mono">{hs.minSupported}</span>), the
                connection is closed with a typed{" "}
                <code className="font-mono">
                  VersionReject{"{ reason, our_version, min_supported }"}
                </code>
                .
              </span>
            </p>
            <p className="text-muted-foreground mt-2 flex items-start gap-2 text-xs">
              <Database className="mt-px size-3.5 shrink-0 text-[var(--ok)]" />
              <span>
                <span className="text-foreground font-medium">engine_version</span>{" "}
                drives result-determinism and quorum policy — hashes are only
                compared across matching engines.
                {hs.requireMatchingEngineVersion
                  ? " This deployment requires an exact engine match."
                  : " A match is not strictly required here, but mismatched engines never enter the same quorum."}
              </span>
            </p>
          </div>

          <div>
            <div className="text-muted-foreground mb-2 text-sm font-semibold tracking-wide uppercase">
              Compatibility · our min {hs.minSupported}
            </div>
            <div className="overflow-hidden rounded-lg border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="pl-4">Peer protocol</TableHead>
                    <TableHead>Outcome</TableHead>
                    <TableHead className="pr-4">Why</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {compat.map((c) => (
                    <TableRow key={c.peer}>
                      <TableCell className="pl-4 font-mono text-xs tabular-nums">
                        {c.peer}
                      </TableCell>
                      <TableCell>
                        {c.result === "Accept" ? (
                          <Badge variant="ok" className="gap-1">
                            <CheckCircle2 className="size-3" /> Accept
                          </Badge>
                        ) : (
                          <Badge variant="destructive" className="gap-1">
                            <Ban className="size-3" /> Reject
                          </Badge>
                        )}
                      </TableCell>
                      <TableCell className="text-muted-foreground pr-4 text-xs">
                        {c.why}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

/* Renders an R→W style direction label inline. */
function STAGE_DIR(from: Role, to: Role) {
  if (from === to) return `${from} only`;
  return (
    <span className="inline-flex items-center gap-1 font-mono">
      {from} <ArrowRight className="size-3" /> {to}
    </span>
  );
}
