#!/usr/bin/env bash
# Library tier — node scenario suite (real loopback QUIC + MockEngine, no chain).
# Proves the adversarial/internal Query/Dispatch + Trust + Hosting invariants the
# live extension cannot inject (cheating, hedged-race, worker deadline, version
# negotiation, parallel streaming, reputation recency).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_scenarios.log"
echo "=================== LIBRARY — node scenarios ==================="
echo "==> cargo test -p p2p-node --test scenarios"
run_cargo_suite p2p-node scenarios "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert QRY-REMOTE-OK-01-lib              "$LOG" scenario_two_node_result_matches_locally_computed
cargo_assert QRY-QUORUM-AGREE-01               "$LOG" scenario_quorum_accepts_matching_hashes
cargo_assert QRY-MINORITY-CHEAT-01             "$LOG" scenario_malicious_worker_detected_and_penalized
cargo_assert TRU-CANARY-01                     "$LOG" scenario_canary_audit_slashes_failing_worker
cargo_assert VER-NEGOTIATE-01                  "$LOG" scenario_versioning_compatible_and_incompatible
cargo_assert HST-ADMIT-DATACLASS-01            "$LOG" scenario_worker_rejects_then_requester_routes_elsewhere
cargo_assert QRY-VERIFY-FAST-01                "$LOG" scenario_hedged_race_fastest_wins_losers_reset
cargo_assert HST-WORKER-DEADLINE-01            "$LOG" scenario_worker_timeout_masked_by_redundancy
cargo_assert QRY-INFLIGHT-RESULTSTREAM-01      "$LOG" scenario_large_result_parallel_streams_and_compression
cargo_assert TRN-PARALLEL-CLAMP-01             "$LOG" scenario_result_parallelism_overridable_per_call
cargo_assert QRY-PAGINATION-STREAM-01          "$LOG" scenario_large_result_streaming_with_backpressure
cargo_assert TRU-REP-CONFIDENCE-01             "$LOG" scenario_reputation_evolves_with_recency
cargo_assert SET-PAID-FREE-NOOP-01-lib         "$LOG" scenario_free_job_is_scored_without_chain
cargo_assert RES-CHURN-BOUNDED-01              "$LOG" scenario_churn_discovery_returns_bounded_healthy_set

finish "LIBRARY node scenarios"