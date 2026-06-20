import type { Metadata } from "next";
import {
  ArrowRight,
  Boxes,
  FileText,
  Globe,
  Layers,
  Settings2,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from "@/components/ui/accordion";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { PageHeader, SectionTitle } from "@/components/common/atoms";
import { Explainer, InfoHint } from "@/components/common/explain";
import { config } from "@/lib/data";

export const metadata: Metadata = { title: "Configuration" };

/* ---------------------------------------------------------- resolution order */

const RESOLUTION: { step: number; layer: string; note: string }[] = [
  {
    step: 1,
    layer: "Built-in defaults",
    note: "Compiled-in, safe values — this is exactly the resolved layer shown below. The grid runs with zero config.",
  },
  {
    step: 2,
    layer: "p2p.toml file",
    note: "Operator file, found via P2P_CONFIG or --config. Overrides defaults.",
  },
  {
    step: 3,
    layer: "P2P_* environment",
    note: "Per-process overrides, ideal for secrets and containers.",
  },
  {
    step: 4,
    layer: "Per-call override",
    note: "Argument on a single call, e.g. a p2p_query() parameter.",
  },
];

/* ----------------------------------------------------- effective config model */

// `config.value` is the real `GridConfig::default()` serialized by the Rust
// config crate: an object keyed by section name (Record<string, unknown>).
// We narrow every value with typeof / Array.isArray before rendering — no `any`.

type FlatRow = { key: string; value: string };

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Render a leaf (primitive | array | null) to a compact mono string. */
function formatLeaf(v: unknown): string {
  if (v === null) return "null";
  if (typeof v === "string") return v === "" ? '""' : v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (Array.isArray(v)) return JSON.stringify(v);
  // Fallback for any unexpected shape — keep it readable, never throw.
  return JSON.stringify(v);
}

/**
 * Flatten one config section into dot-pathed key/value rows.
 * Nested objects recurse (`key.subkey`); arrays and primitives are leaves.
 * Empty objects render as a single `{}` leaf so the key is never lost.
 */
function flattenSection(value: unknown, prefix = ""): FlatRow[] {
  if (!isPlainObject(value)) {
    return [{ key: prefix || "value", value: formatLeaf(value) }];
  }
  const rows: FlatRow[] = [];
  const entries = Object.entries(value);
  if (entries.length === 0) {
    return [{ key: prefix || "value", value: "{}" }];
  }
  for (const [k, v] of entries) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (isPlainObject(v)) {
      rows.push(...flattenSection(v, key));
    } else {
      rows.push({ key, value: formatLeaf(v) });
    }
  }
  return rows;
}

// Sort the most operationally interesting sections to the top; keep the rest
// in a stable, sensible order after them.
const SECTION_ORDER = [
  "transport",
  "discovery",
  "membership",
  "security",
  "trust",
  "economics",
  "scheduler",
  "network",
  "protocol",
  "identity",
  "worker",
  "budget",
  "planner",
  "liveness",
  "sybil",
  "antiabuse",
  "storage",
  "limits",
  "sandbox",
] as const;

const SECTION_NOTES: Record<string, string> = {
  transport: "QUIC tuning — congestion control, kernel offloads, compression, 0-RTT.",
  discovery: "How peers are found and gossiped: mode, gossipsub, Kademlia, NAT traversal.",
  membership: "Request-scoping labels: logical networks, capability groups, region, group enforcement.",
  security: "Closure posture — the single public / private (enterprise) mode switch.",
  trust: "Effective-trust weights, attestation floor, quorum and canary auditing.",
  economics: "Settlement, fees (15% / 5%), time-based pricing and reliability-gated stake weighting — only engaged when a job is paid.",
  scheduler: "Dispatch, retries, timeouts, replicas and the verify mode.",
  network: "QUIC socket binding, windows and stream limits.",
  protocol: "Wire protocol versioning and engine-version matching.",
  identity: "Key material, peer pinning mode and the allowlist.",
  worker: "Local worker execution timeouts.",
  budget: "Local resource ceilings: memory, threads and concurrent jobs.",
  planner: "Local-vs-remote planning heuristics and spill tolerance.",
  liveness: "Failure detection — phi-accrual and SWIM probing.",
  sybil: "Sybil resistance: minimum stake, PoW difficulty, vouch weight.",
  antiabuse: "Rate limits, fault attribution and gossip hardening.",
  storage: "Object-store provider, readable formats and credential TTLs.",
  limits: "Internal cache and pool capacities.",
  sandbox: "Worker sandbox backend, egress policy and per-job limits.",
};

const configValue = config.value;

// Order known sections first, then append any unknown ones (future-proof).
const orderedSectionNames: string[] = [
  ...SECTION_ORDER.filter((name) => name in configValue),
  ...Object.keys(configValue)
    .filter((name) => !(SECTION_ORDER as readonly string[]).includes(name))
    .sort(),
];

