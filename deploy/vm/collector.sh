#!/usr/bin/env bash
# Real-network collector (live-discovery edition).
#
# A long-lived REQUESTER node that joins the swarm over the libp2p discovery
# overlay (Kademlia DHT + gossip) using a SINGLE bootstrap multiaddr — a stable
# DNS address, NOT a hard-coded peer identity or host list (the seed's overlay
# PeerId is learned live on connect). It keeps ONE warm DuckDB/node session so
# the gossiped, signature-and-PoW-verified membership stays warm, then:
#   * sources the host list + online count from the LIVE membership the node has
#     actually discovered (`p2p_network()`), and
#   * repeatedly runs REAL distributed `p2p_query` jobs across those live hosts
#     over QUIC, with quorum verification.
# Everything is measured/observed from the running swarm — no snapshot, no mock,
# no hard-coded seed+worker list.
set -o pipefail

EXT=/node/duckton.duckdb_extension
OUT=/shared/network.json
WORK=/shared/.collector
JOB="$WORK/job.csv"     # last job's p2p_query_meta (key=value per line)
NET="$WORK/net.csv"     # live membership: one discovered node_id per line
TICKF="$WORK/tick"      # barrier: the warm session writes this LAST each step
LOG="$WORK/duckdb.log"
CMD="$WORK/cmd.fifo"
mkdir -p /shared "$WORK"

# The one stable entry point into the swarm. A DNS multiaddr (service name +
# overlay port), not a peer id and not "the network": all real membership is
# discovered live from here. Overridable for other topologies.
BOOTSTRAP_ADDR="${COLLECTOR_BOOTSTRAP:-/dns4/seed/tcp/9595}"

QUERIES=(
  "SELECT 42 AS x"
  "SELECT count(*) AS n FROM range(100000)"
  "SELECT sum(i) AS s FROM range(50000) t(i)"
  "SELECT avg(i) AS a FROM range(20000) t(i)"
)

# Cumulative counters persist across restarts: "TOTAL ATTEMPTS LATSUM" so the
# derived rates stay consistent. TOTAL=verified jobs, ATTEMPTS=dispatch attempts.
STATE=/shared/state
read -r TOTAL ATTEMPTS LATSUM < "$STATE" 2>/dev/null || true
TOTAL=${TOTAL:-0}; ATTEMPTS=${ATTEMPTS:-0}; LATSUM=${LATSUM:-0}
RECENT_ITEMS=()   # newest-first ring buffer of recent verified-job JSON objects

# --- warm session: one DuckDB process reading commands from a FIFO ----------
# Keeping a single session alive keeps the discovery overlay (and its learned
# membership) warm between jobs — a fresh process per query would start with an
# empty, cold gossip view every time.
rm -f "$CMD"; mkfifo "$CMD"
duckdb -unsigned -noheader -list < "$CMD" > /dev/null 2>"$LOG" &
DUCK=$!
exec 3>"$CMD"   # hold the write end open so the session stays alive
trap 'exec 3>&-; kill "$DUCK" 2>/dev/null; rm -f "$CMD"' EXIT

send() { printf '%s\n' "$1" >&3; }

# Permissive requester floors (fresh public hosts have no reputation yet) + a
# small per-job lease so hosts admit the job. discovery.mode/bootstrap come from
# the environment (P2P_DISCOVERY_MODE=kademlia, P2P_DISCOVERY_BOOTSTRAP) so the
# node spawns the libp2p overlay and finds candidates from the live membership.
send "LOAD '${EXT}';"
send "CALL p2p_set('network.bind_addr','0.0.0.0:0');"
send "CALL p2p_set('trust.bootstrap_trust','1.0');"
send "CALL p2p_set('budget.per_job_memory_bytes','33554432');"
send "CALL p2p_set('budget.per_job_threads','1');"
send "CALL p2p_trust(min_trust => 0, min_attest => 'L0');"
send "CALL p2p_selection(replicas => 5, quorum => 3, checksum_min => 1);"
send "CALL p2p_planner(prefer => 'remote', local_execution => false);"
# Force node construction now (builds + bootstraps the discovery overlay) so the
# gossiped membership warms up before the first job is dispatched.
send "SELECT count(*) FROM p2p_network();"

