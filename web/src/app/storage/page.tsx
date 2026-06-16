import type { Metadata } from "next";
import {
  Cloud,
  Database,
  FileText,
  HardDrive,
  Key,
  KeyRound,
  Lock,
  LockKeyhole,
  Server,
  Boxes,
  CheckCircle2,
  AlertTriangle,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { KV, PageHeader, SectionTitle, Stat } from "@/components/common/atoms";
import { Explainer, InfoHint } from "@/components/common/explain";
import { config } from "@/lib/data";
import { durationSecs, inFuture, NOW } from "@/lib/format";

export const metadata: Metadata = {
  title: "Storage",
  description: "Cloud object-storage providers, formats, per-job scoped credentials, and the encryption security boundary for the Duckton grid.",
};

/* ------------------------------------------- real node storage config (live) */

// The real `storage` section of the running node's GridConfig. Read defensively:
// only some keys are populated in any given run (remote access is off here, so
// endpoint/region/url_style/use_ssl are null).
const storageCfg = (config.value.storage ?? {}) as Record<string, unknown>;

const str = (v: unknown): string | null =>
  typeof v === "string" && v.length > 0 ? v : null;
const strList = (v: unknown): string[] =>
  Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];

const defaultProvider = str(storageCfg.provider); // e.g. "local-fake"
const enabledProviders = strList(storageCfg.enabled_providers);
const enabledFormats = strList(storageCfg.enabled_formats); // e.g. csv/json/parquet

/* --------------------------------------------------------------- providers */

type Provider = {
  name: string;
  icon: React.ReactNode;
  mechanism: string;
  scheme: string;
  // which real `enabled_providers` keys make this card "configured" on this node
  match: string[];
  note?: string;
};

const providers: Provider[] = [
  {
    name: "AWS S3",
    icon: <Cloud />,
    mechanism: "httpfs + aws",
    scheme: "s3://",
    match: ["s3", "aws"],
  },
  {
    name: "MinIO / S3-compatible",
    icon: <Server />,
    mechanism: "httpfs + aws",
    scheme: "s3://",
    match: ["minio", "s3"],
    note: "Self-hosted; requires path-style addressing.",
  },
  {
    name: "Azure ADLS",
    icon: <Cloud />,
    mechanism: "azure",
    scheme: "abfss:// / az://",
    match: ["azure", "adls", "abfss"],
  },
  {
    name: "Google Cloud Storage",
    icon: <Cloud />,
    mechanism: "httpfs (S3-interop) / gcs",
    scheme: "gcs://",
    match: ["gcs", "gcp", "google"],
  },
  {
    name: "Generic HTTPS",
    icon: <HardDrive />,
    mechanism: "httpfs",
    scheme: "https://",
    match: ["https", "http", "httpfs"],
  },
  {
    name: "Local",
    icon: <HardDrive />,
    mechanism: "local files",
    scheme: "file://",
    match: ["local", "local-fake", "file"],
  },
];

// Is a provider card backed by the real node config this run?
const isConfigured = (p: Provider) =>
  enabledProviders.some((e) => p.match.includes(e)) ||
  (defaultProvider != null && p.match.includes(defaultProvider));

// Non-secret connection knobs for the S3 / MinIO endpoint (illustrative — the
// running loopback node has remote access disabled, so these are null in config).
const s3Knobs: { label: string; value: string }[] = [
  { label: "endpoint", value: "minio.local:9000" },
  { label: "url_style", value: "path (MinIO) / vhost (AWS)" },
  { label: "use_ssl", value: "false" },
  { label: "region", value: "us-east-1" },
];

// Effective storage config rows, built from whatever real keys are present.
type ConfigRow = { label: string; value: string };
const effectiveConfig: ConfigRow[] = [];
const pushRow = (label: string, value: string | null) => {
  if (value != null) effectiveConfig.push({ label, value });
};
const boolRow = (label: string, v: unknown) =>
  pushRow(label, typeof v === "boolean" ? String(v) : null);
const secsRow = (label: string, v: unknown) =>
  pushRow(label, typeof v === "number" ? durationSecs(v) : null);

pushRow("default provider", defaultProvider);
pushRow("enabled_providers", enabledProviders.length ? enabledProviders.join(", ") : null);
pushRow("enabled_formats", enabledFormats.length ? enabledFormats.join(", ") : null);
boolRow("enable_remote_access", storageCfg.enable_remote_access);
boolRow("require_extensions", storageCfg.require_extensions);
secsRow("credential_ttl", storageCfg.credential_ttl_secs);
secsRow("key_ttl", storageCfg.key_ttl_secs);
pushRow("endpoint", str(storageCfg.endpoint));
pushRow("url_style", str(storageCfg.url_style));
boolRow("use_ssl", storageCfg.use_ssl);
pushRow("region", str(storageCfg.region));

