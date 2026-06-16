#!/usr/bin/env bash
# Library tier — resilience/churn + broken-commitment fine over real loopback
# QUIC + MockEngine + in-memory stake registry (NO live TON, NO per-node gas).
# (Companion to the legacy summary-style 04_resilience_units.sh; this emits
# per-id PASS/FAIL for the runner.)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_resilience.log"
echo "=================== LIBRARY — node resilience ==================="
echo "==> cargo test -p p2p-node --test resilience"
run_cargo_suite p2p-node resilience "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert HST-WORKER-DEADLINE-01-lib "$LOG" host_job_timeout_abandons_and_redispatches
cargo_assert RES-ALLSILENT-01           "$LOG" all_silent_redispatches_to_a_fresh_set
cargo_assert RES-STALL-ABORT-01         "$LOG" progress_stall_detected_redispatches
cargo_assert RES-LIVENESS-EXCLUDE-01    "$LOG" phi_convicted_node_is_excluded_from_selection
cargo_assert RES-MAXRETRY-01            "$LOG" unlimited_retry_until_a_later_healthy_node_succeeds
cargo_assert QRY-INFEASIBLE-01          "$LOG" consensus_infeasible_query_stops_without_retry
cargo_assert RES-TOKENBUCKET-01         "$LOG" retry_budget_caps_a_storm
cargo_assert RES-MAXDUR-01              "$LOG" per_call_overrides_apply_and_max_total_duration_caps
cargo_assert SET-FINE-COMMIT-01         "$LOG" paid_broken_commitment_is_fined
cargo_assert QRY-INFEASIBLE-PAID-01     "$LOG" consensus_infeasible_paid_job_fines_no_one
cargo_assert RES-WINNER-NODELIVER-FREE-01 "$LOG" free_job_non_delivering_node_is_not_fined
cargo_assert SET-FINE-UNSTAKED-NOOP-01  "$LOG" unstaked_provider_is_not_fined

finish "LIBRARY node resilience"