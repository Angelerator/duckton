#!/usr/bin/env bash
# =============================================================================
# scenarios_testnet.sh — the COMPREHENSIVE on-chain scenario runner for the P2P
# DuckDB-over-QUIC settlement contracts (companion to scripts/testnet_e2e.sh).
#
# It drives every TESTNET-BROADCASTABLE *positive* scenario from ton/SCENARIOS.md
# as a real broadcast (scripts/scenarios_testnet.tolk), reads the contract
# getters back, asserts the state change, then:
#   * parses the `::CHECK::<name>::PASS|FAIL` markers into a summary,
#   * prints https://testnet.tonviewer.com/<addr> links for every deployed/used
#     contract and `acton rpc trace` of a representative tx per contract,
#   * runs the Acton EMULATOR suite (`acton test`) so the NEGATIVE +
#     TIME-GATED scenarios (which must NOT be broadcast — they would leave
#     scary failed txs and burn gas) are covered in the same report.
#
# Nothing is left untested: live broadcasts prove the positive on-chain reality;
# the emulator proves the negatives + time-gated positives deterministically.
#
# ---------------------------------------------------------------------------
# env:
#   WALLET_NAME=deployer        Acton wallet that signs (must hold testnet GRAM)
#   WINNER_NAME=winner          a DISTINCT wallet used as the escrow winner
#                               (B1: winner != arbiter). Auto-created if missing.
#   REUSE_DEPLOYED=1            reuse the StakeVault/RecordAnchor from
#                               ton/deployments/testnet.env (gas-light) instead
#                               of deploying fresh ones. GlobalParams + JobEscrow
#                               are always fresh (GP needs upgradeDelay=0; escrow
#                               is per-job).
#   RUN_VAULT/RUN_ANCHOR/RUN_GP/RUN_ESCROW = 0|1   section gates (default 1)
#   RUN_EMULATOR=1             also run `acton test` (default 1)
#   ALLOW_NO_API_KEY=1         run keyless (Toncenter self-throttles ~1 RPS)
#   TON_TESTNET_API_KEY=...    Toncenter testnet key (-> TONCENTER_TESTNET_API_KEY)
#   NET=testnet                TESTNET ONLY — never mainnet
#
# Testnet only. No mainnet. No commits.
# =============================================================================
set -euo pipefail

if [[ -t 1 ]]; then
  C_RESET=$'\033[0m'; C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YLW=$'\033[33m'; C_BLU=$'\033[34m'; C_BOLD=$'\033[1m'
else
  C_RESET=""; C_RED=""; C_GRN=""; C_YLW=""; C_BLU=""; C_BOLD=""
