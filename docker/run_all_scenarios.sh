#!/usr/bin/env bash
# Top-level harness runner for the duckdb-p2p grid scenario catalog.
#
#   docker/run_all_scenarios.sh
#
# 1. Builds p2p-node:latest if missing (force with BUILD=1).
# 2. Brings up the heterogeneous swarm + waits for all nodes ready.
# 3. Runs every scenario GROUP, printing a `PASS <id>` / `FAIL <id>` line each:
#      - single-container : 10 admin/config, 11 query-local, 12 hosting,
#                           13 sandbox, 14 settlement-prepared
#      - live swarm        : 20 query-remote, 21 anti-abuse
#      - library tier      : 30 scenarios, 31 antiabuse, 32 sandbox,
#                           33 transport, 34 trust, 35 resilience, 36 settlement
#    plus the legacy live smoke scenarios (01/02/03/05) for back-compat.
# 4. Captures failing containers' logs to /tmp/p2pgrid/logs/ and tears down
#    (keep the swarm with KEEP_UP=1; skip the cargo library tier with NO_UNITS=1).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PROJECT="${PROJECT:-p2pgrid}"
LOGDIR="${LOGDIR:-/tmp/p2pgrid}"; export LOGDIR PROJECT
mkdir -p "$LOGDIR/logs"
AGG="$LOGDIR/all_results.txt"; : > "$AGG"

S="$HERE/scenarios"
ID_SOLO="10_admin_config 11_query_local 12_hosting 13_sandbox 14_settlement_prepared"
ID_SWARM="20_query_remote 21_antiabuse_live"
ID_UNITS="30_units_scenarios 31_units_antiabuse 32_units_sandbox 33_units_transport 34_units_trust 35_units_resilience 36_units_settlement"
LEGACY_SMOKE="01_cross_node_query 02_chaos 03_health 05_economics_modes"

run_group() {
  local name="$1"
  echo; echo "########################################################################"
  bash "$S/${name}.sh" 2>&1 | tee -a "$AGG"
}

# ---------------------------------------------------------------- 1. build
if [ "${BUILD:-0}" = "1" ] || ! docker image inspect p2p-node:latest >/dev/null 2>&1; then
  echo "==> building p2p-node:latest"
  docker build -f "$HERE/Dockerfile" -t p2p-node:latest "$ROOT" >"$LOGDIR/build.log" 2>&1 || {
    echo "BUILD FAILED — tail:"; tail -n 30 "$LOGDIR/build.log"; exit 1; }
fi

# ---------------------------------------------------------------- 2. swarm up
echo "==> bringing up the heterogeneous swarm"
bash "$HERE/run_swarm.sh"
TOTAL=$(docker ps --filter "label=com.docker.compose.project=${PROJECT}" -q | wc -l | tr -d ' ')
bash "$S/00_wait_ready.sh" "$TOTAL" 120 || { echo "==> nodes not ready; capturing logs"; }

# ---------------------------------------------------------------- 3. scenarios
for g in $ID_SOLO $ID_SWARM; do run_group "$g"; done
if [ "${NO_UNITS:-0}" != "1" ]; then
  for g in $ID_UNITS; do run_group "$g"; done
else
  echo; echo "==> NO_UNITS=1: skipping the cargo library tier"
fi

echo; echo "########## legacy live smoke (back-compat: existing scenarios) ##########"
for g in $LEGACY_SMOKE; do
  echo "----- $g -----"; bash "$S/${g}.sh" 2>&1 | grep -E '==> SCENARIO|PASS|PARTIAL|FAIL|correct' | tail -4
done

# ---------------------------------------------------------------- 4. tally
PASS=$(grep -c '^PASS ' "$AGG" || true)
FAIL=$(grep -c '^FAIL ' "$AGG" || true)
echo; echo "========================================================================"
echo "==> SCENARIO CATALOG RESULTS: ${PASS} passed, ${FAIL} failed (per-id)"
if [ "$FAIL" -gt 0 ]; then
  echo "==> FAILURES:"; grep '^FAIL ' "$AGG" | sed 's/^/    /'
  echo "==> capturing live container logs to $LOGDIR/logs/"
  for c in $(docker ps -a --filter "label=com.docker.compose.project=${PROJECT}" --format '{{.Names}}'); do
    docker logs "$c" >"$LOGDIR/logs/${c}.log" 2>&1 || true
  done
fi

# ---------------------------------------------------------------- 5. teardown
if [ "${KEEP_UP:-0}" = "1" ]; then
  echo "==> KEEP_UP=1: leaving the swarm running"
else
  docker rm -f "${PROJECT}-client" "${PROJECT}-solo" >/dev/null 2>&1 || true
  bash "$HERE/stop_swarm.sh"
fi

[ "$FAIL" -eq 0 ]