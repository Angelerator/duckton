#!/usr/bin/env bash
# Real-network collector: a requester node that repeatedly runs REAL distributed
# p2p_query jobs across the live host nodes (seed + workers) over QUIC and writes
# a small JSON summary that the website reads. Everything here is measured from
# actual cross-container execution — no snapshot, no mock.
set -o pipefail

EXT=/node/duckton.duckdb_extension
OUT=/shared/network.json
mkdir -p /shared

# Permissive floors so a fresh requester can use the (reputation-less) public
# hosts, plus a small per-job lease so the hosts admit the job.
SETUP="LOAD '${EXT}';
CALL p2p_set('network.bind_addr','0.0.0.0:0');
CALL p2p_set('trust.bootstrap_trust','1.0');
CALL p2p_set('budget.per_job_memory_bytes','33554432');
CALL p2p_set('budget.per_job_threads','1');
CALL p2p_trust(min_trust => 0, min_attest => 'L0');
CALL p2p_selection(replicas => 3, quorum => 2, checksum_min => 1);
CALL p2p_planner(prefer => 'remote', local_execution => false);
CALL p2p_join(bootstrap => ['quic://seed:9494','quic://worker-a:9494','quic://worker-b:9494']);"

QUERIES=(
  "SELECT 42 AS x"
  "SELECT count(*) AS n FROM range(100000)"
  "SELECT sum(i) AS s FROM range(50000) t(i)"
  "SELECT avg(i) AS a FROM range(20000) t(i)"
)

TOTAL=$(cat /shared/count 2>/dev/null || echo 0)
declare -A HOSTS
RECENT="[]"

emit() {
  local hjson="" h
  if [ "${#HOSTS[@]}" -gt 0 ]; then
    for h in "${!HOSTS[@]}"; do hjson="${hjson}\"${h}\","; done
  fi
  hjson="${hjson%,}"
  printf '{"network":"real","realJobsRun":%d,"onlineHosts":%d,"hosts":[%s],"recent":%s,"updatedAt":%s}\n' \
    "$TOTAL" "${#HOSTS[@]}" "$hjson" "$RECENT" "$(date -u +%s)" > "$OUT"
}
emit

while true; do
  Q="${QUERIES[$((RANDOM % ${#QUERIES[@]}))]}"
  META=$(timeout 45 duckdb -unsigned -noheader -list -c \
    "${SETUP} SELECT key||'='||value FROM p2p_query_meta('${Q}') WHERE key IN ('winner','latency_ms','verified','participants');" 2>/dev/null)
  WINNER=$(printf '%s\n' "$META" | sed -n 's/^winner=//p')
  LAT=$(printf '%s\n' "$META" | sed -n 's/^latency_ms=//p')
  VER=$(printf '%s\n' "$META" | sed -n 's/^verified=//p')
  PART=$(printf '%s\n' "$META" | sed -n 's/^participants=//p')
  if [ -n "${WINNER:-}" ] && [ "${VER:-}" = "true" ]; then
    TOTAL=$((TOTAL + 1)); echo "$TOTAL" > /shared/count
    HOSTS["$WINNER"]=1
    RECENT=$(printf '[{"winner":"%s","latencyMs":%s,"participants":%s,"query":"%s","ts":%s}]' \
      "$WINNER" "${LAT:-0}" "${PART:-0}" "${Q//\"/}" "$(date -u +%s)")
    emit
  fi
  sleep 5
done
