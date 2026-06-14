#!/usr/bin/env bash
# P2P DuckDB grid node entrypoint.
#
# Loads the extension, becomes a host (`p2p_share`), optionally joins seeds
# (`p2p_join`), then keeps the DuckDB process alive so its QUIC worker accept
# loop keeps serving the swarm. The process is held open by feeding stdin from a
# FIFO whose write end never closes (the `tail -f`), mirroring the in-repo
# two-node grid test which keeps the host's stdin open.
set -euo pipefail

EXT=/node/duckdb_p2p.duckdb_extension
MEM="${P2P_SHARE_MEMORY:-64MB}"
THREADS="${P2P_SHARE_THREADS:-1}"
MAXJOBS="${P2P_SHARE_MAXJOBS:-2}"

mkdir -p "${P2P_CONFIG_DIR:-/node/state}"

INIT=/tmp/init.sql
{
  echo "LOAD '${EXT}';"
  echo "CALL p2p_share(memory => '${MEM}', threads => ${THREADS}, max_jobs => ${MAXJOBS}, data_classes => ['public']);"
  if [ -n "${BOOTSTRAP:-}" ]; then
    list=""
    IFS=','
    for s in ${BOOTSTRAP}; do
      [ -z "$s" ] && continue
      [ -n "$list" ] && list="${list},"
      list="${list}'${s}'"
    done
    unset IFS
    if [ -n "$list" ]; then
      echo "CALL p2p_join(bootstrap => [${list}]);"
    fi
  fi
  # Surface identity + listen addr for log greps (kept tiny).
  echo "SELECT 'NODE_READY ' || (SELECT value FROM p2p_share() WHERE key='node_id') || ' ' || (SELECT value FROM p2p_share() WHERE key='listen_addr') AS ready;"
} > "$INIT"

CMD=/tmp/cmd
rm -f "$CMD"
mkfifo "$CMD"
# Write the init SQL then hold the FIFO open forever so DuckDB stays alive.
( cat "$INIT"; tail -f /dev/null ) > "$CMD" &

exec duckdb -unsigned < "$CMD"
