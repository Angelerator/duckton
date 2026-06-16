#!/usr/bin/env bash
# Group C (config/validation path) — Hosting/Swarm via p2p_share / p2p_join.
#
# Single container: the SHARE/JOIN validation + clamping + (re)build surface.
# The ADMISSION behaviors (HST-ADMIT-*, HST-BID-ETA, HST-WORKER-*, HST-PROGRESS-*)
# are worker-internals proven at the library tier (cargo test -p p2p-node --test
# scenarios / resilience); here we assert the observable SQL surface.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP C — HOSTING/SWARM (solo) ==================="

# HST-SHARE-01 — become a host: status=hosting + node_id + listen_addr + budget.
out="$(solo_sql "CALL p2p_share(memory=>'128MB', threads=>2, max_jobs=>3);")"
assert_have HST-SHARE-01a "$out" "share|status|hosting"
assert_have HST-SHARE-01b "$out" "share|node_id|b3:"
assert_have HST-SHARE-01c "$out" "share|listen_addr|0.0.0.0:"
assert_have HST-SHARE-01d "$out" "share|memory_bytes|134217728"

# HST-SHARE-MEMPARSE-01 — bad memory string is rejected.
out="$(solo_sql "CALL p2p_share(memory=>'notabyte');")"
assert_have HST-SHARE-MEMPARSE-01 "$out" "could not parse memory"

# HST-SHARE-CLASS-01 — unknown data class is rejected.
out="$(solo_sql "CALL p2p_share(data_classes=>['bogus']);")"
assert_have HST-SHARE-CLASS-01 "$out" "unknown data class"

# HST-SHARE-CLAMP-01 — threads / max_jobs are floored to 1.
out="$(solo_sql "CALL p2p_share(threads=>0, max_jobs=>0);")"
assert_have HST-SHARE-CLAMP-01a "$out" "share|threads|1"
assert_have HST-SHARE-CLAMP-01b "$out" "share|max_jobs|1"

# HST-JOIN-01 — join persists the bootstrap + rebuilds the node.
out="$(solo_sql "CALL p2p_join(bootstrap=>['quic://seed-1:9494']);")"
assert_have HST-JOIN-01a "$out" "join|status|joined"
assert_have HST-JOIN-01b "$out" "join|bootstrap|quic://seed-1:9494"

# HST-JOIN-EMPTY-01 — joining with no seeds errors.
out="$(solo_sql "CALL p2p_join(bootstrap=>[]);")"
assert_have HST-JOIN-EMPTY-01 "$out" "provide one or more seeds"

finish "GROUP C (Hosting/Swarm)"