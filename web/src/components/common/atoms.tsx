import * as React from "react";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { InfoHint } from "./explain";
import type { AttestationLevel, DataClass, JobStatus, Verdict } from "@/lib/types";

/* ----------------------------------------------------------------- headers */

export function PageHeader({
  title,
  description,
  icon,
  children,
}: {
  title: string;
  description?: string;
  icon?: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
      <div className="flex items-start gap-3">
        {icon ? (
          <div className="bg-primary/10 text-primary mt-0.5 flex size-9 items-center justify-center rounded-lg [&_svg]:size-5">
            {icon}
          </div>
        ) : null}
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
          {description ? (
            <p className="text-muted-foreground mt-1 max-w-2xl text-sm">
              {description}
            </p>
          ) : null}
        </div>
      </div>
      {children ? <div className="flex items-center gap-2">{children}</div> : null}
    </div>
  );
}

export function SectionTitle({
  children,
  hint,
  info,
  className,
}: {
  children: React.ReactNode;
  hint?: string;
  /** optional plain-language tooltip explaining the section */
  info?: string;
  className?: string;
}) {
  return (
    <div className={cn("mb-3 flex items-baseline justify-between", className)}>
      <h2 className="text-muted-foreground flex items-center text-sm font-semibold tracking-wide uppercase">
        {children}
        {info ? <InfoHint text={info} className="ml-1.5" /> : null}
      </h2>
      {hint ? <span className="text-muted-foreground text-xs">{hint}</span> : null}
    </div>
  );
}

/* ------------------------------------------------------------------- stats */

export function Stat({
  label,
  value,
  sub,
  icon,
  accent,
  hint,
  className,
}: {
  label: string;
  value: React.ReactNode;
  sub?: React.ReactNode;
  icon?: React.ReactNode;
  accent?: "primary" | "ok" | "warn" | "info" | "destructive";
  /** optional plain-language tooltip explaining the metric */
  hint?: string;
  className?: string;
}) {
  const accentColor =
    accent === "ok"
      ? "text-[var(--ok)]"
      : accent === "warn"
        ? "text-[var(--warn)]"
        : accent === "info"
          ? "text-[var(--info)]"
          : accent === "destructive"
            ? "text-destructive"
            : "text-primary";
  return (
    <div className={cn("bg-card rounded-xl border p-4", className)}>
      <div className="flex items-center justify-between">
        <span className="text-muted-foreground flex items-center text-xs font-medium">
          {label}
          {hint ? <InfoHint text={hint} className="ml-1" /> : null}
        </span>
        {icon ? <span className={cn("[&_svg]:size-4", accentColor)}>{icon}</span> : null}
      </div>
      <div className="mt-2 text-2xl font-semibold tracking-tight tabular-nums">
        {value}
      </div>
      {sub ? <div className="text-muted-foreground mt-1 text-xs">{sub}</div> : null}
    </div>
  );
}

/* -------------------------------------------------------------- score bars */

export function ScoreBar({
  value,
  className,
  showValue = true,
}: {
  value: number; // 0..1
  className?: string;
  showValue?: boolean;
}) {
  const color =
    value >= 0.85
      ? "var(--ok)"
      : value >= 0.6
        ? "var(--warn)"
        : "var(--destructive)";
  return (
    <div className={cn("flex items-center gap-2", className)}>
      <div className="bg-secondary h-1.5 w-full overflow-hidden rounded-full">
        <div
          className="h-full rounded-full"
          style={{ width: `${Math.round(value * 100)}%`, background: color }}
        />
      </div>
      {showValue ? (
        <span className="text-xs tabular-nums text-muted-foreground w-9 text-right">
          {value.toFixed(2)}
        </span>
      ) : null}
    </div>
  );
}

export function CapacityBar({
  used,
  total,
  className,
}: {
  used: number;
  total: number;
  className?: string;
}) {
  const ratio = total === 0 ? 0 : used / total;
  const color =
    ratio > 0.85 ? "var(--warn)" : ratio > 0.5 ? "var(--info)" : "var(--ok)";
  return (
    <div className={cn("bg-secondary h-1.5 w-full overflow-hidden rounded-full", className)}>
      <div
        className="h-full rounded-full"
        style={{ width: `${Math.round(ratio * 100)}%`, background: color }}
      />
    </div>
  );
}

/* ------------------------------------------------------------------ badges */

export function AttestationBadge({
  level,
  className,
}: {
  level: AttestationLevel;
  className?: string;
}) {
  const map = {
    L0: { v: "muted" as const, t: "L0 · anon" },
    L1: { v: "info" as const, t: "L1 · TPM" },
    L2: { v: "ok" as const, t: "L2 · TEE" },
  };
  const m = map[level];
  return (
    <Badge variant={m.v} className={cn("font-mono", className)}>
      {m.t}
    </Badge>
  );
}

export function DataClassBadge({ value }: { value: DataClass }) {
  const map = {
    Public: "muted" as const,
    Internal: "info" as const,
    Sensitive: "warn" as const,
  };
  return <Badge variant={map[value]}>{value}</Badge>;
}

export function StatusBadge({ status }: { status: JobStatus }) {
  const map: Record<JobStatus, { v: Parameters<typeof Badge>[0]["variant"]; t: string }> = {
    running: { v: "info", t: "running" },
    verified: { v: "ok", t: "verified" },
    settled: { v: "ok", t: "settled" },
    failed: { v: "destructive", t: "failed" },
    queued: { v: "muted", t: "queued" },
  };
  const m = map[status];
  return <Badge variant={m.v}>{m.t}</Badge>;
}

export function VerdictBadge({ verdict }: { verdict: Verdict }) {
  const ok = verdict === "Correct";
  const provider = ["Incorrect", "Timeout", "Malformed"].includes(verdict);
  const v = ok ? "ok" : provider ? "destructive" : "warn";
  return <Badge variant={v}>{verdict}</Badge>;
}

/* --------------------------------------------------------------- dot + kv */

export function Dot({
  status = "ok",
  pulse,
}: {
  status?: "ok" | "warn" | "destructive" | "muted" | "info";
  pulse?: boolean;
}) {
  const color = {
    ok: "var(--ok)",
    warn: "var(--warn)",
    destructive: "var(--destructive)",
    info: "var(--info)",
    muted: "var(--muted-foreground)",
  }[status];
  return (
    <span className="relative flex size-2.5 items-center justify-center">
      {pulse ? (
        <span
          className="absolute inline-flex size-full animate-ping rounded-full opacity-60"
          style={{ background: color }}
        />
      ) : null}
      <span
        className="relative inline-flex size-2 rounded-full"
        style={{ background: color }}
      />
    </span>
  );
}

export function KV({
  label,
  children,
  className,
}: {
  label: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex items-center justify-between gap-4 py-1.5", className)}>
      <dt className="text-muted-foreground text-sm">{label}</dt>
      <dd className="text-sm font-medium text-right tabular-nums">{children}</dd>
    </div>
  );
}