/* ----------------------------------------------------------------- formats */

const formats: { name: string; ext: string; key: string; note: string }[] = [
  { name: "Parquet", ext: "parquet", key: "parquet", note: "Core / bundled — columnar, the default lake format." },
  { name: "CSV", ext: "core", key: "csv", note: "Built-in reader/writer; schema sniffing." },
  { name: "JSON", ext: "json", key: "json", note: "Core / bundled — newline-delimited & nested." },
  { name: "Delta Lake", ext: "delta + httpfs", key: "delta", note: "Transaction-log aware table reads." },
  { name: "Apache Iceberg", ext: "iceberg", key: "iceberg", note: "Snapshot / manifest-based table reads." },
];

// Which formats the running node actually has enabled.
const isFormatEnabled = (key: string) => enabledFormats.includes(key);

/* ------------------------------------------------------- scoped credential */

// The ScopedCredential shape delivered to a chosen worker per job.
const scopedCredential = {
  provider: "s3",
  token: "opaque STS session / SAS / downscoped token",
  prefix: "s3://acme-lake/orders/2026/",
  expiresAtMs: NOW + 15 * 60_000,
};

const credFlow = [
  "Requester mints short-lived, downscoped credentials (read-only, one prefix).",
  "Seals them to the chosen worker's node / enclave key.",
  "Worker runs CREATE SECRET (… SCOPE …) with the delivered token.",
  "Worker reads only that prefix over HTTPS — nothing else in the bucket.",
];

const createSecretSql = `CREATE SECRET (
  TYPE      s3,
  KEY_ID    …,
  SECRET    …,
  ENDPOINT  'minio.local:9000',
  URL_STYLE 'path',
  USE_SSL   false,
  REGION    …,
  SCOPE     's3://bucket/prefix/'
);`;

/* --------------------------------------------------- security boundary rows */

const boundary: {
  phase: string;
  icon: React.ReactNode;
  status: "ok" | "warn";
  badge: string;
  detail: string;
}[] = [
  {
    phase: "In transit",
    icon: <Lock />,
    status: "ok",
    badge: "solved",
    detail: "QUIC + TLS 1.3 with mutual authentication between peers.",
  },
  {
    phase: "At rest",
    icon: <LockKeyhole />,
    status: "ok",
    badge: "solved",
    detail:
      "Parquet Modular Encryption — the stored bytes are meaningless without the per-job key.",
  },
  {
    phase: "In use",
    icon: <KeyRound />,
    status: "warn",
    badge: "hardware-dependent",
    detail:
      "Only guaranteed on L2 confidential-computing hardware. Commodity laptops cannot guarantee RAM confidentiality, so sensitive data is routed only to attested L2 hosts, while laptops handle public data under quorum + reputation.",
  },
];

/* ------------------------------------------------------------------- page */

