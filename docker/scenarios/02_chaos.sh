#!/usr/bin/env bash
# Scenario 2 — chaos / resilience under real node deaths.
#
# Kills a fraction of the bootstrap workers (abrupt `docker kill`) and asserts
# remote queries STILL complete with the correct result — i.e. the coordinator's
# resilient re-dispatch routes around dead nodes to fresh survivors and quorum is
# still reached.
#
# NOTE: phi-accrual/SWIM exclusion and the FailedCommitment fine are verified
# deterministically against in-memory rails by `cargo test -p p2p-node --test
# resilience` (see 04_resilience_units.sh) — the extension's live Node wires
# plain StaticDiscovery, so those guarantees are proven at the library layer.
#   02_chaos.sh [bootstrap_size] [kill_count] [post_queries]
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

BSIZE="${1:-8}"
KILL="${2:-4}"
POSTQ="${3:-20}"
EXPECTED=500500
QUERY="SELECT sum(i) AS s FROM range(1,1001) t(i)"

POOL=(); while IFS= read -r _l; do POOL+=("$_l"); done < <(services | grep -E '^node' | shuf_lines | head -n "$BSIZE")
BOOT="$(boot_list "${POOL[@]}")"
echo "==> bootstrap workers ($BSIZE): ${POOL[*]}"

CLIENTC="$(ensure_client)"
run_q() { # -> prints value
  local sql="SELECT s FROM p2p_query('${QUERY}', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0)"
  req_query "$CLIENTC" "$BOOT" "$sql" 2>/dev/null | tr -d '[:space:]'
}

echo "==> baseline (pre-chaos): 5 remote queries"
base_pass=0
for i in $(seq 1 5); do [ "$(run_q)" = "$EXPECTED" ] && base_pass=$((base_pass+1)); done
echo "    baseline pass: $base_pass/5"

# Kill a fraction of the bootstrap workers, abruptly.
VICTIMS=("${POOL[@]:0:$KILL}")
echo "==> KILLING $KILL bootstrap workers: ${VICTIMS[*]}"
for v in "${VICTIMS[@]}"; do docker kill "$(container_of "$v")" >/dev/null 2>&1 || true; done
SURV=$((BSIZE-KILL))
echo "    survivors in bootstrap set: $SURV (quorum=2 requires >=2 alive)"

echo "==> post-chaos: $POSTQ remote queries against the SAME bootstrap ($KILL dead)…"
RES="$LOGDIR/chaos_results.txt"; : > "$RES"
for i in $(seq 1 "$POSTQ"); do echo "$(run_q)" >> "$RES"; done
pass=$(grep -cx "$EXPECTED" "$RES" || true)
echo "==> post-chaos query results: ${pass}/${POSTQ} correct after killing $KILL/$BSIZE bootstrap nodes"
if [ "$pass" -eq "$POSTQ" ] && [ "$SURV" -ge 2 ]; then
  echo "==> SCENARIO 2 PASS — re-dispatch routed around $KILL dead nodes; quorum still reached"
else
  echo "==> SCENARIO 2 result: $pass/$POSTQ (survivors=$SURV)"; tail -n 5 "$RES"
fi
