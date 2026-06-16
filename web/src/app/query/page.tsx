"use client";

import * as React from "react";
import {
  AlertTriangle,
  ArrowRight,
  CheckCircle2,
  Coins,
  Cpu,
  Database,
  Hash,
  Lock,
  RefreshCw,
  Send,
  Sparkles,
  Terminal,
  Timer,
} from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import { Slider } from "@/components/ui/slider";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Progress } from "@/components/ui/progress";
import {
  AttestationBadge,
  Dot,
  PageHeader,
  ScoreBar,
  SectionTitle,
  VerdictBadge,
} from "@/components/common/atoms";
import { Explainer, InfoHint } from "@/components/common/explain";
import { CopyId } from "@/components/common/copy";
import { jobs } from "@/lib/data";
import { useLive, type QueryResult } from "@/lib/live";
import { ms, num, ton } from "@/lib/format";
import { cn } from "@/lib/utils";
import type {
  AttestationLevel,
  CandidateState,
  DataClass,
  JobCandidate,
  VerifyMode,
  Worker,
} from "@/lib/types";

/* --------------------------------------------------------------- constants */

type Fn = "p2p_query" | "p2p_join" | "p2p_share";

const STAGES = [
  "offer",
  "bidding",
  "dispatch",
  "executing",
  "commit",
  "verify",
  "settle",
] as const;
type Stage = (typeof STAGES)[number];

const STAGE_LABEL: Record<Stage, string> = {
  offer: "Offer",
  bidding: "Bidding",
  dispatch: "Dispatch",
  executing: "Executing",
  commit: "Commit",
  verify: "Verify",
  settle: "Settle",
};