export default function StoragePage() {
  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Database />}
        title="Storage"
        description="Hosts are pure compute. Data lives in cloud object storage, encrypted at rest; per-job scoped, short-lived credentials are delivered encrypted to the chosen worker."
      />

      <Explainer
        what="Where the data lives (cloud object storage, encrypted at rest) and how a worker is handed a temporary, read-only credential to just the slice of data one job needs."
        impact="Hosts are pure compute — they never hold your data or any long-lived keys, so a machine can't keep or leak your dataset."
      />

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-5">
        <Stat
          label="Default provider"
          value={defaultProvider ?? "—"}
          sub={`${enabledProviders.length || providers.length} provider${
            (enabledProviders.length || providers.length) === 1 ? "" : "s"
          } supported`}
          icon={<Cloud />}
          accent="info"
        />
        <Stat
          label="Formats enabled"
          value={enabledFormats.length || formats.length}
          sub={enabledFormats.length ? enabledFormats.join(" · ") : "readable table formats"}
          icon={<FileText />}
          accent="primary"
        />
        <Stat
          label="At rest"
          value="Parquet ME"
          sub="Modular Encryption"
          icon={<LockKeyhole />}
          accent="ok"
          hint="The files themselves are encrypted, so stored bytes are useless without the per-job key."
        />
        <Stat
          label="Credentials"
          value="per-job scoped"
          sub="short-lived, sealed"
          icon={<Key />}
          accent="warn"
          hint="A short-lived, read-only key limited to one data prefix; even a malicious worker reads only that slice, briefly."
        />
        <Stat label="In transit" value="QUIC TLS 1.3" sub="mutual auth" icon={<Lock />} accent="ok" />
      </div>

      {/* Providers grid */}
      <div>
        <SectionTitle
          hint="DuckDB filesystem extensions"
          info="The cloud or local stores the data can live in; hosts read from these, they never store your data themselves."
        >
          Providers
        </SectionTitle>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {providers.map((p) => {
            const configured = isConfigured(p);
            const isDefault =
              defaultProvider != null && p.match.includes(defaultProvider);
            return (
            <Card key={p.name}>
              <CardHeader>
                <div className="flex items-start justify-between">
                  <CardTitle className="flex items-center gap-2 text-base">
                    <span className="bg-primary/10 text-primary flex size-7 items-center justify-center rounded-md [&_svg]:size-4">
                      {p.icon}
                    </span>
                    {p.name}
                  </CardTitle>
                  <div className="flex items-center gap-1.5">
                    {isDefault ? <Badge variant="info">default</Badge> : null}
                    <Badge variant={configured ? "ok" : "muted"}>
                      {configured ? "configured" : "available"}
                    </Badge>
                  </div>
                </div>
              </CardHeader>
              <CardContent className="space-y-2">
                <div className="flex flex-wrap items-center gap-2 text-xs">
                  <Badge variant="info" className="font-mono">
                    {p.mechanism}
                  </Badge>
                  <code className="bg-muted text-muted-foreground rounded px-1.5 py-0.5 font-mono">
                    {p.scheme}
                  </code>
                </div>
                {p.note ? (
                  <p className="text-muted-foreground text-xs">{p.note}</p>
                ) : null}

                {/* S3 / MinIO connection knobs */}
                {p.name === "MinIO / S3-compatible" ? (
                  <>
                    <Separator className="my-1" />
                    <dl className="text-xs">
                      {s3Knobs.map((k) => (
                        <KV key={k.label} label={k.label} className="py-1">
                          <span className="font-mono">{k.value}</span>
                        </KV>
                      ))}
                    </dl>
                    <p className="text-muted-foreground text-xs">
                      <span className="text-foreground font-medium">No secrets here —</span> the
                      access key / secret are <span className="text-foreground">never</span> in
                      config; they arrive per job, encrypted.
                    </p>
                  </>
                ) : null}
              </CardContent>
            </Card>
            );
          })}
        </div>
      </div>

      {/* Effective storage config */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Boxes className="size-4 text-primary" /> Effective storage config
          </CardTitle>
          <CardDescription>
            The live <code className="font-mono">storage</code> section of this node&apos;s
            GridConfig from the snapshot. Remote access is off on the loopback run, so endpoint /
            region / TLS knobs are unset here.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {effectiveConfig.length > 0 ? (
            <dl className="grid gap-x-8 sm:grid-cols-2">
              {effectiveConfig.map((row) => (
                <KV key={row.label} label={row.label}>
                  {row.label === "default provider" ? (
                    <span className="inline-flex items-center gap-1.5">
                      <span className="font-mono">{row.value}</span>
                      <Badge variant="ok">configured</Badge>
                    </span>
                  ) : (
                    <span className="font-mono">{row.value}</span>
                  )}
                </KV>
              ))}
            </dl>
          ) : (
            <p className="text-muted-foreground text-sm">No storage keys present in config.</p>
          )}
        </CardContent>
      </Card>

      {/* Formats */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <FileText className="size-4 text-primary" /> Formats
          </CardTitle>
          <CardDescription>
            Table formats DuckDB can read directly from object storage. The{" "}
            <Badge variant="ok">enabled</Badge> tag marks the formats this node has turned on in
            its live config.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Format</TableHead>
                <TableHead>DuckDB extension(s)</TableHead>
                <TableHead>On this node</TableHead>
                <TableHead className="pr-6">Note</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {formats.map((f) => (
                <TableRow key={f.name}>
                  <TableCell className="pl-6 font-medium">{f.name}</TableCell>
                  <TableCell>
                    <Badge variant="muted" className="font-mono">
                      {f.ext}
                    </Badge>
                  </TableCell>
                  <TableCell>
                    {isFormatEnabled(f.key) ? (
                      <Badge variant="ok">enabled</Badge>
                    ) : (
                      <Badge variant="muted">available</Badge>
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground pr-6 text-xs whitespace-normal">
                    {f.note}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* Scoped credential + CREATE SECRET */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Key className="size-4 text-primary" /> Per-job scoped credential
            </CardTitle>
            <CardDescription>
              The <code className="font-mono">ScopedCredential</code> sealed to the winning worker.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <dl>
              <KV label="provider">
                <span className="font-mono">{scopedCredential.provider}</span>
              </KV>
              <KV label="token">
                <span className="text-muted-foreground font-mono text-xs">
                  {scopedCredential.token}
                </span>
              </KV>
              <KV label="prefix">
                <span className="font-mono text-xs">{scopedCredential.prefix}</span>
              </KV>
              <KV label="expires_at">
                <span className="inline-flex items-center gap-1.5">
                  <Badge variant="warn" className="font-mono">
                    {inFuture(scopedCredential.expiresAtMs)}
                  </Badge>
                </span>
              </KV>
            </dl>
            <p className="text-muted-foreground text-xs">
              The <span className="font-mono">prefix</span> is a read-only scope — the token cannot
              touch anything outside it.
            </p>

            <Separator />

            <div>
              <SectionTitle
                className="mb-2"
                info="How the read-only, one-job key is minted, sealed to the chosen worker, and used — then expires."
              >
                Delivery flow
              </SectionTitle>
              <ol className="space-y-2">
                {credFlow.map((step, i) => (
                  <li key={i} className="flex gap-3 text-sm">
                    <span className="bg-primary/10 text-primary flex size-5 shrink-0 items-center justify-center rounded-full font-mono text-xs">
                      {i + 1}
                    </span>
                    <span className="text-muted-foreground">{step}</span>
                  </li>
                ))}
              </ol>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <KeyRound className="size-4 text-primary" /> Sample <code className="font-mono">CREATE SECRET</code>
            </CardTitle>
            <CardDescription>
              What the worker runs locally once the scoped token is unsealed.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <pre className="bg-muted/50 overflow-x-auto rounded-lg border p-4 font-mono text-xs leading-relaxed">
              <code>{createSecretSql}</code>
            </pre>
            <div className="flex flex-wrap gap-2 text-xs">
              <Badge variant="outline" className="font-mono">
                TYPE s3
              </Badge>
              <Badge variant="outline" className="font-mono">
                URL_STYLE path
              </Badge>
              <Badge variant="outline" className="font-mono">
                SCOPE prefix
              </Badge>
            </div>
            <p className="text-muted-foreground text-xs">
              <span className="text-foreground font-medium">KEY_ID</span> /{" "}
              <span className="text-foreground font-medium">SECRET</span> come from the per-job
              token — they live only in the worker&apos;s process for the life of the job, never on
              disk or in the grid catalog.
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Encryption & honest security boundary */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Lock className="size-4 text-primary" /> Encryption &amp; the honest security boundary
          </CardTitle>
          <CardDescription>
            Two of the three data states are cryptographically solved; the third depends on the
            host&apos;s hardware tier.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {boundary.map((b) => {
            const color = b.status === "ok" ? "var(--ok)" : "var(--warn)";
            return (
              <div
                key={b.phase}
                className="flex flex-col gap-2 rounded-lg border p-4 sm:flex-row sm:items-start sm:gap-4"
                style={{ borderColor: `color-mix(in oklab, ${color} 30%, transparent)` }}
              >
                <div
                  className="flex size-8 shrink-0 items-center justify-center rounded-lg [&_svg]:size-4"
                  style={{
                    background: `color-mix(in oklab, ${color} 15%, transparent)`,
                    color,
                  }}
                >
                  {b.icon}
                </div>
                <div className="flex-1">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-semibold">{b.phase}</span>
                    {b.phase === "In use" ? (
                      <InfoHint text="True memory privacy needs L2 confidential hardware; commodity laptops cannot guarantee it." />
                    ) : null}
                    <Badge variant={b.status === "ok" ? "ok" : "warn"}>
                      {b.status === "ok" ? (
                        <CheckCircle2 className="size-3" />
                      ) : (
                        <AlertTriangle className="size-3" />
                      )}
                      {b.badge}
                    </Badge>
                  </div>
                  <p className="text-muted-foreground mt-1 text-xs leading-relaxed">{b.detail}</p>
                </div>
              </div>
            );
          })}
          <div className="text-muted-foreground flex items-center gap-2 text-xs">
            <Boxes className="size-3.5" />
            Net effect: storage operators and host operators both see only ciphertext; plaintext
            exists only inside an L2 enclave or, for public data, transiently in a laptop&apos;s RAM
            under quorum + reputation guards.
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
