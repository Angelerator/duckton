#!/usr/bin/env bash
# Scenario 1 — cross-node distributed queries over QUIC.
#
# A requester runs `FROM p2p_query('...', prefer=>'remote')` routed to OTHER
# nodes; results stream back and must equal the locally-computed expected value.
# Runs one sanity query, then many concurrently.
#   01_cross_node_query.sh [concurrency] [replicas] [quorum]
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

CONC="${1:-30}"
REPLICAS="${2:-3}"
QUORUM="${3:-2}"

# Deterministic query with a known answer (computed locally here, not on a node).
# p2p_query streams columns back as VARCHAR, so the remote does the aggregation
# and the requester simply reads the value.
QUERY="SELECT sum(i) AS s FROM range(1,1001) t(i)"
EXPECTED=500500
RESULTS="$LOGDIR/query_results.txt"; : > "$RESULTS"

worker_pool() { public_workers ; }

CLIENTC="$(ensure_client)"
# Cache the worker list ONCE: calling `docker compose ps` per query under heavy
# concurrency intermittently returns empty (CLI/daemon contention), which would
# make p2p_join fail on an empty bootstrap.
WORKERS_FILE="$LOGDIR/workers.txt"; worker_pool > "$WORKERS_FILE"

run_one() {
  local idx="$1"
  local cexec="$CLIENTC"
  # Offer to a wide sample so the coordinator can pick free workers even when
  # some are busy (max_jobs cap); it sends offers to all and dispatches to the
  # best `replicas` that accept.
  local boot; boot="$(boot_list $(shuf_lines < "$WORKERS_FILE" | head -n16))"
  local sql="SELECT s FROM p2p_query('${QUERY}', prefer=>'remote', replicas=>${REPLICAS}, quorum=>${QUORUM}, min_trust=>0.0)"
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
INFLIGHT="${INFLIGHT:-8}"
for i in $(seq 1 "$CONC"); do
  run_one "$i" &
  pids+=($!)
  # cap simultaneous requesters so the client isn't CPU-starved (which would make
  # offers time out and queries fail spuriously).
  if [ $((i % INFLIGHT)) -eq 0 ]; then wait "${pids[@]}"; pids=(); fi
done
wait || true

total=$(wc -l < "$RESULTS" | tr -d ' ')
pass=$(awk -v e="$EXPECTED" '$2==e{c++} END{print c+0}' "$RESULTS")
echo "==> cross-node query results: ${pass}/${total} returned the correct value (${EXPECTED})"
[ "$pass" -eq "$total" ] && echo "==> SCENARIO 1 PASS" || { echo "==> SCENARIO 1 PARTIAL/FAIL"; awk -v e="$EXPECTED" '$2!=e' "$RESULTS" | head; }
