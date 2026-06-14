#!/usr/bin/env bash
# Scenario 5b — paid settlement guarantees against in-memory rails.
#
# The extension's live swarm node wires the money rail from [economics] but NOT
# a stake registry / on-chain GlobalParams source, so the deep paid guarantees
# are proven DETERMINISTICALLY by the library's settlement-integration suite over
# real loopback QUIC (coordinator + workers) + the mock settlement rail + an
# in-memory stake registry — NO live TON, NO per-node gas. It covers exactly the
# §8/§10/§12 paid path the swarm exercises end-to-end but cannot assert from the
# outside:
#   * open-escrow-per-job → settle the payout split (winner + participation
#     commissions + platform fee), bounded by the escrowed bid B;
#   * GlobalParams policy overlay onto a PAID job (fee overlay) + params-version
#     bound into the per-job escrow terms + stamped into the anchored record;
#   * free vs paid divergence: a FREE job NEVER engages the rail (a settlement
#     that panics on any call is wired and never fires), and the stake_factor
#     ranking seam is consulted ONLY when paid AND economics enabled.
#
# Output is written to /tmp; only the summary is printed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LOG=/tmp/p2pgrid/settlement_test.log
mkdir -p /tmp/p2pgrid

echo "==> cargo test -p p2p-node --test settlement_integration (in-memory mock rail; escrow/settle/overlay/version-binding + free/paid divergence)"
( cd "$ROOT" && cargo test -p p2p-node --test settlement_integration -- --nocapture ) >"$LOG" 2>&1 || true

echo "==> result line:"
grep -E "test result:" "$LOG" | tail -n 1 | sed 's/^/    /'
echo "==> key cases:"
grep -E "^test (free_job_runs_full_grid_path_with_zero_chain_calls|free_job_never_engages_coordinator_settlement|paid_job_drives_coordinator_open_settle_anchor|paid_job_settles_split_and_anchors_record|paid_job_settles_two_agreeing_participant_commissions|paid_settlement_rejects_payout_exceeding_escrow|paid_job_syncs_params_overlays_config_and_binds_version|stake_seam_consulted_only_when_paid_and_enabled|paid_stake_factor_decides_single_replica_winner) " "$LOG" \
  | sed 's/^/    /' || true