// Deterministic pseudo-hash (no Math.random — keeps the demo reproducible).
// Used only for the ephemeral per-dispatch job_id / query_hash; the committed
// result hash below is the REAL one from the grid snapshot.
function fakeHash(seed: string): string {
  let h = 2166136261;
  for (let i = 0; i < seed.length; i++) {
    h ^= seed.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  const hex = (h >>> 0).toString(16).padStart(8, "0");
  return "b3:" + (hex.repeat(4) + "0123456789abcdef").slice(0, 32);
}

// Deterministic candidate state for a given stage index.
type CandState =
  | "bidding"
  | "dispatched"
  | "running"
  | "committed"
  | "won"
  | "reset";

function stateFor(
  stageIdx: number,
  rank: number,
  quorum: number,
): CandState {
  // rank 0 == fastest / winner. Stage order index from STAGES.
  if (stageIdx <= 0) return "bidding"; // offer
  if (stageIdx === 1) return "bidding"; // bidding
  if (stageIdx === 2) return "dispatched"; // dispatch
  if (stageIdx === 3) return "running"; // executing
  if (stageIdx === 4) {
    // commit: fastest commits first
    return rank === 0 ? "committed" : "running";
  }
  if (stageIdx === 5) {
    // verify: quorum members committed, slowest still running
    return rank < quorum ? "committed" : "running";
  }
  // settle: winner wins, quorum-1 others committed, the rest RESET
  if (rank === 0) return "won";
  if (rank < quorum) return "committed";
  return "reset";
}

const stateMeta: Record<CandState, { variant: Parameters<typeof Badge>[0]["variant"]; label: string }> = {
  bidding: { variant: "muted", label: "bidding" },
  dispatched: { variant: "info", label: "dispatched" },
  running: { variant: "info", label: "running" },
  committed: { variant: "secondary", label: "committed" },
  won: { variant: "ok", label: "won" },
  reset: { variant: "muted", label: "RESET" },
};

// Visual treatment for the REAL terminal candidate states the grid reports
// (the live CandidateState enum; superset of the simulation's CandState).
const realStateMeta: Record<
  CandidateState,
  { variant: Parameters<typeof Badge>[0]["variant"]; label: string }
> = {
  won: { variant: "ok", label: "won" },
  committed: { variant: "secondary", label: "committed" },
  reset: { variant: "muted", label: "RESET" },
  dispatched: { variant: "info", label: "dispatched" },
  bidding: { variant: "muted", label: "bidding" },
  rejected: { variant: "destructive", label: "rejected" },
};

// The REAL result the grid produced for this query (columns + rows + hash),
// seeded from the snapshot so the preview matches what the grid actually ran.
const REAL_JOB = jobs[0];
const RESULT = REAL_JOB.result;
const RESULT_HASH = REAL_JOB.resultHash ?? fakeHash("result:" + REAL_JOB.id);
const RESULT_ROWS = RESULT.rows.length;

const TICK_MS = 850;

const ATT_RANK: Record<AttestationLevel, number> = { L0: 0, L1: 1, L2: 2 };

// The data-class selection policy the live backend enforces: a minimum
// attestation tier (a hard gate) plus a trust floor. A host is eligible for a
// class only if it clears BOTH — so the quorum can never exceed the number of
// eligible hosts (else the grid rejects the dispatch with a policy error).
const CLASS_POLICY: Record<
  DataClass,
  { tier: number; att: AttestationLevel; trust: number; paid: boolean }
> = {
  Public: { tier: 0, att: "L0", trust: 0.7, paid: false },
  Internal: { tier: 1, att: "L1", trust: 0.85, paid: true },
  Sensitive: { tier: 2, att: "L2", trust: 0.8, paid: true },
};

/** Honest hosts that clear both gates of a data class's policy. */
function countEligibleHosts(list: Worker[], cls: DataClass): number {
  const p = CLASS_POLICY[cls];
  return list.filter(
    (w) => w.behavior === "honest" && ATT_RANK[w.attestation] >= p.tier && w.trust >= p.trust
  ).length;
}

/* ------------------------------------------------------------------ helpers */

function WireLine({
  kind,
  fields,
  tone = "default",
}: {
  kind: string;
  fields: string;
  tone?: "default" | "accent" | "muted";
}) {
  return (
    <div className="flex items-start gap-3">
      <span
        className={cn(
          "mt-px shrink-0 rounded px-1.5 py-0.5 font-mono text-[11px] font-semibold",
          tone === "accent"
            ? "bg-primary/15 text-primary"
            : tone === "muted"
              ? "bg-muted text-muted-foreground"
              : "bg-secondary text-foreground",
        )}
      >
        {kind}
      </span>
      <code className="text-muted-foreground min-w-0 flex-1 break-words font-mono text-xs leading-relaxed">
        {fields}
      </code>
    </div>
  );
}

/* -------------------------------------------------------------------- page */

export default function QueryConsolePage() {
  // LIVE grid: live worker trust seeds the candidate pool, and submitQuery
  // dispatches a REAL job that ripples to Jobs / Overview / Network.
  const { workers, connected, submitQuery } = useLive();

  // Form state (controlled, deterministic defaults). SQL seeds from the real job.
  const [fn, setFn] = React.useState<Fn>("p2p_query");
  const [sql, setSql] = React.useState<string>(REAL_JOB.sql);
  const [dataClass, setDataClass] = React.useState<DataClass>("Internal");
  const [verifyMode, setVerifyMode] = React.useState<VerifyMode>("Quorum");
  const [quorum, setQuorum] = React.useState(3);
  const [hedgeK, setHedgeK] = React.useState(4);
  const [freeTier, setFreeTier] = React.useState(false);
  const [maxEscrow, setMaxEscrow] = React.useState(2);

  // Simulation state. Initial render is idle => hydration-safe (no timers run on server).
  const [running, setRunning] = React.useState(false);
  const [done, setDone] = React.useState(false);
  const [tick, setTick] = React.useState(0);

  // The REAL outcome from the grid (populated by submitQuery when connected).
  // While null we render the simulated/placeholder run. `liveResult.error` is
  // surfaced if the dispatch failed.
  const [liveResult, setLiveResult] = React.useState<QueryResult | null>(null);
  // `true` when the most recent dispatch went to the live grid (vs. local sim).
  const [ranLive, setRanLive] = React.useState(false);

  // Guard ref checked synchronously inside dispatch to prevent multiple intervals
  // from starting on rapid double-clicks before the `running` state flush.
  const isRunningRef = React.useRef(false);

  // A frozen snapshot of the knobs at dispatch time, so mid-run knob edits
  // don't distort an in-flight simulation.
  const [run, setRun] = React.useState<{
    fn: Fn;
    dataClass: DataClass;
    verifyMode: VerifyMode;
    quorum: number;
    k: number;
    free: boolean;
    escrow: number;
    jobId: string;
    queryHash: string;
    nonce: number;
  } | null>(null);

  // Drive the stepper. setInterval only ever starts after a user Dispatch,
  // so the server/initial client render stays static.
  //
  // Local sim: animate offer→settle, then finish (done=true).
  // Live dispatch (awaiting submitQuery): animate offer→verify and HOLD there
  // until the real outcome lands — the resolve handler advances to settle/done,
  // so we never flash a fake result before the real one arrives.
  React.useEffect(() => {
    if (!running) return;
    const awaitingLive = ranLive && liveResult == null;
    const lastIdx = STAGES.length - 1; // settle
    const holdIdx = STAGES.length - 2; // verify
    const id = setInterval(() => {
      setTick((t) => {
        const next = t + 1;
        if (awaitingLive) {
          // Ramp up to (but not past) verify while the grid runs the job.
          return Math.min(next, holdIdx);
        }
        if (next >= lastIdx) {
          isRunningRef.current = false;
          setRunning(false);
          setDone(true);
          return lastIdx;
        }
        return next;
      });
    }, TICK_MS);
    return () => {
      clearInterval(id);
    };
  }, [running, ranLive, liveResult]);

  const kClamped = Math.min(hedgeK, 6);
  // Candidate set = LIVE honest workers by trust desc, capped to k. The race is
  // then ordered by real measured latency so the fastest real worker tends to win.
  const candidates = React.useMemo(() => {
    // Data-class selection policy (§7.5): the minimum attestation tier gates who
    // is even eligible — Internal needs L1, Sensitive needs L2 — so free/L0 hosts
    // are excluded from those classes. (The grid also enforces a trust floor.)
    const { tier } = CLASS_POLICY[dataClass];
    const pool = [...workers]
      .filter((w) => w.behavior === "honest" && ATT_RANK[w.attestation] >= tier)
      .sort((a, b) => b.trust - a.trust)
      .slice(0, kClamped);
    return [...pool].sort((a, b) => a.p50LatencyMs - b.p50LatencyMs);
  }, [workers, kClamped, dataClass]);

  // Hosts that clear BOTH gates (attestation + trust) of the selected class —
  // the live backend rejects a dispatch whose quorum exceeds this count, so we
  // cap the quorum slider and guard Dispatch against it.
  const eligibleHostCount = React.useMemo(
    () => countEligibleHosts(workers, dataClass),
    [workers, dataClass]
  );
  const quorumMax = Math.min(5, Math.max(1, eligibleHostCount));
  const effQuorum = Math.min(quorum, quorumMax);
  const noEligibleHosts = eligibleHostCount === 0;
  const pol = CLASS_POLICY[dataClass];

  const kBelowQuorum = hedgeK < effQuorum;

  function dispatch() {
    // Guard against rapid double-clicks: check the ref synchronously before any
    // state update so a second click that arrives before the React flush is ignored.
    if (isRunningRef.current || noEligibleHosts) return;
    isRunningRef.current = true;
    const qEff = Math.min(effQuorum, hedgeK);
    const seed = `${fn}:${dataClass}:${verifyMode}:${quorum}:${hedgeK}`;
    const snap = {
      fn,
      dataClass,
      verifyMode,
      quorum: qEff,
      k: kClamped,
      free: freeTier,
      escrow: freeTier ? 0 : maxEscrow,
      jobId: "job_" + fakeHash(seed).slice(3, 11),
      queryHash: fakeHash("q:" + sql),
      nonce: 0x5e1f - sql.length, // deterministic, no Date/random
    };
    // Freeze the SQL too, so the request matches what the panel shows.
    const sqlSnap = sql;
    setRun(snap);
    setLiveResult(null);
    setDone(false);
    setTick(0);
    setRanLive(connected);
    setRunning(true);

    if (!connected) {
      // Offline: pure local simulation (the stepper effect finishes it).
      return;
    }

    // Online: fire a REAL job at the grid. The stepper animates offer→verify
    // while we await; populate the run panel from the real outcome on resolve.
    void (async () => {
      let outcome: QueryResult;
      try {
        outcome = await submitQuery({
          sql: sqlSnap,
          dataClass,
          verifyMode,
          quorum: qEff,
          k: kClamped,
        });
      } catch (e) {
        outcome = {
          id: snap.jobId,
          status: "failed",
          winner: null,
          latencyMs: 0,
          rowCount: 0,
          resultHash: null,
          quorum: qEff,
          k: kClamped,
          candidates: [],
          result: { columns: [], rows: [] },
          error: e instanceof Error ? e.message : "dispatch failed",
        };
      }
      isRunningRef.current = false;
      setLiveResult(outcome);
      setTick(STAGES.length - 1); // settle
      setRunning(false);
      setDone(true);
    })();
  }

  function reset() {
    isRunningRef.current = false;
    setRunning(false);
    setDone(false);
    setTick(0);
    setRun(null);
    setLiveResult(null);
    setRanLive(false);
  }

  // Effective view config: while idle/never-run, fall back to current knobs
  // for the "translates to" preview; during a run use the frozen snapshot.
  const view = run ?? {
    fn,
    dataClass,
    verifyMode,
    quorum: Math.min(effQuorum, hedgeK),
    k: kClamped,
    free: freeTier,
    escrow: freeTier ? 0 : maxEscrow,
    jobId: "job_pending",
    queryHash: fakeHash("q:" + sql),
    nonce: 0,
  };

  const stageIdx = tick;
  const sealed = view.dataClass === "Sensitive";
  const runCandidates = candidates.slice(0, view.k);
  const committedCount = runCandidates.filter((_, rank) => {
    const s = stateFor(stageIdx, rank, view.quorum);
    return s === "committed" || s === "won";
  }).length;
  const quorumReached = committedCount >= view.quorum;
  const winner = runCandidates[0];

  // ---- REAL outcome view (when a live dispatch has resolved) --------------
  const real = liveResult; // alias for brevity in JSX
  const realError = real?.error ?? null;
  // Translate the backend's raw data-class policy rejection into an actionable
  // message that names how many hosts qualify and what to do about it.
  const friendlyError = React.useMemo(() => {
    if (!realError) return null;
    if (!/policy|no hosts|attestation|trust|eligible/i.test(realError)) return realError;
    const errClass = run?.dataClass ?? dataClass;
    const p = CLASS_POLICY[errClass];
    const n = countEligibleHosts(workers, errClass);
    const alt = errClass === "Sensitive" ? "Internal or Public" : "Public";
    return `Only ${n} host${n === 1 ? "" : "s"} meet the ${errClass} policy (≥ ${p.att}, trust ≥ ${p.trust.toFixed(2)}) — lower the quorum or choose ${alt}.`;
  }, [realError, run, dataClass, workers]);
  const realOk = real != null && !realError;
  // Real candidates straight from the grid (alias/attestation/state/verdict/…).
  const realCandidates: JobCandidate[] = real?.candidates ?? [];
  // Result table + headline metrics: prefer the real outcome, else the snapshot
  // fallback used by the offline simulation.
  const resultCols = realOk ? real!.result.columns : RESULT.columns;
  const resultRows = realOk ? real!.result.rows : RESULT.rows;
  const resultHash = realOk ? real!.resultHash : RESULT_HASH;
  const resultRowCount = realOk ? real!.rowCount : RESULT_ROWS;
  const resultLatencyMs = realOk
    ? real!.latencyMs
    : winner
      ? winner.p50LatencyMs
      : 0;
  const realWinnerAlias =
    realCandidates.find((c) => c.state === "won")?.alias ??
    (real?.winner ?? null);

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Terminal />}
        title="Query Console"
        description="Compose p2p_query / p2p_join / p2p_share over data in S3/ADLS/GCS. Several hosts run it redundantly; the first correct result that reaches quorum wins."
      >
        <Button onClick={dispatch} disabled={running || noEligibleHosts}>
          <Send /> Dispatch
        </Button>
      </PageHeader>

      <Explainer
        what="Compose a query and watch the grid run it. Instead of one server, several machines run your SQL at the same time and must agree on the answer before it is accepted."
        impact="You get a fast result that has already been cross-checked — so a single broken or dishonest machine cannot hand you a wrong answer."
      />

      <div className="grid gap-6 lg:grid-cols-12">
        {/* ------------------------------------------------------- LEFT: editor */}
        <div className="space-y-6 lg:col-span-5">
          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2">
                <Sparkles className="size-4 text-primary" /> Compose
              </CardTitle>
              <CardDescription>
                Pick a grid function and write SQL against object-store paths.
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-5">
              <div className="space-y-2">
                <Label className="text-muted-foreground text-xs">Function</Label>
                <Tabs value={fn} onValueChange={(v) => setFn(v as Fn)}>
                  <TabsList className="w-full">
                    <TabsTrigger value="p2p_query">p2p_query</TabsTrigger>
                    <TabsTrigger value="p2p_join">p2p_join</TabsTrigger>
                    <TabsTrigger value="p2p_share">p2p_share</TabsTrigger>
                  </TabsList>
                </Tabs>
              </div>

              <div className="space-y-2">
                <Label className="text-muted-foreground text-xs" htmlFor="sql">
                  SQL
                </Label>
                <Textarea
                  id="sql"
                  value={sql}
                  onChange={(e) => setSql(e.target.value)}
                  rows={10}
                  spellCheck={false}
                  className="font-mono text-xs leading-relaxed"
                />
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2">
                <Cpu className="size-4 text-primary" /> Execution policy
              </CardTitle>
              <CardDescription>
                Redundancy, verification, and escrow controls.
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-6">
              <div className="grid gap-5 sm:grid-cols-2">
                {/* Data class */}
                <div className="space-y-2">
                  <Label className="text-muted-foreground text-xs">
                    Data class
                    <InfoHint
                      text="How sensitive the data is — Sensitive routes only to hardware-attested (L2) machines."
                      className="ml-1"
                    />
                  </Label>
                  <Select
                    value={dataClass}
                    onValueChange={(v) => setDataClass(v as DataClass)}
                  >
                    <SelectTrigger className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="Public">Public</SelectItem>
                      <SelectItem value="Internal">Internal</SelectItem>
                      <SelectItem value="Sensitive">Sensitive</SelectItem>
                    </SelectContent>
                  </Select>
                </div>

                {/* Verify mode */}
                <div className="space-y-2">
                  <Label className="text-muted-foreground text-xs">Verify mode</Label>
                  <Tabs
                    value={verifyMode}
                    onValueChange={(v) => setVerifyMode(v as VerifyMode)}
                  >
                    <TabsList className="w-full">
                      <TabsTrigger value="Fast">Fast</TabsTrigger>
                      <TabsTrigger value="Quorum">Quorum</TabsTrigger>
                    </TabsList>
                  </Tabs>
                </div>
              </div>

              {/* Active selection policy + live eligibility (§7.5 + §8.2.1): the
                  attestation tier is a HARD gate and the grid enforces a trust
                  floor, so the quorum can't exceed how many hosts clear both. */}
              <div className="bg-muted/40 flex flex-wrap items-center gap-x-3 gap-y-1.5 rounded-lg border p-3 text-xs">
                <span className="text-foreground font-medium">{dataClass} selection policy</span>
                <Badge variant="muted" className="font-mono">≥ {pol.att} attestation</Badge>
                <Badge variant="muted" className="font-mono">trust ≥ {pol.trust.toFixed(2)}</Badge>
                <Badge variant={pol.paid ? "warn" : "ok"}>
                  {pol.paid ? "paid · stake counts" : "free · off-chain"}
                </Badge>
                <Badge variant={noEligibleHosts ? "destructive" : "info"} className="font-mono">
                  {eligibleHostCount} eligible
                </Badge>
                <span className="text-muted-foreground">
                  {noEligibleHosts
                    ? `no hosts meet this policy — choose ${dataClass === "Sensitive" ? "Internal or Public" : "Public"}.`
                    : `${eligibleHostCount} host${eligibleHostCount === 1 ? "" : "s"} eligible for ${dataClass} (≥ ${pol.att}, trust ≥ ${pol.trust.toFixed(2)}) — quorum capped at ${quorumMax}.`}
                </span>
              </div>

              {dataClass === "Sensitive" ? (
                <div className="flex items-start gap-2 rounded-lg border border-[var(--warn)]/30 bg-[var(--warn)]/10 p-3 text-xs text-[var(--warn)]">
                  <AlertTriangle className="mt-px size-4 shrink-0" />
                  <span>
                    Routes only to L2-attested hosts; data key sealed to enclave.
                  </span>
                </div>
              ) : null}

              {/* Quorum slider */}
              <div className="space-y-2">
                <div className="flex items-baseline justify-between">
                  <Label className="text-muted-foreground text-xs">
                    Quorum
                    <InfoHint
                      text="How many workers must return the same result before it is accepted."
                      className="ml-1"
                    />
                  </Label>
                  <span className="text-sm font-medium tabular-nums">{effQuorum}</span>
                </div>
                <Slider
                  min={1}
                  max={quorumMax}
                  step={1}
                  value={[effQuorum]}
                  onValueChange={(v) => setQuorum(v[0])}
                />
                {quorum > quorumMax ? (
                  <p className="flex items-center gap-1 text-xs text-[var(--warn)]">
                    <AlertTriangle className="size-3" /> capped at {quorumMax} — only{" "}
                    {eligibleHostCount} host{eligibleHostCount === 1 ? "" : "s"} eligible for{" "}
                    {dataClass}
                  </p>
                ) : (
                  <p className="text-muted-foreground text-xs">
                    Matching result hashes required before a result is accepted.
                  </p>
                )}
              </div>

              {/* Hedge k slider */}
              <div className="space-y-2">
                <div className="flex items-baseline justify-between">
                  <Label className="text-muted-foreground text-xs">
                    Hedge k
                    <InfoHint
                      text="How many workers run your query in parallel (a race); the rest are cancelled once it is decided."
                      className="ml-1"
                    />
                  </Label>
                  <span className="text-sm font-medium tabular-nums">{hedgeK}</span>
                </div>
                <Slider
                  min={1}
                  max={6}
                  step={1}
                  value={[hedgeK]}
                  onValueChange={(v) => setHedgeK(v[0])}
                />
                {kBelowQuorum ? (
                  <p className="flex items-center gap-1 text-xs text-[var(--warn)]">
                    <AlertTriangle className="size-3" /> k should be ≥ quorum
                  </p>
                ) : (
                  <p className="text-muted-foreground text-xs">
                    Hosts the query races on redundantly.
                  </p>
                )}
              </div>

              <Separator />

              {/* Escrow */}
              <div className="space-y-4">
                <div className="flex items-center justify-between gap-3">
                  <div>
                    <Label className="text-sm">Free public tier</Label>
                    <p className="text-muted-foreground text-xs">
                      Off-chain path; no escrow, price = 0.
                    </p>
                  </div>
                  <Switch checked={freeTier} onCheckedChange={setFreeTier} />
                </div>

                <div className="space-y-2">
                  <Label
                    className="text-muted-foreground text-xs"
                    htmlFor="escrow"
                  >
                    Max escrow (TON)
                  </Label>
                  <Input
                    id="escrow"
                    type="number"
                    min={0}
                    step={0.5}
                    value={freeTier ? 0 : maxEscrow}
                    onChange={(e) => setMaxEscrow(Math.max(0, Number(e.target.value) || 0))}
                    disabled={freeTier}
                    className="tabular-nums"
                  />
                </div>
              </div>

              <div className="flex items-center gap-2 pt-1">
                <Button onClick={dispatch} disabled={running || noEligibleHosts} className="flex-1">
                  <Send /> Dispatch query
                </Button>
                <Button variant="ghost" onClick={reset} disabled={running}>
                  <RefreshCw /> Reset
                </Button>
              </div>

              <p className="text-muted-foreground flex items-start gap-1.5 text-xs">
                <Database className="mt-px size-3 shrink-0" />
                <span>
                  Candidate set and result preview are seeded from the real grid snapshot
                  (honest workers by trust; the result the grid actually computed).
                </span>
              </p>
            </CardContent>
          </Card>
        </div>

        {/* -------------------------------------------------------- RIGHT: run */}
        <div className="space-y-6 lg:col-span-7">
          <Card>
            <CardHeader>
              <div className="flex items-start justify-between gap-3">
                <div>
                  <CardTitle className="flex items-center gap-2">
                    <Timer className="size-4 text-primary" /> Live run
                  </CardTitle>
                  <CardDescription>
                    Hedged execution across {view.k} host
                    {view.k === 1 ? "" : "s"} · verify {view.verifyMode} · quorum{" "}
                    {view.quorum}
                  </CardDescription>
                </div>
                {run ? (
                  <CopyId value={run.jobId} display={run.jobId} truncate={false} />
                ) : null}
              </div>
            </CardHeader>
            <CardContent className="space-y-5">
              {/* Stage stepper */}
              <div className="flex flex-wrap items-center gap-1.5">
                {STAGES.map((s, i) => {
                  const reached = run != null && i <= stageIdx;
                  const current = run != null && i === stageIdx && (running || done);
                  return (
                    <React.Fragment key={s}>
                      <span
                        className={cn(
                          "rounded-md border px-2 py-1 text-xs font-medium transition-colors",
                          current
                            ? "border-primary/40 bg-primary/15 text-primary"
                            : reached
                              ? "border-[var(--ok)]/30 bg-[var(--ok)]/10 text-[var(--ok)]"
                              : "text-muted-foreground bg-card",
                        )}
                      >
                        {STAGE_LABEL[s]}
                      </span>
                      {i < STAGES.length - 1 ? (
                        <ArrowRight className="text-muted-foreground/40 size-3" />
                      ) : null}
                    </React.Fragment>
                  );
                })}
              </div>

              {!run ? (
                <div className="text-muted-foreground flex flex-col items-center justify-center gap-2 rounded-lg border border-dashed py-14 text-center text-sm">
                  <Send className="size-5 opacity-60" />
                  <span>Configure the policy and dispatch to watch the race.</span>
                </div>
              ) : real != null ? (
                <>
                  {/* REAL outcome: the grid actually ran this job. */}
                  {realError ? (
                    <div className="flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/10 p-3 text-xs text-destructive">
                      <AlertTriangle className="mt-px size-4 shrink-0" />
                      <span>Dispatch failed: {friendlyError}</span>
                    </div>
                  ) : (
                    /* Dispatched-to-grid banner */
                    <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-[var(--ok)]/30 bg-[var(--ok)]/10 px-3 py-2 text-sm">
                      <span className="flex items-center gap-2">
                        <CheckCircle2 className="size-4 text-[var(--ok)]" />
                        <span className="font-medium">
                          Quorum {real.quorum}/{real.quorum} · status {real.status}
                        </span>
                      </span>
                      <Badge variant="ok">real · dispatched to grid</Badge>
                    </div>
                  )}

                  {/* Real candidate rows — alias / attestation / state / verdict. */}
                  <div className="space-y-2.5">
                    {realCandidates.map((c) => {
                      const meta = realStateMeta[c.state];
                      const isWinner =
                        c.state === "won" || c.workerId === real.winner;
                      const divergent =
                        c.state === "committed" && c.verdict !== "Correct";
                      return (
                        <div
                          key={c.workerId}
                          className={cn(
                            "rounded-lg border p-3 transition-colors",
                            isWinner
                              ? "border-primary/50 ring-1 ring-primary/40 bg-primary/[0.04]"
                              : divergent
                                ? "border-destructive/30 bg-destructive/5"
                                : c.state === "reset" || c.state === "dispatched"
                                  ? "opacity-60"
                                  : "bg-card",
                          )}
                        >
                          <div className="flex items-center gap-3">
                            <div className="min-w-0 flex-1">
                              <div className="flex flex-wrap items-center gap-2">
                                <span className="truncate text-sm font-medium">
                                  {c.alias}
                                </span>
                                <AttestationBadge level={c.attestation} />
                                <Badge variant={meta.variant}>{meta.label}</Badge>
                                <VerdictBadge verdict={c.verdict} />
                                {isWinner ? (
                                  <Badge variant="ok" className="gap-1">
                                    <CheckCircle2 className="size-3" /> winner
                                  </Badge>
                                ) : null}
                              </div>
                            </div>
                            <div className="text-right text-xs tabular-nums">
                              <div className="text-muted-foreground">
                                eta {ms(c.etaMs)}
                              </div>
                              <div className="font-medium">
                                {c.price === 0 ? "free" : `${num(c.price)} TON`}
                              </div>
                            </div>
                          </div>
                          <div className="mt-2.5 flex items-center gap-3">
                            <Progress
                              value={c.progressPct}
                              className="h-1.5"
                              indicatorClassName={
                                isWinner
                                  ? "bg-primary"
                                  : divergent
                                    ? "bg-[var(--destructive)]"
                                    : c.state === "committed"
                                      ? "bg-[var(--ok)]"
                                      : c.state === "reset"
                                        ? "bg-muted-foreground"
                                        : undefined
                              }
                            />
                            <span className="text-muted-foreground w-9 text-right text-xs tabular-nums">
                              {c.progressPct}%
                            </span>
                          </div>
                          {c.committedHash ? (
                            <div className="mt-2 flex items-center gap-2">
                              <Hash className="text-muted-foreground size-3" />
                              <CopyId value={c.committedHash} className="text-[11px]" />
                              {c.commitLatencyMs ? (
                                <span className="text-muted-foreground text-[11px] tabular-nums">
                                  · {ms(c.commitLatencyMs)}
                                </span>
                              ) : null}
                            </div>
                          ) : null}
                        </div>
                      );
                    })}
                    {realCandidates.length === 0 && !realError ? (
                      <div className="text-muted-foreground rounded-lg border border-dashed p-3 text-xs">
                        No per-candidate detail returned for this job.
                      </div>
                    ) : null}
                  </div>
                </>
              ) : (
                <>
                  {/* Quorum banner */}
                  <div
                    className={cn(
                      "flex items-center justify-between rounded-lg border px-3 py-2 text-sm",
                      quorumReached
                        ? "border-[var(--ok)]/30 bg-[var(--ok)]/10"
                        : "bg-muted/40",
                    )}
                  >
                    <span className="flex items-center gap-2">
                      {quorumReached ? (
                        <CheckCircle2 className="size-4 text-[var(--ok)]" />
                      ) : (
                        <Dot status="info" pulse={running} />
                      )}
                      <span className="font-medium">
                        Quorum {Math.min(committedCount, view.quorum)}/{view.quorum}
                        {quorumReached ? " reached" : ""}
                      </span>
                    </span>
                    <span className="text-muted-foreground text-xs">
                      {view.k} racing · {committedCount} committed
                    </span>
                  </div>

                  {/* Candidate rows */}
                  <div className="space-y-2.5">
                    {runCandidates.map((w, rank) => {
                      const st = stateFor(stageIdx, rank, view.quorum);
                      const meta = stateMeta[st];
                      // Progress: ramps with executing; committed/won/reset are pinned.
                      const base =
                        stageIdx <= 2
                          ? stageIdx * 12
                          : stageIdx === 3
                            ? 45 + rank * 4
                            : 100;
                      const prog =
                        st === "reset"
                          ? Math.min(72, 50 + rank * 6)
                          : st === "running"
                            ? Math.min(96, base + rank * 3)
                            : st === "committed" || st === "won"
                              ? 100
                              : Math.min(40, base);
                      const showHash =
                        st === "committed" || st === "won";
                      const isWinner = st === "won";
                      // ETA / price seeded from the worker's real measured latency.
                      const etaMs = w.p50LatencyMs;
                      const priceUnits = Math.max(1, Math.round(w.trust * 8));
                      return (
                        <div
                          key={w.id}
                          className={cn(
                            "rounded-lg border p-3 transition-colors",
                            isWinner
                              ? "border-primary/50 ring-1 ring-primary/40 bg-primary/[0.04]"
                              : st === "reset"
                                ? "opacity-55"
                                : "bg-card",
                          )}
                        >
                          <div className="flex items-center gap-3">
                            <div className="min-w-0 flex-1">
                              <div className="flex items-center gap-2">
                                <span className="truncate text-sm font-medium">
                                  {w.alias}
                                </span>
                                <AttestationBadge level={w.attestation} />
                                <Badge variant={meta.variant}>{meta.label}</Badge>
                                {isWinner ? (
                                  <Badge variant="ok" className="gap-1">
                                    <CheckCircle2 className="size-3" /> winner
                                  </Badge>
                                ) : null}
                              </div>
                              <div className="mt-1 w-40">
                                <ScoreBar value={w.trust} />
                              </div>
                            </div>
                            <div className="text-right text-xs tabular-nums">
                              <div className="text-muted-foreground">
                                eta {ms(etaMs)}
                              </div>
                              <div className="font-medium">
                                {view.free ? "free" : `${priceUnits} u`}
                              </div>
                            </div>
                          </div>
                          <div className="mt-2.5 flex items-center gap-3">
                            <Progress
                              value={prog}
                              className="h-1.5"
                              indicatorClassName={
                                isWinner
                                  ? "bg-primary"
                                  : st === "committed"
                                    ? "bg-[var(--ok)]"
                                    : st === "reset"
                                      ? "bg-muted-foreground"
                                      : undefined
                              }
                            />
                            <span className="text-muted-foreground w-9 text-right text-xs tabular-nums">
                              {Math.round(prog)}%
                            </span>
                          </div>
                          {showHash ? (
                            <div className="mt-2 flex items-center gap-2">
                              <Hash className="text-muted-foreground size-3" />
                              <CopyId
                                value={RESULT_HASH}
                                className="text-[11px]"
                              />
                              <span className="text-muted-foreground text-[11px] tabular-nums">
                                · {num(RESULT_ROWS)} rows · {ms(etaMs)}
                              </span>
                            </div>
                          ) : null}
                        </div>
                      );
                    })}
                  </div>
                </>
              )}
            </CardContent>
          </Card>

          {/* Translates to (wire messages) */}
          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2">
                <Hash className="size-4 text-primary" /> Translates to
              </CardTitle>
              <CardDescription>
                The QUIC wire messages this dispatch emits, in order.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <ol className="relative space-y-3 border-l pl-5">
                {[
                  {
                    kind: "Offer",
                    tone: "default" as const,
                    fields: `{ job_id: "${view.jobId}", query_hash: "${view.queryHash.slice(0, 14)}…", data_class: ${view.dataClass}, nonce: ${view.nonce} }`,
                  },
                  {
                    kind: "Bid",
                    tone: "default" as const,
                    fields: `{ decision: Accept, eta_ms: ${winner ? winner.p50LatencyMs : 0}, price: ${view.free ? 0 : winner ? Math.max(1, Math.round(winner.trust * 8)) : 0}, attestation: ${winner?.attestation ?? "L0"} }`,
                  },
                  {
                    kind: "Dispatch",
                    tone: "accent" as const,
                    fields: `{ sql: <…${sql.length} chars>, ${view.free ? "" : "credential: ScopedCredential, "}memory_limit_bytes: 4 GiB, threads: 8, verify_mode: ${view.verifyMode}${sealed ? ", sealed_key: SealedKey" : ""} }`,
                  },
                  {
                    kind: "Commit",
                    tone: "default" as const,
                    fields: `{ result_hash: "${(resultHash ?? "—").slice(0, 14)}…", row_count: ${resultRowCount}, latency_ms: ${resultLatencyMs} }`,
                  },
                  {
                    kind: "Receipt",
                    tone: "default" as const,
                    fields: `{ verdict: Correct, sig: "ed25519:${(resultHash ?? "—").slice(0, 8)}…" }`,
                  },
                ].map((m) => (
                  <li key={m.kind} className="relative">
                    <span className="bg-primary absolute top-1.5 -left-[1.42rem] size-2 rounded-full ring-4 ring-background" />
                    <WireLine kind={m.kind} fields={m.fields} tone={m.tone} />
                  </li>
                ))}
              </ol>
              {sealed ? (
                <p className="text-muted-foreground mt-4 flex items-center gap-1.5 text-xs">
                  <Lock className="size-3 text-[var(--warn)]" />
                  <span>
                    <span className="text-foreground font-medium">sealed_key</span>{" "}
                    is present because the data class is Sensitive — released only
                    after L2 attestation.
                  </span>
                </p>
              ) : null}
            </CardContent>
          </Card>

          {/* Result preview */}
          {done && !realError ? (
            <Card>
              <CardHeader>
                <div className="flex items-center justify-between gap-2">
                  <div>
                    <CardTitle className="flex items-center gap-2">
                      <CheckCircle2 className="size-4 text-[var(--ok)]" /> Result
                      preview
                    </CardTitle>
                    <CardDescription>
                      Real result returned by{" "}
                      {(realOk ? realWinnerAlias : winner?.alias) ?? "winner"} — agreed
                      by {view.quorum} host{view.quorum === 1 ? "" : "s"}.
                    </CardDescription>
                  </div>
                  {realOk ? (
                    <Badge variant="ok">real · dispatched to grid</Badge>
                  ) : (
                    <Badge variant="ok">verified</Badge>
                  )}
                </div>
              </CardHeader>
              <CardContent className="space-y-4">
                <div className="overflow-hidden rounded-lg border">
                  <Table>
                    <TableHeader>
                      <TableRow>
                        {resultCols.map((c, i) => (
                          <TableHead
                            key={c}
                            className={cn(
                              i === 0 ? "pl-4" : "text-right",
                              i === resultCols.length - 1 && i !== 0 ? "pr-4" : "",
                            )}
                          >
                            {c}
                          </TableHead>
                        ))}
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {resultRows.map((row, ri) => (
                        <TableRow key={ri}>
                          {row.map((cell, ci) => (
                            <TableCell
                              key={ci}
                              className={cn(
                                "tabular-nums",
                                ci === 0 ? "pl-4 font-mono text-xs" : "text-right",
                                ci === row.length - 1 && ci !== 0 ? "pr-4" : "",
                              )}
                            >
                              {cell}
                            </TableCell>
                          ))}
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </div>
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                  <div className="bg-muted/40 rounded-lg border p-3">
                    <div className="text-muted-foreground text-xs">result_hash</div>
                    <div className="mt-1">
                      {resultHash ? <CopyId value={resultHash} /> : "—"}
                    </div>
                  </div>
                  <div className="bg-muted/40 rounded-lg border p-3">
                    <div className="text-muted-foreground text-xs">row_count</div>
                    <div className="mt-1 text-sm font-medium tabular-nums">
                      {num(resultRowCount)}
                    </div>
                  </div>
                  <div className="bg-muted/40 rounded-lg border p-3">
                    <div className="text-muted-foreground text-xs">latency</div>
                    <div className="mt-1 text-sm font-medium tabular-nums">
                      {ms(resultLatencyMs)}
                    </div>
                  </div>
                </div>
                {realOk ? (
                  <div className="text-muted-foreground flex items-start gap-2 text-xs">
                    <Sparkles className="mt-px size-3.5 shrink-0 text-[var(--ok)]" />
                    <span>
                      This ran on the live grid — see it appear in Jobs, Overview and
                      the Network graph.
                    </span>
                  </div>
                ) : null}
                <div className="text-muted-foreground flex items-center gap-2 text-xs">
                  <Coins className="size-3.5 text-[var(--warn)]" />
                  {(() => {
                    // For a live dispatch, the grid decides free vs paid from the
                    // data class (Public = free/off-chain; Internal/Sensitive = paid,
                    // escrow opened + settled). Reflect that real outcome rather than
                    // the editor's escrow knobs. Offline preview falls back to them.
                    const paid = realOk ? !!real!.paid : !view.free;
                    const escrow = realOk ? (real!.escrowTon ?? 0) : view.escrow;
                    return paid ? (
                      <span>
                        Settled from escrow ≤ {ton(escrow)} · losers RESET, unspent
                        escrow refunded.
                      </span>
                    ) : (
                      <span>Free public tier — no settlement, receipts gossiped.</span>
                    );
                  })()}
                </div>
              </CardContent>
            </Card>
          ) : null}

          {/* Offline fallback note */}
          {run && ranLive === false && connected === false ? (
            <p className="text-muted-foreground flex items-start gap-1.5 text-xs">
              <AlertTriangle className="mt-px size-3.5 shrink-0 text-[var(--warn)]" />
              <span>
                The live backend is offline (
                <code className="font-mono">cargo run -p console-server</code>) — this
                is a local preview seeded from the snapshot, not a real grid dispatch.
              </span>
            </p>
          ) : null}

          {/* Static hint footer */}
          {!run ? (
            <SectionTitle hint="commit-first">
              Offer → Bid → Dispatch → Commit → Verify → Settle
            </SectionTitle>
          ) : null}
        </div>
      </div>
    </div>
  );
}
