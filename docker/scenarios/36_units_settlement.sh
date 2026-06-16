#!/usr/bin/env bash
# Library tier — paid settlement integration over real loopback QUIC + the
# in-memory MOCK rail (escrow open/settle split, GlobalParams overlay + version
# binding, free/paid divergence). NO live TON, NO per-node gas.
# (Companion to the legacy summary-style 06_settlement_units.sh.)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_settlement.log"
echo "=================== LIBRARY — node settlement_integration ==================="
echo "==> cargo test -p p2p-node --test settlement_integration"
run_cargo_suite p2p-node settlement_integration "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert SET-PAID-FREE-NOOP-01      "$LOG" free_job_runs_full_grid_path_with_zero_chain_calls
cargo_assert SET-PAID-SETTLE-01         "$LOG" paid_job_settles_split_and_anchors_record
cargo_assert SET-PARAMS-SYNC-01         "$LOG" paid_job_syncs_params_overlays_config_and_binds_version
cargo_assert SET-ESCROW-BOUND-01        "$LOG" paid_settlement_rejects_payout_exceeding_escrow
cargo_assert SET-STAKE-SEAM-01          "$LOG" stake_seam_consulted_only_when_paid_and_enabled

finish "LIBRARY node settlement_integration"