#!/usr/bin/env bash
# Shared helpers for the P2P DuckDB grid swarm scenarios.
#
# Conventions:
#   * Compose project name: $PROJECT (default p2pgrid).
#   * Service/DNS names: seed1.., node1..  (used in QUIC bootstrap URLs).
#   * Container names:    <project>-<service>-1 (used for docker exec / kill).
#   * Heavy output goes to $LOGDIR (/tmp/p2pgrid); callers read tails/greps only.
set -euo pipefail

PROJECT="${PROJECT:-p2pgrid}"
EXT="/node/duckdb_p2p.duckdb_extension"
LOGDIR="${LOGDIR:-/tmp/p2pgrid}"
mkdir -p "$LOGDIR"

# All compose service names (seed1.., node1..) for this project.
services() {
  docker compose -p "$PROJECT" ps --services 2>/dev/null | sort -V
}

# Running container names for this project.
containers() {
  docker ps --filter "label=com.docker.compose.project=${PROJECT}" --format '{{.Names}}'
}

# Map a service name (node1) -> container name (p2pgrid-node1-1).
container_of() { echo "${PROJECT}-$1-1"; }

# Build a SQL list literal of quic:// bootstrap URLs from service names.
#   boot_list node1 node2 seed1  ->  'quic://node1:9494','quic://node2:9494','quic://seed1:9494'
boot_list() {
  local out=""
  for h in "$@"; do
    [ -n "$out" ] && out="${out},"
    out="${out}'quic://${h}:9494'"
  done
  echo "$out"
}

# Run a one-shot requester query INSIDE a container against a bootstrap set.
#   req_query <exec_container> <boot_list_literal> <sql>
# Uses an ephemeral bind port + isolated config dir so it never clashes with the
# container's long-running host (which owns :9494). Prints query stdout.
req_query() {
  local cexec="$1" boot="$2" sql="$3"
  docker exec \
    -e P2P_BIND_ADDR=0.0.0.0:0 \
    -e "P2P_CONFIG_DIR=/tmp/req-$$-$RANDOM" \
    "$cexec" \
    duckdb -unsigned -list -noheader -c \
    "LOAD '${EXT}'; CALL p2p_join(bootstrap => [${boot}]); ${sql}" \
    2> >(tail -n 3 >&2)
}

# Pick a healthy worker container to act as the requester's exec host.
pick_requester_container() {
  containers | grep -E "${PROJECT}-node" | sort -V | head -n 1
}
