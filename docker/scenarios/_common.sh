#!/usr/bin/env bash
# Shared helpers for the P2P DuckDB grid swarm scenarios.
#
# Conventions:
#   * Compose project name: $PROJECT (default p2pgrid).
#   * Service/DNS names: seed1.., node1..  (used in QUIC bootstrap URLs).
#   * Container names:    <project>-<service>-1 (used for docker exec / kill).
#   * Heavy output goes to $LOGDIR (/tmp/p2pgrid); callers read tails/greps only.
# NOTE: deliberately NOT using `-e`/`pipefail` — these orchestration scripts pipe
# into `head` (SIGPIPE) and tolerate individual query failures, counting outcomes
# explicitly instead.
set -u

PROJECT="${PROJECT:-p2pgrid}"
EXT="/node/duckdb_p2p.duckdb_extension"
LOGDIR="${LOGDIR:-/tmp/p2pgrid}"
mkdir -p "$LOGDIR"

# Portable line shuffle (macOS lacks `shuf`/`sort -R`): prefix each line with a
# random key, sort, strip.
shuf_lines() { awk 'BEGIN{srand()}{printf "%010.0f\t%s\n", rand()*1e9, $0}' | sort -k1,1n | cut -f2-; }

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

# The compose-created network name (project + default network "grid").
net_name() { echo "${PROJECT}_grid"; }

# Ensure a dedicated, generously-resourced REQUESTER container exists on the grid
# network. It does NOT host (entrypoint overridden) — it only runs short-lived
# requester `duckdb` processes, so concurrent requesters never compete with a
# hosting node's tight mem_limit. Returns the client container name.
CLIENT="${PROJECT}-client"
ensure_client() {
  if ! docker ps --format '{{.Names}}' | grep -qx "$CLIENT"; then
    docker rm -f "$CLIENT" >/dev/null 2>&1 || true
    docker run -d --name "$CLIENT" --network "$(net_name)" --hostname client \
      --memory 1500m --cpus 3 --entrypoint sleep p2p-node:latest infinity >/dev/null
  fi
  echo "$CLIENT"
}

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
# The requester dispatches its own `budget.per_job_memory_bytes` as the job's
# memory lease; keep it small (64 MiB) so workers admit it under their lean
# donated budget and the remote DuckDB memory_limit stays under the container cap.
req_query() {
  local cexec="$1" boot="$2" sql="$3"
  # The CLI prints each statement's result; the final SELECT's value is the LAST
  # stdout line. p2p_set/p2p_join tables precede it, so we return only that line.
  docker exec \
    -e P2P_BIND_ADDR=0.0.0.0:0 \
    -e "P2P_CONFIG_DIR=/tmp/req-$$-$RANDOM" \
    "$cexec" \
    duckdb -unsigned -list -noheader -c \
    "LOAD '${EXT}'; CALL p2p_set('budget.per_job_memory_bytes', '67108864'); CALL p2p_join(bootstrap => [${boot}]); ${sql}" \
    2>/dev/null | tail -n 1
}

# The requester's exec host is always the dedicated client container.
pick_requester_container() { ensure_client; }
