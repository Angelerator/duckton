#!/usr/bin/env bash
# Library tier — anti-abuse suite (candidates carry real node_ids, so the
# selection-time effects the live extension cannot show are proven here).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_antiabuse.log"
echo "=================== LIBRARY — node antiabuse ==================="
echo "==> cargo test -p p2p-node --test antiabuse"
run_cargo_suite p2p-node antiabuse "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert QRY-NONDET-01            "$LOG" nondeterministic_query_marks_non_verifiable_and_applies_no_penalty
cargo_assert TRU-REQTRUST-GRIEF-01    "$LOG" requester_trust_weighting_gates_new_senders
cargo_assert ABU-COSTGATE-ROWS-01     "$LOG" cost_gate_declines_over_budget_offer
cargo_assert ABU-CAND-EXCLUDE-01      "$LOG" blocklist_excludes_blocked_candidate_from_selection
cargo_assert ABU-WORKER-REFUSE-01     "$LOG" worker_refuses_blocked_requester
cargo_assert ABU-RATELIMIT-01         "$LOG" free_mode_rate_limit_triggers_per_requester
cargo_assert ABU-FAULTATTR-CHEAT-01   "$LOG" deterministic_cheater_still_penalized_by_default

finish "LIBRARY node antiabuse"