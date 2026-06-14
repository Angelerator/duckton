#!/usr/bin/env bash
# Wait until at least <min_ready> nodes have logged NODE_READY (bound their QUIC
# endpoint + are hosting). Lean: counts greps of container logs, not full dumps.
#   00_wait_ready.sh [min_ready] [timeout_secs]
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

TOTAL=$(containers | wc -l | tr -d ' ')
MIN_READY="${1:-$TOTAL}"
TIMEOUT="${2:-120}"

echo "==> waiting for >= $MIN_READY / $TOTAL nodes to report NODE_READY (timeout ${TIMEOUT}s)"
deadline=$(( $(date +%s) + TIMEOUT ))
while :; do
  ready=0
  for c in $(containers); do
    if docker logs "$c" 2>&1 | grep -q "NODE_READY"; then
      ready=$((ready+1))
    fi
  done
  now=$(date +%s)
  if [ "$ready" -ge "$MIN_READY" ]; then
    echo "==> READY: $ready / $TOTAL nodes hosting"
    exit 0
  fi
  if [ "$now" -ge "$deadline" ]; then
    echo "==> TIMEOUT: only $ready / $TOTAL ready after ${TIMEOUT}s"
    exit 1
  fi
  sleep 3
done
