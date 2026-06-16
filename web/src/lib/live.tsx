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
  /** true when the grid ran this as a PAID job (escrow opened + settled). */
  paid?: boolean;
  /** escrow locked for a paid job, in whole TON (0 for free jobs). */
  escrowTon?: number;
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
    verifyMode?: string;
    quorum?: number;
    k?: number;
  }) => Promise<QueryResult>;
}

const Ctx = React.createContext<LiveCtx | null>(null);

/**
 * Lightly validate an SSE frame before applying it. The stream is trusted
 * infrastructure, but a malformed or partial frame must be ignored rather than
 * rendered as garbage — so we check that every field we consume is present and
 * the expected shape (overview object + workers/jobs/receipts arrays +
 * commGraph.nodes/edges arrays). Returns the typed state, or null to skip.
 */
function parseFrame(raw: string): LiveState | null {
  const d: unknown = JSON.parse(raw);
  if (typeof d !== "object" || d === null) return null;
  const f = d as Record<string, unknown>;
  const cg = f.commGraph as Record<string, unknown> | null | undefined;
  if (
    typeof f.overview !== "object" ||
    f.overview === null ||
    !Array.isArray(f.workers) ||
    !Array.isArray(f.jobs) ||
    !Array.isArray(f.receipts) ||
    typeof cg !== "object" ||
    cg === null ||
    !Array.isArray(cg.nodes) ||
    !Array.isArray(cg.edges)
  ) {
    return null;
  }
  return {
    overview: f.overview as LiveState["overview"],
    workers: f.workers as LiveState["workers"],
    jobs: f.jobs as LiveState["jobs"],
    receipts: f.receipts as LiveState["receipts"],
    commGraph: f.commGraph as LiveState["commGraph"],
  };
}

export function LiveProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = React.useState<LiveState>(FALLBACK);
  const [connected, setConnected] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    let es: EventSource | null = null;
    let retry: ReturnType<typeof setTimeout> | null = null;
    let attempt = 0;

    // The browser auto-reconnects an EventSource while it is CONNECTING (e.g. a
    // backend that is simply down), so the console picks the stream back up on
    // its own when the grid returns. We only intervene when the stream is
    // permanently CLOSED (a fatal error such as a non-2xx / CORS response):
    // recreate it ourselves on a capped exponential backoff so the live overlay
    // still recovers without a page reload.
    const scheduleReconnect = () => {
      if (cancelled || retry) return;
      const delay = Math.min(1000 * 2 ** attempt, 15000);
      attempt += 1;
      retry = setTimeout(() => {
        retry = null;
        connect();
      }, delay);
    };

    const connect = () => {
      if (cancelled) return;
      try {
        es = new EventSource(`${LIVE_URL}/api/stream`);
      } catch {
        // EventSource unavailable → stay on the snapshot fallback.
        return;
      }
      es.onopen = () => {
        attempt = 0;
      };
      es.onmessage = (e) => {
        if (cancelled) return;
        try {
          const frame = parseFrame(e.data);
          if (!frame) return; // malformed / partial → ignore, keep last good state
          setState(frame);
          setConnected(true);
        } catch {
          /* ignore malformed frame */
        }
      };
      es.onerror = () => {
        if (cancelled) return;
        setConnected(false);
        if (es && es.readyState === EventSource.CLOSED) {
          es.close();
          es = null;
          scheduleReconnect();
        }
      };
    };

    connect();
    return () => {
      cancelled = true;
      if (retry) clearTimeout(retry);
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
