"use client";

// Shared client hook for the REAL multi-node network feed (live.duckton.com),
// used by both the marketing landing and the console overview. This is genuine
// data: independent node processes executing verified distributed p2p_query
// jobs over QUIC — distinct from the console-server loopback demo.
import * as React from "react";

export interface RealNet {
  realJobsRun: number;
  attempts?: number;
  verifiedRatePct?: number;
  avgLatencyMs?: number;
  onlineHosts: number;
  hosts: string[];
  recent: { winner: string; latencyMs: number; participants: number; query: string; ts: number }[];
  updatedAt: number;
}

// Same-origin path (served by the Cloudflare Worker), which proxies to the VM.
// Avoids corporate networks that 403 the raw cloud-IP live.duckton.com domain.
export const LIVE_NET_URL = "/api/realnet";

/** Poll the real-network summary every 5s. Returns null until the first load. */
export function useRealNet(): RealNet | null {
  const [data, setData] = React.useState<RealNet | null>(null);
  React.useEffect(() => {
    let alive = true;
    const load = () =>
      fetch(LIVE_NET_URL)
        .then((r) => (r.ok ? r.json() : null))
        .then((j: RealNet | null) => {
          if (alive && j) setData(j);
        })
        .catch(() => {});
    load();
    const t = setInterval(load, 5000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);
  return data;
}

/** Shorten a node id (b3:abcd…wxyz) for display. */
export const shortId = (s: string) => (s.length > 16 ? `${s.slice(0, 9)}…${s.slice(-4)}` : s);
