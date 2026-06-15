"use client";

import * as React from "react";
import {
  commGraph as snapComm,
  jobs as snapJobs,
  overview as snapOverview,
  receipts as snapReceipts,
  workers as snapWorkers,
} from "@/lib/data";
import type { CommEdge, CommNode } from "@/lib/data";
import type { Job, Receipt, Snapshot, Worker } from "@/lib/types";
import { cn } from "@/lib/utils";
import { Dot } from "@/components/common/atoms";

/** Where the live grid backend (crates/console-server) is reachable. */
export const LIVE_URL =
  process.env.NEXT_PUBLIC_LIVE_URL || "http://localhost:8787";

interface LiveState {
  overview: Snapshot["overview"];
  workers: Worker[];
  jobs: Job[];
  receipts: Receipt[];
  commGraph: { nodes: CommNode[]; edges: CommEdge[] };
}

const FALLBACK: LiveState = {
  overview: snapOverview,
  workers: snapWorkers,
  jobs: snapJobs,
  receipts: snapReceipts,
  commGraph: snapComm,
};

export interface QueryResult {
  id: string;
  status: string;
  winner: string | null;
  latencyMs: number;
  rowCount: number;
  resultHash: string | null;
  quorum: number;
  k: number;
  candidates: Job["candidates"];
  result: { columns: string[]; rows: string[][] };
  error?: string;
}

interface LiveCtx extends LiveState {
  /** true once the SSE stream from the live backend is connected */
  connected: boolean;
  jobsRun: number;
  /** dispatch a REAL job on the grid; resolves with its outcome */
  submitQuery: (body: {
    sql: string;
    dataClass?: string;
    quorum?: number;
    k?: number;
  }) => Promise<QueryResult>;
}

const Ctx = React.createContext<LiveCtx | null>(null);

export function LiveProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = React.useState<LiveState>(FALLBACK);
  const [connected, setConnected] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    let es: EventSource | null = null;
    try {
      es = new EventSource(`${LIVE_URL}/api/stream`);
      es.onmessage = (e) => {
        if (cancelled) return;
        try {
          const d = JSON.parse(e.data);
          if (!d?.overview) return;
          setState({
            overview: d.overview,
            workers: d.workers,
            jobs: d.jobs,
            receipts: d.receipts,
            commGraph: d.commGraph,
          });
          setConnected(true);
        } catch {
          /* ignore malformed frame */
        }
      };
      es.onerror = () => setConnected(false);
    } catch {
      setConnected(false);
    }
    return () => {
      cancelled = true;
      es?.close();
    };
  }, []);

  const submitQuery = React.useCallback<LiveCtx["submitQuery"]>(async (body) => {
    const r = await fetch(`${LIVE_URL}/api/query`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!r.ok) throw new Error(`query failed (${r.status})`);
    return (await r.json()) as QueryResult;
  }, []);

  const value: LiveCtx = React.useMemo(
    () => ({ ...state, connected, jobsRun: state.overview?.jobsRun ?? 0, submitQuery }),
    [state, connected, submitQuery]
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useLive(): LiveCtx {
  const c = React.useContext(Ctx);
  if (!c) throw new Error("useLive must be used within LiveProvider");
  return c;
}

/** Header status pill: LIVE (connected) or snapshot (offline). */
export function LiveStatus() {
  const { connected, overview, jobsRun } = useLive();
  const online = overview.workersOnline;
  return (
    <div className="flex items-center gap-2 text-sm">
      <Dot status={connected ? "ok" : "muted"} pulse={connected} />
      {connected ? (
        <>
          <span className="font-medium text-[var(--ok)]">LIVE</span>
          <span className="text-muted-foreground hidden sm:inline tabular-nums">
            {online} nodes · {jobsRun} jobs
          </span>
        </>
      ) : (
        <>
          <span className={cn("font-medium")}>snapshot</span>
          <span className="text-muted-foreground hidden sm:inline">backend offline</span>
        </>
      )}
    </div>
  );
}