fi
log()  { printf '%s==>%s %s\n' "$C_BLU" "$C_RESET" "$*"; }
ok()   { printf '%s ok %s %s\n' "$C_GRN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YLW" "$C_RESET" "$*" >&2; }
die()  { printf '%sFAIL%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; exit 1; }
step() { printf '\n%s%s== %s ==%s\n' "$C_BOLD" "$C_BLU" "$*" "$C_RESET"; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TON_DIR="$REPO_ROOT/ton"
cd "$REPO_ROOT"

NET="${NET:-testnet}"
[[ "$NET" == "testnet" ]] || die "this runner is TESTNET ONLY (NET=$NET refused)"
WALLET_NAME="${WALLET_NAME:-deployer}"
WINNER_NAME="${WINNER_NAME:-winner}"
WALLET_VERSION="${WALLET_VERSION:-v5r1}"
EXPLORER_BASE="https://testnet.tonviewer.com"
OUT_DIR="$REPO_ROOT/${OUT_DIR:-ton/deployments}"
LOG_DIR="$OUT_DIR/logs"; mkdir -p "$LOG_DIR"
RUNNER_LOG="$LOG_DIR/scenarios.log"

need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
need acton; need jq

if [[ -n "${TON_TESTNET_API_KEY:-}" ]]; then
  export TONCENTER_TESTNET_API_KEY="$TON_TESTNET_API_KEY"
elif [[ -z "${TONCENTER_TESTNET_API_KEY:-}" && "${ALLOW_NO_API_KEY:-0}" != "1" ]]; then
  die "set TON_TESTNET_API_KEY (Toncenter testnet key), or pass ALLOW_NO_API_KEY=1 to run keyless (throttled)"
fi
if command -v xcrun >/dev/null 2>&1 && [[ -z "${SDKROOT:-}" ]]; then export SDKROOT="$(xcrun --show-sdk-path)"; fi

# --- wallets ----------------------------------------------------------------
step "0. wallets"
acton wallet list --json 2>/dev/null | jq -e --arg n "$WALLET_NAME" '.wallets[]?|select(.name==$n)' >/dev/null \
  || die "signing wallet '$WALLET_NAME' not found — import/create it first (see docs/TESTNET.md)"
WALLET_ADDR="$(acton wallet list --json 2>/dev/null | jq -r --arg n "$WALLET_NAME" '.wallets[]?|select(.name==$n)|.address' | head -1)"
ok "signer '$WALLET_NAME': $WALLET_ADDR"

if ! acton wallet list --json 2>/dev/null | jq -e --arg n "$WINNER_NAME" '.wallets[]?|select(.name==$n)' >/dev/null; then
  log "creating winner wallet '$WINNER_NAME' (escrow payout target; B1 winner != arbiter)"
  acton wallet new --name "$WINNER_NAME" --global --version "$WALLET_VERSION" --secure false >/dev/null 2>&1 \
    || die "could not create winner wallet '$WINNER_NAME'"
fi
WINNER_ADDR="$(acton wallet list --json 2>/dev/null | jq -r --arg n "$WINNER_NAME" '.wallets[]?|select(.name==$n)|.address' | head -1)"
[[ -n "$WINNER_ADDR" && "$WINNER_ADDR" != "$WALLET_ADDR" ]] || die "winner wallet must exist and differ from the signer"
ok "winner '$WINNER_NAME': $WINNER_ADDR"
log "balances:"; acton wallet list --balance 2>/dev/null | sed 's/^/    /' || true

# --- optionally reuse already-deployed StakeVault / RecordAnchor (gas-light) -
PIN_ENV=()
if [[ "${REUSE_DEPLOYED:-0}" == "1" && -f "$OUT_DIR/$NET.env" ]]; then
  # shellcheck disable=SC1090
  set -a; . "$OUT_DIR/$NET.env"; set +a
  [[ -n "${TON_TESTNET_VAULT_ADDR:-}"  ]] && PIN_ENV+=("VAULT_ADDR=$TON_TESTNET_VAULT_ADDR")
  [[ -n "${TON_TESTNET_ANCHOR_ADDR:-}" ]] && PIN_ENV+=("ANCHOR_ADDR=$TON_TESTNET_ANCHOR_ADDR")
  log "REUSE_DEPLOYED=1 — pinning ${PIN_ENV[*]:-<none>} from $OUT_DIR/$NET.env"
fi

# --- build + run the live comprehensive runner ------------------------------
step "1. build contracts"
( cd "$TON_DIR" && acton build ) >/dev/null || die "acton build failed"
ok "contracts compiled"

step "2. live comprehensive scenarios (broadcast -> trace -> getters -> assert)"
DEADLINE=$(( $(date +%s) + 3600 ))
set +e
( cd "$TON_DIR" && \
  env "${PIN_ENV[@]}" \
    WINNER_ADDR="$WINNER_ADDR" \
    SCN_DEADLINE="$DEADLINE" \
    RUN_VAULT="${RUN_VAULT:-1}" RUN_ANCHOR="${RUN_ANCHOR:-1}" \
    RUN_GP="${RUN_GP:-1}" RUN_ESCROW="${RUN_ESCROW:-1}" \
    acton script scripts/scenarios_testnet.tolk --net "$NET" --explorer tonviewer ) 2>&1 | tee "$RUNNER_LOG"
RC=${PIPESTATUS[0]}
set -e
[[ $RC -eq 0 ]] || warn "runner exited non-zero ($RC) — parsing partial checks"

# --- parse ::CHECK:: markers + ::ADDR:: lines -------------------------------
# (Portable: macOS ships bash 3.2 — no `mapfile`, no `declare -A`.)
step "3. on-chain trackability (explorer links + representative traces)"
grep -oE '::ADDR::[a-z_]+=[A-Za-z0-9_-]+' "$RUNNER_LOG" 2>/dev/null | sort -u | while IFS= read -r l; do
  name="${l#::ADDR::}"; name="${name%%=*}"
  addr="${l##*=}"
  [[ -z "$addr" ]] && continue
  printf '  %-13s %s/%s\n' "$name" "$EXPLORER_BASE" "$addr"
  hash="$( ( cd "$TON_DIR" && acton rpc info "$addr" --net "$NET" ) 2>/dev/null | grep -oiE 'last tx hash: +\S+' | awk '{print $4}' | head -1 || true)"
  if [[ -n "$hash" ]]; then
    printf '      last tx: %s\n' "$hash"
    ( cd "$TON_DIR" && acton rpc trace "$hash" --net "$NET" --summary ) 2>/dev/null | sed 's/^/      /' || true
  fi
done

# --- summary of live checks -------------------------------------------------
step "4. live scenario summary"
grep -E '::CHECK::[a-z_]+::(PASS|FAIL|SKIP)|::CHECK::[a-z_]+::SKIP' "$RUNNER_LOG" 2>/dev/null \
  | sed -E 's/^.*::CHECK::([a-z_]+)::(PASS|FAIL|SKIP).*/\2  \1/' \
  | while IFS= read -r ln; do printf '  %s\n' "$ln"; done || true
LIVE_PASS=$(grep -cE '::CHECK::[a-z_]+::PASS' "$RUNNER_LOG" 2>/dev/null || echo 0)
LIVE_FAIL=$(grep -cE '::CHECK::[a-z_]+::FAIL' "$RUNNER_LOG" 2>/dev/null || echo 0)
LIVE_SKIP=$(grep -cE '::CHECK::[a-z_]+::SKIP|::SKIP' "$RUNNER_LOG" 2>/dev/null || echo 0)
printf '%s\n' "  live: ${LIVE_PASS} PASS / ${LIVE_FAIL} FAIL / ${LIVE_SKIP} SKIP"

# --- emulator suite (negatives + time-gated positives, gas-free) ------------
EMU_RC=0
if [[ "${RUN_EMULATOR:-1}" == "1" ]]; then
  step "5. emulator suite (acton test) — negatives + time-gated positives"
  set +e
  ( cd "$TON_DIR" && acton test ) 2>&1 | tee "$LOG_DIR/acton_test.log" | tail -40
  EMU_RC=${PIPESTATUS[0]}
  set -e
  [[ $EMU_RC -eq 0 ]] && ok "emulator suite passed" || warn "emulator suite reported failures (see $LOG_DIR/acton_test.log)"
fi

step "SUMMARY"
printf '%s\n' "Network:        $NET"
printf '%s\n' "Signer:         $WALLET_ADDR"
printf '%s\n' "Winner:         $WINNER_ADDR"
printf '%s\n' "Live checks:    ${LIVE_PASS} PASS / ${LIVE_FAIL} FAIL / ${LIVE_SKIP} SKIP"
[[ "${RUN_EMULATOR:-1}" == "1" ]] && printf '%s\n' "Emulator:       $([[ $EMU_RC -eq 0 ]] && echo PASS || echo FAIL)"
printf '%s\n' "Runner log:     $RUNNER_LOG"
echo
if [[ "$LIVE_FAIL" == "0" && "$EMU_RC" == "0" ]]; then
  printf '%s%s ALL SCENARIOS GREEN %s\n' "$C_BOLD" "$C_GRN" "$C_RESET"; exit 0
else
  printf '%s%s SOME SCENARIOS FAILED %s\n' "$C_BOLD" "$C_RED" "$C_RESET"; exit 1
fi
