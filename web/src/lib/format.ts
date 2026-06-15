// Deterministic formatting helpers. No Date.now()/Math.random() so server and
// client render identically — time is pinned to the snapshot generation time.

import snapshot from "@/data/snapshot.json";

/** The moment the real data snapshot was generated (unix ms). Pins all "ago". */
export const NOW = (snapshot as { meta: { generatedAtMs: number } }).meta.generatedAtMs;

export function bytes(n: number, digits = 1): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let u = -1;
  let v = n;
  do {
    v /= 1024;
    u++;
  } while (v >= 1024 && u < units.length - 1);
  return `${v.toFixed(digits)} ${units[u]}`;
}

export function num(n: number): string {
  // Intentionally fixed "en-US" locale — all formatted numbers must be identical
  // between server and client renders (hydration determinism requires a pinned locale,
  // not the browser/OS default which varies per user).
  return new Intl.NumberFormat("en-US").format(n);
}

export function compact(n: number): string {
  return new Intl.NumberFormat("en-US", {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(n);
}

export function pct(n: number, digits = 0): string {
  return `${(n * 100).toFixed(digits)}%`;
}

export function ms(n: number): string {
  if (n < 1) return `${(n * 1000).toFixed(0)}µs`;
  if (n < 1000) return `${Math.round(n)}ms`;
  if (n < 60_000) return `${(n / 1000).toFixed(2)}s`;
  const m = Math.floor(n / 60_000);
  const s = Math.round((n % 60_000) / 1000);
  return `${m}m ${s}s`;
}

export function durationSecs(s: number): string {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${(s / 3600).toFixed(1)}h`;
  return `${(s / 86400).toFixed(1)}d`;
}

/** Relative time from the pinned NOW. `tsMs` is a unix-millis timestamp. */
export function ago(tsMs: number): string {
  const diff = Math.max(0, NOW - tsMs);
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}

export function inFuture(tsMs: number): string {
  const diff = Math.max(0, tsMs - NOW);
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  return `${d}d`;
}

/** Shorten a hex id/hash to head…tail. */
export function short(id: string, head = 6, tail = 4): string {
  if (id.length <= head + tail + 1) return id;
  return `${id.slice(0, head)}…${id.slice(-tail)}`;
}

/** TON amount with the symbol. */
export function ton(n: number, digits = 2): string {
  return `${n.toFixed(digits)} TON`;
}
