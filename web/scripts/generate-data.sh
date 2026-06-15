#!/usr/bin/env bash
# Regenerate web/src/data/snapshot.json from the REAL duckdb-p2p system.
#
#   1. Runs the snapshot exporter test (real loopback-QUIC grid, trust,
#      settlement, config, protocol) → writes snapshot.json.
#   2. Runs the real loopback transport benchmark across a parallelism sweep
#      and merges the measured throughput/latency into snapshot.json.
#
# Usage:  web/scripts/generate-data.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SNAP="$ROOT/web/src/data/snapshot.json"
cd "$ROOT"

echo "==> [1/3] Running snapshot exporter (real grid run)…"
cargo test -p p2p-node --test console_export -- --ignored --nocapture 2>&1 | grep -E "CONSOLE_SNAPSHOT|workers=" || true

echo "==> [2/3] Running real loopback transport benchmark sweep…"
SWEEP_JSON="["
first=1
for P in 1 2 4 8; do
  LOG=$(P2P_BENCH_ROWS=80000 P2P_BENCH_PARALLELISM=$P \
        cargo test -p p2p-node --test benches -- --nocapture 2>/dev/null)
  ROWS=$(echo "$LOG" | sed -n 's/.*rows\/sec *: *\([0-9.]*\).*/\1/p' | head -1)
  MBPS=$(echo "$LOG" | sed -n 's/.*MB\/sec *: *\([0-9.]*\).*/\1/p' | head -1)
  P50=$(echo "$LOG"  | sed -n 's/.*min \/ p50 \/ avg \/ max ms : [0-9.]* \/ \([0-9.]*\).*/\1/p' | head -1)
  echo "    parallelism=$P -> rows/sec=$ROWS MB/sec=$MBPS p50=${P50}ms"
  [ $first -eq 0 ] && SWEEP_JSON+=","
  first=0
  SWEEP_JSON+="{\"parallelism\":$P,\"rowsPerSec\":${ROWS:-0},\"mbPerSec\":${MBPS:-0},\"p50Ms\":${P50:-0}}"
done
SWEEP_JSON+="]"

echo "==> [3/3] Merging benchmark into snapshot.json…"
node -e '
const fs = require("fs");
const path = process.argv[1];
const sweep = JSON.parse(process.argv[2]);
const s = JSON.parse(fs.readFileSync(path, "utf8"));
s.transport.bench = {
  rows: 80000,
  sweep,
  best: sweep.reduce((a, b) => (b.mbPerSec > a.mbPerSec ? b : a), sweep[0]),
  command: "cargo test -p p2p-node --test benches -- --nocapture",
  envKnobs: ["P2P_BENCH_ROWS", "P2P_BENCH_PARALLELISM", "P2P_BENCH_COMPRESSION", "P2P_BENCH_CONGESTION"],
};
fs.writeFileSync(path, JSON.stringify(s, null, 2));
console.log("    merged bench sweep into", path);
' "$SNAP" "$SWEEP_JSON"

echo "==> [4/4] Merging real TON deployment artifacts…"
node "$ROOT/web/scripts/extract-ton.mjs"

echo "==> Done. $(wc -c < "$SNAP" | tr -d ' ') bytes written to web/src/data/snapshot.json"
