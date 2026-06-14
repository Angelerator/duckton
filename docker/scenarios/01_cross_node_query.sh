#!/usr/bin/env bash
# Scenario 1 — cross-node distributed queries over QUIC.
#
# A requester runs `FROM p2p_query('...', prefer=>'remote')` routed to OTHER
# nodes; results stream back and must equal the locally-computed expected value.
# Runs one sanity query, then many concurrently.
#   01_cross_node_query.sh [concurrency] [replicas] [quorum]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

CONC="${1:-30}"
REPLICAS="${2:-3}"
QUORUM="${3:-2}"

# Deterministic query with a known answer (computed locally here, not on a node).
QUERY="SELECT sum(i) FROM range(1,1001) t(i)"
EXPECTED=500500
RESULTS="$LOGDIR/query_results.txt"; : > "$RESULTS"

mapfile -t NODES < <(services)
worker_pool() { printf '%s\n' "${NODES[@]}" | grep -E '^node' ; }

run_one() {
  local idx="$1"
  local cexec; cexec="$(containers | shuf | head -n1)"
  local boot; boot="$(boot_list $(worker_pool | shuf | head -n5))"
  local sql="SELECT sum(i) FROM p2p_query('${QUERY}', prefer=>'remote', replicas=>${REPLICAS}, quorum=>${QUORUM}, min_trust=>0.0) t(i)"
  local out; out="$(req_query "$cexec" "$boot" "$sql" 2>/dev/null | tr -d '[:space:]')"
  echo "${idx} ${out}" >> "$RESULTS"
}

echo "==> sanity: one remote query (expect $EXPECTED)"
run_one 0
sanity="$(awk 'NR==1{print $2}' "$RESULTS")"
echo "    got: ${sanity:-<empty>}"
if [ "$sanity" != "$EXPECTED" ]; then
  echo "==> SANITY FAILED (got '${sanity}', expected ${EXPECTED})"; exit 1
fi

echo "==> running $CONC concurrent remote queries (replicas=$REPLICAS quorum=$QUORUM)…"
pids=()
for i in $(seq 1 "$CONC"); do
  run_one "$i" &
  pids+=($!)
  # cap inflight to avoid host overload
  if [ $((i % 20)) -eq 0 ]; then wait "${pids[@]}"; pids=(); fi
done
wait || true

total=$(wc -l < "$RESULTS" | tr -d ' ')
pass=$(awk -v e="$EXPECTED" '$2==e{c++} END{print c+0}' "$RESULTS")
echo "==> cross-node query results: ${pass}/${total} returned the correct value (${EXPECTED})"
[ "$pass" -eq "$total" ] && echo "==> SCENARIO 1 PASS" || { echo "==> SCENARIO 1 PARTIAL/FAIL"; awk -v e="$EXPECTED" '$2!=e' "$RESULTS" | head; }
