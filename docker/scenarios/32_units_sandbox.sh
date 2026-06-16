#!/usr/bin/env bash
# Library tier — OS sandbox suite (egress allow-list derivation, no-op degrade,
# rlimit child caps, macOS seatbelt scoped read). Complements the in-DuckDB
# lockdown asserted live in 13_sandbox.sh.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_sandbox.log"
echo "=================== LIBRARY — node sandbox ==================="
echo "==> cargo test -p p2p-node --test sandbox"
run_cargo_suite p2p-node sandbox "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert SBX-EGRESS-DERIVE-01     "$LOG" egress_allowlist_is_derived_from_storage_config
cargo_assert SBX-NOOP-WARN-01         "$LOG" disabled_sandbox_is_noop_and_runs_program
cargo_assert SBX-RLIMIT-01            "$LOG" rlimit_file_size_cap_constrains_a_runaway_writer
cargo_assert SBX-RLIMIT-FD-CPU-01     "$LOG" rlimit_fd_and_cpu_caps_are_applied_to_child
cargo_assert SBX-FIXTURE-ALLOWED-01   "$LOG" macos_seatbelt_blocks_disallowed_read_but_allows_scoped_read

finish "LIBRARY node sandbox"