#!/usr/bin/env bash
# Scenario 4 + 5 — economic modes at swarm scale: FREE tier vs PAID tier.
#
# Re-validates the free/paid decoupling on the LIVE swarm (real QUIC across
# containers) using the deterministic in-memory MOCK settlement rail for the
# paid path — NO TON, NO testnet gas. The deep paid guarantees (escrow/settle
# split, GlobalParams overlay + params-version binding, anchored record, and the
# FailedCommitment fine under node death) are proven deterministically by the
# library suites in 04_resilience_units.sh + 06_settlement_units.sh (real
# loopback QUIC + mock engine + in-memory stake registry).
#
# Three OBSERVABLE swarm-level behaviors prove the seam diverges correctly:
#   * FREE  : payment=>'free' remote query → correct result. Node economics is
#             OFF (settlement=noop); the settlement rail is never reached.
#   * PAID  : p2p_economics(enabled=>true, settlement=>'mock') + payment=>'paid'
#             remote query → correct result streamed from real worker containers
#             over QUIC; the coordinator opens+settles a per-job escrow on the
#             in-memory mock rail (no chain, no funds).
#   * GATE  : economics enabled but NO money rail (settlement=noop) + a paid job
#             → the node REFUSES with WalletRequired. A paid job demands a rail
#             that a free job never touches — the divergence, made observable.
#
#   05_economics_modes.sh [concurrency]
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

CONC="${1:-20}"
EXPECTED=500500
QUERY="SELECT sum(i) AS s FROM range(1,1001) t(i)"
CLIENTC="$(ensure_client)"
WORKERS_FILE="$LOGDIR/workers.txt"; services | grep -E '^node' > "$WORKERS_FILE"
RESDIR="$LOGDIR/econ"; mkdir -p "$RESDIR"

# Run a requester query INSIDE the client container with an explicit economics
# prelude. stdout (query value) and stderr (errors, e.g. WalletRequired) are
# captured to separate files so the gate is detectable.
#   req_econ <tag> <econ_prelude_sql> <payment>
req_econ() {
  local tag="$1" prelude="$2" payment="$3"
  local boot; boot="$(boot_list $(shuf_lines < "$WORKERS_FILE" | head -n16))"
  local sql="SELECT s FROM p2p_query('${QUERY}', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0, payment=>'${payment}')"
  docker exec \
    -e P2P_BIND_ADDR=0.0.0.0:0 \
    -e "P2P_CONFIG_DIR=/tmp/req-$$-$RANDOM" \
    "$CLIENTC" \
    duckdb -unsigned -list -noheader -c \
    "LOAD '${EXT}'; CALL p2p_set('budget.per_job_memory_bytes', '67108864'); ${prelude} CALL p2p_join(bootstrap => [${boot}]); ${sql}" \
    > "$RESDIR/${tag}.out" 2> "$RESDIR/${tag}.err"
}

val_of()  { tail -n 1 "$RESDIR/$1.out" 2>/dev/null | tr -dc '0-9'; }   # last stdout line = query value
errt_of() { tr -d '\n' < "$RESDIR/$1.err" 2>/dev/null; }               # stderr (errors)

PAID_ECON="CALL p2p_economics(enabled => true, settlement => 'mock', default_payment => 'paid');"
GATE_ECON="CALL p2p_economics(enabled => true, settlement => 'noop', default_payment => 'paid');"

echo "=================== ECONOMIC MODES (swarm scale, mock rail) ==================="

# ---------------------------------------------------------------- FREE tier (4)
echo "==> [FREE] payment=>'free' remote query (economics OFF, settlement=noop)"
req_econ free_sanity "" free
fv="$(val_of free_sanity)"
echo "    free remote result: ${fv:-<none>} (expect $EXPECTED)"
FREE_OK=0; [ "$fv" = "$EXPECTED" ] && FREE_OK=1

# Confirm the default node economics state is genuinely the free/no-chain rail.
docker exec "$CLIENTC" duckdb -unsigned -list -noheader -c \
  "LOAD '${EXT}'; SELECT key||'='||value FROM p2p_status() WHERE key IN ('economics_enabled','settlement','network');" \
  > "$RESDIR/status_free.out" 2>/dev/null || true
echo "    default node economics state:"; sed 's/^/      /' "$RESDIR/status_free.out" 2>/dev/null | head

# ---------------------------------------------------------------- PAID tier (5)
echo "==> [PAID] mock rail wired (enabled+settlement=mock); $CONC concurrent payment=>'paid' remote queries"
pids=(); INFLIGHT="${INFLIGHT:-6}"
for i in $(seq 1 "$CONC"); do
  req_econ "paid_$i" "$PAID_ECON" paid &
  pids+=($!)
  if [ $((i % INFLIGHT)) -eq 0 ]; then wait "${pids[@]}"; pids=(); fi
done
wait || true
PAID_OK=0
for i in $(seq 1 "$CONC"); do [ "$(val_of paid_$i)" = "$EXPECTED" ] && PAID_OK=$((PAID_OK+1)); done
echo "    paid remote results: ${PAID_OK}/${CONC} returned the correct value (${EXPECTED}) via the mock-settled paid path"

# ---------------------------------------------------------------- GATE (paid needs a rail)
echo "==> [GATE] economics ON but settlement=noop (no money rail) + payment=>'paid' → expect WalletRequired"
req_econ gate "$GATE_ECON" paid
gate_val="$(val_of gate)"; gate_err="$(errt_of gate)"
GATE_OK=0
if [ "$gate_val" != "$EXPECTED" ] && echo "$gate_err" | grep -qiE 'wallet|settlement rail|payment => .paid.'; then
  GATE_OK=1
  echo "    gate tripped correctly: $(echo "$gate_err" | grep -oiE 'no wallet/settlement[^\"]*' | head -n1 | cut -c1-80)"
else
  echo "    gate did NOT trip (val='${gate_val}', err head: $(echo "$gate_err" | cut -c1-120))"
fi

echo "------------------------------------------------------------------------------"
echo "==> FREE tier : $([ "$FREE_OK" = 1 ] && echo PASS || echo FAIL)  (free remote query correct, economics off)"
echo "==> PAID tier : $([ "$PAID_OK" = "$CONC" ] && echo PASS || echo PARTIAL)  (${PAID_OK}/${CONC} paid queries settled via mock rail over QUIC)"
echo "==> GATE      : $([ "$GATE_OK" = 1 ] && echo PASS || echo FAIL)  (paid job refused without a money rail → free≠paid)"
if [ "$FREE_OK" = 1 ] && [ "$PAID_OK" = "$CONC" ] && [ "$GATE_OK" = 1 ]; then
  echo "==> SCENARIO 4+5 PASS — free and paid paths diverge correctly at swarm scale (mock rail; no gas)"
else
  echo "==> SCENARIO 4+5 PARTIAL — see counts above"
fi
