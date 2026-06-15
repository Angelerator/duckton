#!/usr/bin/env bash
# P2P DuckDB grid node entrypoint.
#
# Loads the extension and becomes a host (`p2p_share`), then keeps the DuckDB
# process alive so its QUIC worker accept loop keeps serving the swarm. The
# process is held open by feeding stdin from a FIFO whose write end never closes
# (the `tail -f`), mirroring the in-repo two-node grid test which keeps the
# host's stdin open.
#
# IMPORTANT: a node binds a FIXED port (9494), and every `p2p_share`/`p2p_join`
# CALL rebuilds + rebinds the node. Calling both on a fixed port collides
# ("address already in use"). So bootstrap seeds are supplied via the
# `P2P_BOOTSTRAP` env (read by the extension's config layer) and we issue exactly
# ONE `p2p_share` — which builds the node from the effective config (seeds
# included) and starts hosting with a single bind.
set -euo pipefail

EXT=/node/duckdb_p2p.duckdb_extension
# Total donated budget (admission accounting, not a hard RAM reservation). Must
# be >= per-job memory or admission rejects every offer with "at capacity".
MEM="${P2P_SHARE_MEMORY:-256MB}"
THREADS="${P2P_SHARE_THREADS:-1}"
MAXJOBS="${P2P_SHARE_MAXJOBS:-2}"
# Per-job memory lease (bytes). Defaults in config are 1 GiB which exceeds a lean
# donated budget; shrink it so a job is admissible AND its DuckDB memory_limit
# stays well under the container's mem_limit. 64 MiB = 67108864.
PERJOB_BYTES="${P2P_SHARE_PERJOB_BYTES:-67108864}"

# ---------------------------------------------------------------------------
# Input validation — these values are interpolated into DuckDB SQL; reject
# anything that does not match the expected allowlist pattern.
#   MEM:         digits optionally followed by a size suffix (KB/MB/GB/TB,
#                case-insensitive) — e.g. "256MB", "1GB".
#   THREADS:     positive integer.
#   MAXJOBS:     positive integer.
#   PERJOB_BYTES: positive integer (plain bytes, no suffix).
# ---------------------------------------------------------------------------
validate_mem() {
    # Accepts: <digits>[<suffix>] where suffix in kKmMgGtT (optionally followed by 'b'/'B')
    if ! printf '%s' "$1" | grep -qE '^[0-9]+([kKmMgGtT][bB]?)?$'; then
        printf 'ERROR: invalid value for %s: %s (expected numeric with optional size suffix e.g. 256MB)\n' "$2" "$1" >&2
        exit 1
    fi
}
validate_uint() {
    if ! printf '%s' "$1" | grep -qE '^[0-9]+$'; then
        printf 'ERROR: invalid value for %s: %s (expected non-negative integer)\n' "$2" "$1" >&2
        exit 1
    fi
}
validate_posint() {
    if ! printf '%s' "$1" | grep -qE '^[1-9][0-9]*$'; then
        printf 'ERROR: invalid value for %s: %s (expected positive integer)\n' "$2" "$1" >&2
        exit 1
    fi
}

validate_mem     "$MEM"          "P2P_SHARE_MEMORY"
validate_posint  "$THREADS"      "P2P_SHARE_THREADS"
validate_posint  "$MAXJOBS"      "P2P_SHARE_MAXJOBS"
validate_uint    "$PERJOB_BYTES" "P2P_SHARE_PERJOB_BYTES"

mkdir -p "${P2P_CONFIG_DIR:-/node/state}"

# Public knob BOOTSTRAP -> the extension's P2P_BOOTSTRAP (comma-separated seeds).
if [ -n "${BOOTSTRAP:-}" ]; then
  export P2P_BOOTSTRAP="${BOOTSTRAP}"
fi

INIT=/tmp/init.sql
{
  echo "LOAD '${EXT}';"
  # Shrink the per-job memory/thread lease BEFORE sharing (p2p_set persists to the
  # runtime layer without rebinding; p2p_share then builds the node from it).
  echo "CALL p2p_set('budget.per_job_memory_bytes', '${PERJOB_BYTES}');"
  echo "CALL p2p_set('budget.per_job_threads', '1');"
  echo "CALL p2p_share(memory => '${MEM}', threads => ${THREADS}, max_jobs => ${MAXJOBS}, data_classes => ['public']);"
  # Readiness marker (does NOT re-call p2p_share, which would rebind the port).
  echo "SELECT 'NODE_READY' AS ready;"
} > "$INIT"

CMD=/tmp/cmd
rm -f "$CMD"
mkfifo "$CMD"
# Write the init SQL then hold the FIFO open forever so DuckDB stays alive.
( cat "$INIT"; tail -f /dev/null ) > "$CMD" &

exec duckdb -unsigned < "$CMD"