const sections = orderedSectionNames.map((name) => ({
  name,
  note: SECTION_NOTES[name],
  rows: flattenSection(configValue[name]),
}));

// Real network mode, read straight from the resolved economics section.
const economics = isPlainObject(configValue.economics)
  ? configValue.economics
  : {};
const networkMode =
  typeof economics.network === "string" ? economics.network : "testnet";
const mainnetConfirmed = economics.mainnet_confirmed === true;

/* ----------------------------------------------------------- sql admin surface */

const SQL_ROWS: { stmt: string; what: string }[] = [
  {
    stmt: "SELECT * FROM p2p_info();",
    what: "Node identity, version, and connected peer count — the health check.",
  },
  {
    stmt: "SELECT * FROM p2p_query('SELECT …', data_class:='internal');",
    what: "Run a query across the grid, tagging the data sensitivity class.",
  },
  {
    stmt: "CALL p2p_share('view_name', 's3://…');",
    what: "Publish a named logical view backed by an object-store path.",
  },
  {
    stmt: "SELECT * FROM p2p_join(…);",
    what: "Distributed join across remote shared datasets.",
  },
  {
    stmt: "CALL p2p_set('transport.compression', 'zstd');",
    what: "Override a config key at runtime (call-layer precedence).",
  },
  {
    stmt: "CALL p2p_network('testnet');",
    what: "Switch the settlement network; mainnet requires explicit opt-in.",
  },
];

/* ---------------------------------------------------------------- trait seams */

const TRAITS: { name: string; note: string }[] = [
  {
    name: "DataFormat",
    note: "Maps a format to the DuckDB extensions it needs (parquet, delta, iceberg, …) and loads them on demand.",
  },
  {
    name: "StorageProvider",
    note: "Turns a ScopedCredential into a CREATE SECRET. Impls: S3, Azure, Gcs, Https, Local.",
  },
  {
    name: "query engine",
    note: "Swap mock ↔ a locked-down DuckDB executor, feature-gated at build time.",
  },
  {
    name: "settlement",
    note: "Pluggable economics backend: noop, mock, or the on-chain TON layer.",
  },
];

