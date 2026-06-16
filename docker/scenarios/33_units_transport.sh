#!/usr/bin/env bash
# Library tier — transport version negotiation over real loopback QUIC.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_transport.log"
echo "=================== LIBRARY — transport versioning ==================="
echo "==> cargo test -p p2p-transport --test versioning"
run_cargo_suite p2p-transport versioning "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert VER-MINOR-01     "$LOG" compatible_versions_negotiate_common_lower
cargo_assert VER-BELOWMIN-01  "$LOG" peer_below_min_supported_is_rejected_typed
cargo_assert VER-MAJOR-01     "$LOG" different_major_cannot_connect

finish "LIBRARY transport versioning"