emit() {
  local hjson="" h recent avg=0 vrate=100
  local hosts=()
  [ -s "$NET" ] && mapfile -t hosts < "$NET"
  for h in "${hosts[@]}"; do [ -n "$h" ] && hjson="${hjson}\"${h}\","; done
  hjson="${hjson%,}"
  recent=$(IFS=,; echo "${RECENT_ITEMS[*]:-}")
  [ "$TOTAL" -gt 0 ] && avg=$((LATSUM / TOTAL))
  [ "$ATTEMPTS" -gt 0 ] && vrate=$((TOTAL * 100 / ATTEMPTS))
  printf '{"network":"real","realJobsRun":%d,"attempts":%d,"verifiedRatePct":%d,"avgLatencyMs":%d,"onlineHosts":%d,"hosts":[%s],"recent":[%s],"updatedAt":%s}\n' \
    "$TOTAL" "$ATTEMPTS" "$vrate" "$avg" "${#hosts[@]}" "$hjson" "$recent" "$(date -u +%s)" > "$OUT"
}

# Run one job + membership refresh inside the warm session and block until the
# barrier tick lands. Statements execute in order, so when the tick file holds
# the expected number, JOB and NET have been (re)written.
TICK=0
step() { # $1 = query SQL
  local q="$1" expect qsql
  TICK=$((TICK + 1)); expect=$TICK
  rm -f "$JOB"                # so a failed/NoCandidates job leaves no stale row
  qsql=${q//\'/\'\'}          # escape single quotes for the SQL string literal
  send "COPY (SELECT key||'='||value FROM p2p_query_meta('${qsql}') WHERE key IN ('winner','latency_ms','verified','participants')) TO '${JOB}' (HEADER false, QUOTE '');"
  send "COPY (SELECT node_id FROM p2p_network() ORDER BY node_id) TO '${NET}' (HEADER false, QUOTE '');"
  send "COPY (SELECT ${expect}) TO '${TICKF}' (HEADER false);"
  local i
  for i in $(seq 1 600); do
    [ -f "$TICKF" ] && [ "$(tr -dc '0-9' < "$TICKF" 2>/dev/null)" = "$expect" ] && return 0
    sleep 0.05
  done
  return 1
}

# Publish an initial (empty) summary, then let the overlay learn the first ads.
emit
sleep 8

while true; do
  Q="${QUERIES[$((RANDOM % ${#QUERIES[@]}))]}"
  ATTEMPTS=$((ATTEMPTS + 1))
  step "$Q"
  WINNER=""; LAT=""; VER=""; PART=""
  if [ -s "$JOB" ]; then
    WINNER=$(sed -n 's/^winner=//p' "$JOB")
    LAT=$(sed -n 's/^latency_ms=//p' "$JOB")
    VER=$(sed -n 's/^verified=//p' "$JOB")
    PART=$(sed -n 's/^participants=//p' "$JOB")
  fi
  if [ -n "${WINNER:-}" ] && [ "${VER:-}" = "true" ]; then
    TOTAL=$((TOTAL + 1))
    LATSUM=$((LATSUM + ${LAT:-0}))
    # Publish only an opaque hash of the query — never the SQL text (the network
    # itself only ever sees a query_hash in the broadcast Offer).
    QH=$(printf '%s' "$Q" | sha256sum | cut -c1-16)
    item=$(printf '{"winner":"%s","latencyMs":%s,"participants":%s,"queryHash":"%s","ts":%s}' \
      "$WINNER" "${LAT:-0}" "${PART:-0}" "$QH" "$(date -u +%s)")
    RECENT_ITEMS=("$item" "${RECENT_ITEMS[@]:0:5}")   # keep newest 6
  fi
  echo "$TOTAL $ATTEMPTS $LATSUM" > "$STATE"
  emit
  # If the warm session died, exit so Docker restarts us (and rebuilds a fresh
  # overlay) rather than spinning against a dead pipe.
  kill -0 "$DUCK" 2>/dev/null || { echo "duckdb session exited; see $LOG" >&2; exit 1; }
  sleep 5
done
