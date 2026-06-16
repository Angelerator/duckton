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

# All compose service names (seed-1.., honest-worker-1.., …) for this project.
services() {
  docker compose -p "$PROJECT" ps --services 2>/dev/null | sort -V
}

# Hosts that serve `public` and so can satisfy the default cross-node query:
# the seed mesh + the honest workers. (internal-host / oom-worker / remote-only
# are deliberately excluded — they refuse or cannot admit a public job.)
public_workers() {
  services | grep -E '^(seed|honest-worker)-[0-9]+$'
}

# Just the honest public workers (no seeds) — used where we want pure workers.
honest_workers() {
  services | grep -E '^honest-worker-[0-9]+$'
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

# Like req_query but returns COMBINED stdout+stderr (so error strings such as
# NoCandidates / InsufficientWorkers / WalletRequired are visible to assertions).
# Optional 4th arg is an economics/planner prelude SQL run before p2p_join.
req_query_all() {
  local cexec="$1" boot="$2" sql="$3" prelude="${4:-}"
  docker exec \
    -e P2P_BIND_ADDR=0.0.0.0:0 \
    -e "P2P_CONFIG_DIR=/tmp/req-$$-$RANDOM" \
    "$cexec" \
    duckdb -unsigned -list -noheader -c \
    "LOAD '${EXT}'; CALL p2p_set('budget.per_job_memory_bytes', '67108864'); ${prelude} CALL p2p_join(bootstrap => [${boot}]); ${sql}" \
    2>&1
}

# ---------------------------------------------------------------------------
# Single-container ("solo") helpers — for the scenarios that don't need the
# swarm (Admin/Config, local query, sandbox, prepared settlement). A solo
# container just runs the image with the entrypoint overridden so it does NOT
# host; each SQL call gets a FRESH, isolated P2P_CONFIG_DIR unless one is given.
# ---------------------------------------------------------------------------
SOLO="${PROJECT}-solo"
ensure_solo() {
  if ! docker ps --format '{{.Names}}' | grep -qx "$SOLO"; then
    docker rm -f "$SOLO" >/dev/null 2>&1 || true
    docker run -d --name "$SOLO" --memory 1g --cpus 2 \
      --entrypoint sleep p2p-node:latest infinity >/dev/null
  fi
  echo "$SOLO"
}

# Run LOAD + <sql> in the solo container, fresh config dir, combined stdout+stderr.
# An ephemeral QUIC bind (:0) so p2p_share/p2p_join calls never collide on :9494.
solo_sql() {
  local sql="$1"
  docker exec -e P2P_BIND_ADDR=0.0.0.0:0 -e "P2P_CONFIG_DIR=/tmp/s-$$-$RANDOM" "$SOLO" \
    duckdb -unsigned -list -c "LOAD '${EXT}'; ${sql}" 2>&1
}

# Like solo_sql but with a caller-chosen config dir (for persistence/restart tests).
solo_sql_dir() {
  local dir="$1" sql="$2"
  docker exec -e P2P_BIND_ADDR=0.0.0.0:0 -e "P2P_CONFIG_DIR=${dir}" "$SOLO" \
    duckdb -unsigned -list -c "LOAD '${EXT}'; ${sql}" 2>&1
}

# ---------------------------------------------------------------------------
# Tiny assertion framework. Each assertion prints `PASS <id>` / `FAIL <id> …`
# (the top-level runner greps these). `finish` exits non-zero if any failed.
# ---------------------------------------------------------------------------
PASS_N=0; FAIL_N=0
_ok() { PASS_N=$((PASS_N+1)); printf 'PASS %s\n' "$1"; }
_ng() { FAIL_N=$((FAIL_N+1)); printf 'FAIL %s :: %s\n' "$1" "$2"; }

# assert_have <id> <haystack> <literal-substring>
assert_have() {
  if printf '%s' "$2" | grep -qF -- "$3"; then _ok "$1"
  else _ng "$1" "want substring [$3] got [$(printf '%s' "$2" | tr '\n' '|' | cut -c1-220)]"; fi
}
# assert_re <id> <haystack> <ERE>
assert_re() {
  if printf '%s' "$2" | grep -qE -- "$3"; then _ok "$1"
  else _ng "$1" "want /$3/ got [$(printf '%s' "$2" | tr '\n' '|' | cut -c1-220)]"; fi
}
# assert_missing <id> <haystack> <literal-substring> — passes when ABSENT
assert_missing() {
  if printf '%s' "$2" | grep -qF -- "$3"; then _ng "$1" "did NOT expect [$3]"
  else _ok "$1"; fi
}
# assert_eq <id> <actual> <expected>
assert_eq() {
  if [ "$2" = "$3" ]; then _ok "$1"; else _ng "$1" "want [$3] got [$2]"; fi
}
finish() {
  printf -- '---- %s: %d passed, %d failed ----\n' "${1:-scenario}" "$PASS_N" "$FAIL_N"
  [ "$FAIL_N" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Library-tier helpers — adversarial / internal scenarios the live extension
# cannot inject (cheating/equivocation/attestation/transport/etc.) are proven
# deterministically by the workspace's cargo test suites over real loopback QUIC
# + MockEngine + in-memory rails (NO live TON, NO per-node gas).
# ---------------------------------------------------------------------------
REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

# Run a workspace integration-test binary once, capturing output to a log.
#   run_cargo_suite <crate> <test-bin> <logfile>
run_cargo_suite() {
  local crate="$1" bin="$2" log="$3"
  : "${SDKROOT:=$(xcrun --show-sdk-path 2>/dev/null || true)}"; export SDKROOT
  ( cd "$REPO_ROOT" && cargo test -p "$crate" --test "$bin" -- --nocapture ) >"$log" 2>&1 || true
}

# cargo_assert <scenario-id> <logfile> <test_fn> — PASS iff `test <fn> ... ok`.
cargo_assert() {
  if grep -qE "^test ${3} \.\.\. ok" "$2"; then _ok "$1"
  else _ng "$1" "cargo: test ${3} not ok (see $2)"; fi
}