export default function ConfigPage() {
  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Settings2 />}
        title="Configuration"
        description="Layered, validated config — defaults < file < env < per-call; nothing is hard-coded. The values shown are this node's REAL resolved defaults (serialized from the Rust GridConfig); the file / env / per-call layers override these at runtime."
      />

      <Explainer
        what="Every setting the node runs with, and where each value comes from. Config is layered: a built-in default, overridden by a file, then environment variables, then a per-query override."
        impact="Operators tune behavior without touching code — nothing is hard-coded — and you can see exactly what is in effect right now."
      />

      {/* Resolution order */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Layers className="size-4 text-primary" /> Resolution order
            <InfoHint text="Later layers win: default < file < env < per-call." />
          </CardTitle>
          <CardDescription>
            Each layer overrides the one before it. The last writer wins, so a
            single call argument can trump the file and the environment.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <ol className="grid gap-3 lg:grid-cols-[repeat(4,minmax(0,1fr))] lg:items-stretch">
            {RESOLUTION.map((r, i) => (
              <li key={r.step} className="contents lg:block">
                <div className="bg-muted/40 relative flex h-full flex-col rounded-lg border p-4">
                  <div className="flex items-center gap-2">
                    <span className="bg-primary/10 text-primary flex size-6 items-center justify-center rounded-md font-mono text-xs font-semibold">
                      {r.step}
                    </span>
                    <span className="text-sm font-semibold">{r.layer}</span>
                  </div>
                  <p className="text-muted-foreground mt-2 text-xs">{r.note}</p>
                  {i < RESOLUTION.length - 1 ? (
                    <ArrowRight className="text-muted-foreground/50 absolute top-1/2 -right-2.5 hidden size-4 -translate-y-1/2 lg:block" />
                  ) : null}
                </div>
              </li>
            ))}
          </ol>
        </CardContent>
      </Card>

      {/* Effective configuration */}
      <Card>
        <CardHeader>
          <CardTitle>Effective configuration</CardTitle>
          <CardDescription>
            The full resolved <span className="font-mono">GridConfig</span> as
            this node sees it — {sections.length} sections, serialized live from
            the Rust config crate. This snapshot is the{" "}
            <Badge variant="muted">default</Badge> layer, so every value is its
            built-in default; the file, environment and per-call layers override
            these at runtime.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Accordion type="multiple" className="border-t">
            {sections.map((section) => (
              <AccordionItem key={section.name} value={section.name}>
                <AccordionTrigger>
                  <span className="flex flex-1 items-baseline justify-between gap-3 pr-2">
                    <span className="font-mono">[{section.name}]</span>
                    <span className="text-muted-foreground text-xs font-normal">
                      {section.rows.length} keys
                    </span>
                  </span>
                </AccordionTrigger>
                <AccordionContent>
                  {section.note ? (
                    <p className="text-muted-foreground mb-3 text-xs">
                      {section.note}
                    </p>
                  ) : null}
                  <div className="overflow-hidden rounded-lg border">
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead className="pl-4">Key</TableHead>
                          <TableHead>Value</TableHead>
                          <TableHead className="pr-4 text-right">Source</TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {section.rows.map((row) => (
                          <TableRow key={row.key}>
                            <TableCell className="pl-4 font-mono text-xs whitespace-normal">
                              {row.key}
                            </TableCell>
                            <TableCell className="font-mono text-xs whitespace-normal break-all">
                              {row.value}
                            </TableCell>
                            <TableCell className="pr-4 text-right">
                              <Badge variant="muted">default</Badge>
                            </TableCell>
                          </TableRow>
                        ))}
                      </TableBody>
                    </Table>
                  </div>
                </AccordionContent>
              </AccordionItem>
            ))}
          </Accordion>
        </CardContent>
      </Card>

      {/* Example p2p.toml */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <FileText className="size-4 text-primary" /> Example p2p.toml
            (documented)
          </CardTitle>
          <CardDescription>
            The real, fully-commented{" "}
            <span className="font-mono">p2p.example.toml</span> shipped with the
            project. Every key is optional — omit it to inherit the documented
            default shown above. Unknown keys are a hard error (fail fast).
          </CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="bg-muted/40 max-h-[480px] overflow-auto rounded-lg border p-4 font-mono text-xs leading-relaxed">
            {config.exampleToml}
          </pre>
        </CardContent>
      </Card>

      {/* SQL admin surface */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Terminal className="size-4 text-primary" /> SQL admin surface
          </CardTitle>
          <CardDescription>
            The whole grid is driven from SQL — these are the business-user entry
            points. Zero-config quickstart: install the extension and the calls
            below work out-of-the-box against the built-in defaults; no file or
            environment setup is required to run your first query.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Statement</TableHead>
                <TableHead className="pr-6">What it does</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {SQL_ROWS.map((r) => (
                <TableRow key={r.stmt}>
                  <TableCell className="pl-6 font-mono text-xs whitespace-normal">
                    {r.stmt}
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-sm whitespace-normal">
                    {r.what}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Network mode + trait seams */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between gap-2">
              <CardTitle className="flex items-center gap-2">
                <Globe className="size-4 text-primary" /> Network mode
                <InfoHint text="testnet uses play money; mainnet uses real funds and is guarded behind an explicit opt-in." />
              </CardTitle>
              <Badge variant="info">{networkMode}</Badge>
            </div>
            <CardDescription>
              Which TON network settlement transactions are signed against. Read
              live from <span className="font-mono">economics.network</span>.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="bg-[var(--info)]/5 ring-[var(--info)]/30 rounded-lg border p-3 ring-1">
                <div className="flex items-center gap-2">
                  <Badge variant="info">{networkMode}</Badge>
                  <span className="text-xs font-medium">current</span>
                </div>
                <p className="text-muted-foreground mt-2 text-xs">
                  Play money. Stakes, escrow and slashing all run, but no
                  real-value funds are ever at risk. The default for development.
                </p>
              </div>
              <div className="bg-muted/30 rounded-lg border p-3">
                <div className="flex items-center gap-2">
                  <Badge variant="warn">mainnet</Badge>
                  <span className="flex items-center gap-1 text-xs font-medium">
                    <ShieldCheck className="size-3.5" /> opt-in
                  </span>
                </div>
                <p className="text-muted-foreground mt-2 text-xs">
                  Real funds. Disabled unless explicitly enabled, so a stray
                  config can never accidentally move value.
                </p>
              </div>
            </div>
            <Separator />
            <p className="text-muted-foreground text-xs">
              Switching to mainnet requires an explicit opt-in — set{" "}
              <span className="font-mono">network = &quot;mainnet&quot;</span>{" "}
              and flip{" "}
              <span className="font-mono">mainnet_confirmed = true</span> (now{" "}
              <span className="font-mono">{String(mainnetConfirmed)}</span>).
              There is no implicit upgrade path from testnet.
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Boxes className="size-4 text-primary" /> Pluggable trait seams
            </CardTitle>
            <CardDescription>
              Extensibility points — implement the trait to add a backend.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <ul className="space-y-3">
              {TRAITS.map((t) => (
                <li key={t.name} className="flex flex-col gap-1">
                  <Badge variant="outline" className="w-fit font-mono">
                    {t.name}
                  </Badge>
                  <span className="text-muted-foreground text-xs">{t.note}</span>
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      </div>

      <div>
        <SectionTitle hint="config">Notes</SectionTitle>
        <p className="text-muted-foreground text-xs">
          Every key above is validated on load; an unknown key or out-of-range
          value fails fast at startup rather than silently degrading at runtime.
        </p>
      </div>
    </div>
  );
}
