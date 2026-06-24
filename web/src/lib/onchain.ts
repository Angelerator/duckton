"use client";

// Shared client hook for the LIVE mainnet GlobalParams contract, read from TON
// via the edge-cached /api/onchain Worker route (not from any snapshot).
import * as React from "react";

export interface OnchainStats {
  address: string;
  explorer: string;
  status: string | null;
  balanceTon: number | null;
  paramsVersion: number | null;
  platformFeeBps: number | null;
  participationBps: number | null;
  fetchedAt: number;
}

export function useOnchain(): { data: OnchainStats | null; loading: boolean } {
  const [data, setData] = React.useState<OnchainStats | null>(null);
  const [loading, setLoading] = React.useState(true);
  React.useEffect(() => {
    let alive = true;
    const load = () =>
      fetch("/api/onchain")
        .then((r) => (r.ok ? r.json() : null))
        .then((d: OnchainStats | null) => {
          if (alive && d) setData(d);
        })
        .catch(() => {})
        .finally(() => alive && setLoading(false));
    load();
    const t = setInterval(load, 30_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);
  return { data, loading };
}
