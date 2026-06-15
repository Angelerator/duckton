"use client";

import * as React from "react";
import { Banknote, Coins, Percent, Users, Wallet } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { Slider } from "@/components/ui/slider";
import { SectionTitle } from "@/components/common/atoms";
import { computeEarning, earningExample } from "@/lib/data";
import type { EarningTerm } from "@/lib/data";
import { num } from "@/lib/format";

/* ------------------------------------------------------------------ formula */

const FORMULA: { lhs: string; rhs: string }[] = [
  { lhs: "fee", rhs: "φ · B" },
  { lhs: "commission_j", rhs: "κ · B   (to each agreeing verifier)" },
  { lhs: "winner", rhs: "B − φ·B − κ·B·participants" },
  { lhs: "total", rhs: "fee + Σ commission + winner = B" },
];

/* ----------------------------------------------------------------- controls */

type Knob = {
  key: "B" | "phi" | "kappa" | "participants";
  label: string;
  sym: string;
  min: number;
  max: number;
  step: number;
  fmt: (v: number) => string;
  hint: string;
};

const f2 = (v: number) => v.toFixed(2);

export function EarningsCalculator() {
  // Initialize from the deterministic real example so the first render matches SSR.
  const [B, setB] = React.useState(earningExample.B);
  const [phi, setPhi] = React.useState(earningExample.phi);
  const [kappa, setKappa] = React.useState(earningExample.kappa);
  const [participants, setParticipants] = React.useState(
    earningExample.participants
  );

  function reset() {
    setB(earningExample.B);
    setPhi(earningExample.phi);
    setKappa(earningExample.kappa);
    setParticipants(earningExample.participants);
  }

  // Build a modified copy of the real example and reuse the canonical reducer.
  const terms = React.useMemo<EarningTerm[]>(
    () => computeEarning({ ...earningExample, B, phi, kappa, participants }),
    [B, phi, kappa, participants]
  );

  const byKey = React.useMemo(
    () => Object.fromEntries(terms.map((t) => [t.key, t])),
    [terms]
  );
  const winner = byKey.winner?.value ?? 0;
  const fee = -(byKey.fee?.value ?? 0); // stored negative in the reducer
  const commissionEach = byKey.commissionEach?.value ?? 0;
  const total = fee + commissionEach * participants + winner;

  // Bars are scaled against the escrow ceiling B so they read comparably.
  const maxAbs = Math.max(B, ...terms.map((t) => Math.abs(t.value)));

  const knobs: Knob[] = [
    {
      key: "B",
      label: "Escrow (max bid)",
      sym: "B",
      min: 10,
      max: 500,
      step: 10,
      fmt: (v) => `${num(Math.round(v))} TON`,
      hint: "requester locks this up front",
    },
    {
      key: "phi",
      label: "Platform fee",
      sym: "φ",
      min: 0,
      max: 0.2,
      step: 0.005,
      fmt: (v) => `${(v * 100).toFixed(1)}%`,
      hint: "protocol cut on the escrow",
    },
    {
      key: "kappa",
      label: "Participation commission",
      sym: "κ",
      min: 0,
      max: 0.1,
      step: 0.005,
      fmt: (v) => `${(v * 100).toFixed(1)}%`,
      hint: "flat cut to each agreeing verifier",
    },
    {
      key: "participants",
      label: "Agreeing verifiers",
      sym: "n",
      min: 0,
      max: 6,
      step: 1,
      fmt: (v) => `×${Math.round(v)}`,
      hint: "non-winners that matched the quorum hash",
    },
  ];

  const value: Record<Knob["key"], number> = { B, phi, kappa, participants };
  const setter: Record<Knob["key"], (v: number) => void> = {
    B: (v) => setB(Math.round(v)),
    phi: (v) => setPhi(Math.round(v * 1000) / 1000),
    kappa: (v) => setKappa(Math.round(v * 1000) / 1000),
    participants: (v) => setParticipants(Math.round(v)),
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <CardTitle className="flex items-center gap-2">
              <Banknote className="size-4 text-primary" /> Earnings model
            </CardTitle>
            <CardDescription>
              The REAL off-chain split the coordinator computed for this run&apos;s
              paid jobs. The winner takes the escrow remainder after the platform
              fee and a flat commission to each agreeing verifier. Drag the knobs
              to see how the split moves.
            </CardDescription>
          </div>
          <Button variant="outline" size="sm" onClick={reset}>
            Reset
          </Button>
        </div>
      </CardHeader>

      <CardContent className="space-y-6">
        {/* Formula */}
        <div className="bg-muted/40 rounded-lg border p-4">
          <SectionTitle className="mb-2">Off-chain split</SectionTitle>
          <dl className="grid gap-x-6 gap-y-1.5 font-mono text-xs sm:grid-cols-2">
            {FORMULA.map((row) => (
              <div key={row.lhs} className="flex items-baseline gap-2">
                <dt className="text-primary shrink-0">{row.lhs}</dt>
                <span className="text-muted-foreground">=</span>
                <dd className="text-foreground">{row.rhs}</dd>
              </div>
            ))}
          </dl>
        </div>

        {/* Headline outcomes */}
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <Outcome
            icon={<Wallet />}
            accent="ok"
            label="Winner payout"
            value={`${f2(winner)} TON`}
            sub="escrow remainder"
          />
          <Outcome
            icon={<Percent />}
            accent="warn"
            label="Platform fee"
            value={`${f2(fee)} TON`}
            sub={`φ = ${(phi * 100).toFixed(1)}%`}
          />
          <Outcome
            icon={<Users />}
            accent="primary"
            label="Commission / verifier"
            value={`${f2(commissionEach)} TON`}
            sub={`κ=${(kappa * 100).toFixed(1)}% · ×${participants}`}
          />
          <Outcome
            icon={<Coins />}
            accent="info"
            label="Total (= escrow B)"
            value={`${f2(total)} TON`}
            sub="nothing exceeds B"
          />
        </div>

        <div className="grid gap-6 lg:grid-cols-[minmax(0,360px)_minmax(0,1fr)]">
          {/* Sliders */}
          <div className="space-y-4">
            <SectionTitle className="mb-0" hint={`B = ${num(B)} TON`}>
              Inputs
            </SectionTitle>
            {knobs.map((k) => (
              <div key={k.key} className="space-y-1.5">
                <div className="flex items-baseline justify-between gap-2">
                  <label className="text-sm font-medium">
                    {k.label}{" "}
                    <span className="text-muted-foreground font-mono text-xs">
                      {k.sym}
                    </span>
                  </label>
                  <span className="text-sm font-semibold tabular-nums">
                    {k.fmt(value[k.key])}
                  </span>
                </div>
                <Slider
                  value={[value[k.key]]}
                  min={k.min}
                  max={k.max}
                  step={k.step}
                  onValueChange={(v) => setter[k.key](v[0])}
                  aria-label={k.label}
                />
                <div className="text-muted-foreground text-xs">{k.hint}</div>
              </div>
            ))}
          </div>

          {/* Breakdown */}
          <div>
            <SectionTitle className="mb-2" hint="bars scaled to escrow B">
              Breakdown
            </SectionTitle>
            <div className="space-y-2.5">
              {terms.map((t) => {
                const negative = t.kind === "neg";
                const out = t.kind === "out" || t.kind === "pos";
                const pctW = Math.min(100, (Math.abs(t.value) / maxAbs) * 100);
                return (
                  <div key={t.key} className="space-y-1">
                    <div className="flex items-baseline justify-between gap-3">
                      <span className="font-mono text-xs">{t.label}</span>
                      <span
                        className={cn(
                          "shrink-0 text-sm font-semibold tabular-nums",
                          negative && "text-destructive",
                          out && "text-[var(--ok)]"
                        )}
                      >
                        {negative ? "−" : ""}
                        {f2(Math.abs(t.value))}
                      </span>
                    </div>
                    <div className="bg-secondary h-1.5 w-full overflow-hidden rounded-full">
                      <div
                        className="h-full rounded-full"
                        style={{
                          width: `${pctW}%`,
                          background: negative
                            ? "var(--destructive)"
                            : out
                              ? "var(--ok)"
                              : "var(--primary)",
                        }}
                      />
                    </div>
                    <div className="text-muted-foreground text-xs">{t.note}</div>
                  </div>
                );
              })}
            </div>
          </div>
        </div>

        <Separator />

        {/* Real settled reference + on-chain design params */}
        <div className="grid gap-4 lg:grid-cols-2">
          <div className="bg-muted/40 rounded-lg border p-4">
            <SectionTitle className="mb-2">This run settled</SectionTitle>
            <p className="text-muted-foreground text-xs leading-relaxed">
              From a real escrow of{" "}
              <span className="text-foreground font-mono">
                {num(earningExample.B)} TON
              </span>
              : winner{" "}
              <span className="text-[var(--ok)] font-medium tabular-nums">
                {f2(earningExample.winnerTon)} TON
              </span>
              , fee{" "}
              <span className="text-foreground font-medium tabular-nums">
                {f2(earningExample.platformFeeTon)} TON
              </span>
              , commission{" "}
              <span className="text-foreground font-medium tabular-nums">
                {f2(earningExample.commissionEachTon)} TON
              </span>{" "}
              to each of {earningExample.participants} verifiers, totalling{" "}
              <span className="text-foreground font-medium tabular-nums">
                {f2(earningExample.totalTon)} TON
              </span>
              .
            </p>
          </div>

          <div className="bg-muted/40 rounded-lg border p-4">
            <SectionTitle className="mb-2">On-chain design params</SectionTitle>
            <dl className="grid grid-cols-3 gap-2 font-mono text-xs">
              <Param sym="ρ" value={earningExample.rho} />
              <Param sym="λq" value={earningExample.lambdaQ} />
              <Param sym="λs" value={earningExample.lambdaS} />
            </dl>
            <p className="text-muted-foreground mt-3 text-xs leading-relaxed">
              These govern the on-chain JobEscrow perf-split (a quality- and
              speed-weighted bonus). Off-chain, the winner simply takes the
              escrow remainder — the bonus is bounded by B, so no payout can
              ever exceed the escrowed maximum.
            </p>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function Param({ sym, value }: { sym: string; value: number }) {
  return (
    <div className="bg-card rounded-md border px-2 py-1.5 text-center">
      <div className="text-primary">{sym}</div>
      <div className="text-foreground mt-0.5 text-sm font-semibold tabular-nums">
        {value.toFixed(2)}
      </div>
    </div>
  );
}

function Outcome({
  icon,
  label,
  value,
  sub,
  accent,
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  sub: string;
  accent: "ok" | "info" | "warn" | "primary";
}) {
  const color =
    accent === "ok"
      ? "text-[var(--ok)]"
      : accent === "info"
        ? "text-[var(--info)]"
        : accent === "warn"
          ? "text-[var(--warn)]"
          : "text-primary";
  return (
    <div className="bg-card rounded-xl border p-4">
      <div className="flex items-center justify-between">
        <span className="text-muted-foreground text-xs font-medium">{label}</span>
        <span className={cn("[&_svg]:size-4", color)}>{icon}</span>
      </div>
      <div className="mt-2 text-2xl font-semibold tracking-tight tabular-nums">
        {value}
      </div>
      <div className="text-muted-foreground mt-1 text-xs">{sub}</div>
    </div>
  );
}
