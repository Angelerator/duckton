#!/usr/bin/env bash
# =============================================================================
# testnet_e2e.sh — TURNKEY TON testnet deploy + live end-to-end harness for the
# P2P DuckDB-over-QUIC grid (BLOCKCHAIN_ECONOMICS §6/§7/§8).
#
# Supply a FUNDED testnet wallet mnemonic + (recommended) a Toncenter testnet
# API key, then run this script. It will, against the TON **testnet**:
#
#   0. import your wallet, build the contracts + the DuckDB extension
#   1. run a REAL DuckDB query through the loaded extension and hash the result
#      (this hash becomes the escrow's HTLC lock + the anchored epoch leaf)
#   2. verify the ton_proof two-way wallet<->node binding (pure-Rust crypto)
#   3. deploy the FOUR contracts (StakeVault, RecordAnchor, JobEscrow, GlobalParams)
#   4. `acton verify` the published bytecode against source
#   5. record the deployed addresses into a generated config file
#   6. run the live scenario: GlobalParams admin update / non-admin rejection /
#      governance blocklist -> stake deposit -> 1:1 transfer-locked receipt +
#      Duckton TEP-64 metadata -> wallet-bind anchor -> open escrow -> settle
#      (winner + platform fee + participation commissions) -> anchor epoch root +
#      single- and multi-leaf inclusion proofs -> optional bonded dispute
#   7. print a PASS/FAIL summary with testnet.tonviewer.com explorer links
#   8. (optional) run the `ton-live`-gated Rust integration test against the RPC
#
# NOTHING here runs unless you provide the env vars below — it fails fast with a
# clear message if a required input is missing. See docs/TESTNET.md for the full
# runbook (Acton install, wallet creation, faucet, API key, troubleshooting).
#
# -----------------------------------------------------------------------------
# REQUIRED env (export, or put in a sourced config file passed via --config):
#   TON_TESTNET_MNEMONIC        24-word wallet mnemonic (space-separated), OR
#   TON_TESTNET_MNEMONIC_FILE   path to a file containing the mnemonic
#
# RECOMMENDED env:
#   TON_TESTNET_API_KEY         Toncenter testnet API key (@tonapibot / t.me/toncenter)
#                               -> exported to Acton as TONCENTER_TESTNET_API_KEY.
#                               Without it Toncenter is throttled to ~1 RPS; pass
#                               ALLOW_NO_API_KEY=1 to proceed keyless anyway.
#
# OPTIONAL env (sensible defaults shown):
#   TON_TESTNET_RPC=https://testnet.toncenter.com/api/v2   (used by the Rust live test)
#   WALLET_NAME=deployer        Acton wallet name the Tolk scripts resolve
#   WALLET_VERSION=v5r1         wallet contract version
#   OUT_DIR=ton/deployments     where the generated config/addresses are written
#   NODE_ID=b3:demo-node        node id folded into the on-chain binding hash
#   STAKE_DEPOSIT_AMOUNT=100000000        nanoton to bond (0.1 TON)
#   ESCROW_AMOUNT=300000000               nanoton locked in escrow (0.3 TON)
#   ESCROW_WINDOW_SECS=3600               refund-on-timeout window
#   SETTLE_WINNER_AMOUNT=100000000        winner payout (0.1 TON)
#   SETTLE_FEE=20000000                   platform fee (0.02 TON)
#   SETTLE_PARTICIPANT_AMOUNT=20000000    participation commission (0.02 TON)
#   E2E_RUN_DISPUTE=0           set 1 to also run the bonded dispute round
#   SKIP_EXTENSION_BUILD=0      set 1 to reuse an existing dist/ extension
#   SKIP_VERIFY=0               set 1 to skip `acton verify`
#   RUN_RUST_LIVE_TEST=0        set 1 to run the ton-live-gated Rust test at the end
#   DUCKDB_QUERY_FILE=...       override the SQL run through the extension
#
# Amounts are nanoton (1 TON = 1e9). Defaults are tiny so a testnet wallet with a
# couple of test-GRAM can run the whole loop; payouts default to your OWN wallet
# (winner = treasury = participant = deployer) so funds cycle back.
#
# This script is re-runnable: the wallet import is idempotent, the StakeVault /
# RecordAnchor have deterministic addresses (re-deploy is a harmless top-up), the
# anchor epoch is read from chain and advanced by one each run, and each run
# opens a FRESH escrow (its address varies with the per-run deadline) so settle
# always targets an unsettled escrow.
# =============================================================================
set -euo pipefail

# ----------------------------------------------------------------------------
# Pretty logging
# ----------------------------------------------------------------------------
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

# ----------------------------------------------------------------------------
# Parse args (only --config so far) and locate the repo
# ----------------------------------------------------------------------------
CONFIG_FILE="${CONFIG_FILE:-}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) CONFIG_FILE="${2:-}"; shift 2 ;;
    --config=*) CONFIG_FILE="${1#*=}"; shift ;;
    -h|--help) sed -n '2,70p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TON_DIR="$REPO_ROOT/ton"
cd "$REPO_ROOT"

# A sourced config file is the convenient way to keep secrets out of the shell
# history. scripts/testnet_e2e.env is auto-loaded if present.
if [[ -z "$CONFIG_FILE" && -f "$REPO_ROOT/scripts/testnet_e2e.env" ]]; then
  CONFIG_FILE="$REPO_ROOT/scripts/testnet_e2e.env"
fi
if [[ -n "$CONFIG_FILE" ]]; then
  [[ -f "$CONFIG_FILE" ]] || die "config file not found: $CONFIG_FILE"
  log "sourcing config: $CONFIG_FILE"
  # shellcheck disable=SC1090
  set -a; . "$CONFIG_FILE"; set +a
fi

# ----------------------------------------------------------------------------
# Resolve + validate inputs (FAIL FAST with actionable messages)
# ----------------------------------------------------------------------------
NET="${NET:-testnet}"
WALLET_NAME="${WALLET_NAME:-deployer}"
WALLET_VERSION="${WALLET_VERSION:-v5r1}"
OUT_DIR_REL="${OUT_DIR:-ton/deployments}"
OUT_DIR="$REPO_ROOT/$OUT_DIR_REL"
TON_TESTNET_RPC="${TON_TESTNET_RPC:-https://testnet.toncenter.com/api/v2}"
NODE_ID="${NODE_ID:-b3:demo-node}"
EXPLORER_BASE="https://testnet.tonviewer.com"

STAKE_DEPOSIT_AMOUNT="${STAKE_DEPOSIT_AMOUNT:-100000000}"     # 0.1 TON
ESCROW_AMOUNT="${ESCROW_AMOUNT:-300000000}"                   # 0.3 TON
ESCROW_WINDOW_SECS="${ESCROW_WINDOW_SECS:-3600}"
SETTLE_WINNER_AMOUNT="${SETTLE_WINNER_AMOUNT:-100000000}"     # 0.1 TON
SETTLE_FEE="${SETTLE_FEE:-20000000}"                         # 0.02 TON
SETTLE_PARTICIPANT_AMOUNT="${SETTLE_PARTICIPANT_AMOUNT:-20000000}"  # 0.02 TON
E2E_RUN_DISPUTE="${E2E_RUN_DISPUTE:-0}"
SKIP_EXTENSION_BUILD="${SKIP_EXTENSION_BUILD:-0}"
SKIP_VERIFY="${SKIP_VERIFY:-0}"
RUN_RUST_LIVE_TEST="${RUN_RUST_LIVE_TEST:-0}"

# tools
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1 ($2)"; }
need acton  "install: https://ton-blockchain.github.io/acton/docs/installation"
need duckdb "install the DuckDB CLI: https://duckdb.org/docs/installation"
need curl   "curl is required for explorer/RPC checks"
need jq      "jq is required to parse Acton JSON output"
need cargo  "the Rust toolchain is required"
if command -v sha256sum >/dev/null 2>&1; then SHA256() { sha256sum | awk '{print $1}'; }
elif command -v shasum  >/dev/null 2>&1; then SHA256() { shasum -a 256 | awk '{print $1}'; }
else die "need sha256sum or shasum"; fi

# mnemonic
if [[ -z "${TON_TESTNET_MNEMONIC:-}" && -n "${TON_TESTNET_MNEMONIC_FILE:-}" ]]; then
  [[ -f "$TON_TESTNET_MNEMONIC_FILE" ]] || die "TON_TESTNET_MNEMONIC_FILE not found: $TON_TESTNET_MNEMONIC_FILE"
  TON_TESTNET_MNEMONIC="$(tr -s '[:space:]' ' ' < "$TON_TESTNET_MNEMONIC_FILE" | sed 's/^ *//;s/ *$//')"
fi
[[ -n "${TON_TESTNET_MNEMONIC:-}" ]] || die "set TON_TESTNET_MNEMONIC (24 words) or TON_TESTNET_MNEMONIC_FILE — see docs/TESTNET.md"
# shellcheck disable=SC2206
_WORDS=( $TON_TESTNET_MNEMONIC )
[[ ${#_WORDS[@]} -eq 24 || ${#_WORDS[@]} -eq 12 ]] || die "mnemonic should be 12 or 24 words, got ${#_WORDS[@]}"

# API key (recommended). Acton + the Rust live test read the Toncenter testnet key.
if [[ -n "${TON_TESTNET_API_KEY:-}" ]]; then
  export TONCENTER_TESTNET_API_KEY="$TON_TESTNET_API_KEY"
elif [[ -n "${TONCENTER_TESTNET_API_KEY:-}" ]]; then
  TON_TESTNET_API_KEY="$TONCENTER_TESTNET_API_KEY"
elif [[ "${ALLOW_NO_API_KEY:-0}" != "1" ]]; then
  die "set TON_TESTNET_API_KEY (Toncenter testnet key from t.me/toncenter), or pass ALLOW_NO_API_KEY=1 to run keyless (throttled to ~1 RPS)"
else
  warn "no Toncenter API key — Toncenter is throttled to ~1 RPS; runs will be slow"
fi

# Acton needs the macOS SDK path for its execution engine.
if command -v xcrun >/dev/null 2>&1 && [[ -z "${SDKROOT:-}" ]]; then
  export SDKROOT="$(xcrun --show-sdk-path)"
fi

mkdir -p "$OUT_DIR"
GEN_ENV="$OUT_DIR/$NET.env"
GEN_TOML="$OUT_DIR/economics.$NET.toml"
LOG_DIR="$OUT_DIR/logs"
mkdir -p "$LOG_DIR"

# Track step results for the final summary.
declare -a SUMMARY=()
record() { SUMMARY+=("$1"); }
FAILED=0
checkmark() { # checkmark "<label>" <0|1 pass>
  if [[ "$2" == "1" ]]; then record "${C_GRN}PASS${C_RESET}  $1"; else record "${C_RED}FAIL${C_RESET}  $1"; FAILED=1; fi
}

# ----------------------------------------------------------------------------
# 0. Import wallet (idempotent) + resolve its address
# ----------------------------------------------------------------------------
step "0. wallet"
if acton wallet list --json 2>/dev/null | jq -e --arg n "$WALLET_NAME" '.wallets[]?|select(.name==$n)' >/dev/null; then
  ok "wallet '$WALLET_NAME' already configured (reusing)"
else
  log "importing wallet '$WALLET_NAME' ($WALLET_VERSION) from mnemonic"
  # Mnemonic words are passed as positional args; not echoed.
  acton wallet import --name "$WALLET_NAME" --local --version "$WALLET_VERSION" "${_WORDS[@]}" >/dev/null \
    || die "wallet import failed (check the mnemonic + version)"
  ok "wallet imported"
fi
WALLET_ADDR="$(acton wallet list --json 2>/dev/null | jq -r --arg n "$WALLET_NAME" '.wallets[]?|select(.name==$n)|.address' | head -1)"
[[ -n "$WALLET_ADDR" && "$WALLET_ADDR" != "null" ]] || die "could not resolve wallet address for '$WALLET_NAME'"
ok "wallet address: $WALLET_ADDR"
log "balance:"; acton wallet list --balance 2>/dev/null | sed 's/^/    /' || warn "balance lookup failed (continuing)"

# ----------------------------------------------------------------------------
# 1. Build contracts
# ----------------------------------------------------------------------------
step "1. acton build"
( cd "$TON_DIR" && acton build ) || die "acton build failed"
ok "contracts compiled"

# ----------------------------------------------------------------------------
# 2. Real DuckDB query through the loaded extension -> result hash
# ----------------------------------------------------------------------------
step "2. DuckDB query through the extension"
EXT="$REPO_ROOT/dist/duckdb_p2p.duckdb_extension"
if [[ "$SKIP_EXTENSION_BUILD" != "1" || ! -f "$EXT" ]]; then
  log "building the loadable extension (scripts/build_extension.sh)"
  bash "$REPO_ROOT/scripts/build_extension.sh" >/dev/null 2>&1 || die "extension build failed (try: SDKROOT=\$(xcrun --show-sdk-path) scripts/build_extension.sh)"
fi
[[ -f "$EXT" ]] || die "extension not found at $EXT"

QUERY_SQL="${DUCKDB_QUERY_FILE:-}"
if [[ -z "$QUERY_SQL" ]]; then
  QUERY_SQL="$(mktemp -t p2p_e2e_query.XXXXXX.sql)"
  trap 'rm -f "$QUERY_SQL"' EXIT
  cat > "$QUERY_SQL" <<SQL
-- A real, deterministic DuckDB computation, plus the extension's own metadata
-- table function (proves the loaded extension participated in the query).
LOAD '$EXT';
SELECT count(*) AS n, sum(i) AS s, avg(i) AS a FROM range(100000) t(i);
SELECT key, value FROM p2p_info() ORDER BY key;
SQL
fi
log "query: $QUERY_SQL"
QUERY_OUT="$(duckdb -unsigned -noheader -list < "$QUERY_SQL")" || die "DuckDB query failed (is the extension built for this platform?)"
printf '%s\n' "$QUERY_OUT" | sed 's/^/    /'
RESULT_HEX="$(printf '%s' "$QUERY_OUT" | SHA256)"
RESULT_HASH="0x$RESULT_HEX"
ok "query result hash (HTLC lock / epoch leaf): $RESULT_HASH"

# ----------------------------------------------------------------------------
# 3. ton_proof two-way wallet<->node binding (pure-Rust crypto verification)
# ----------------------------------------------------------------------------
step "3. ton_proof wallet<->node binding"
log "verifying the two-way ton_proof binding crypto (p2p-settlement::ton_proof)"
if cargo test -p p2p-settlement ton_proof --quiet >/dev/null 2>&1; then
  ok "ton_proof two-way binding verified (Ed25519 + sha256, both directions)"
  BINDING_OK=1
else
  warn "ton_proof unit verification failed"
  BINDING_OK=0
fi
# Anchor an opaque on-chain binding hash for this wallet<->node pair in the vault.
BINDING_HEX="$(printf 'duckdb-p2p-wallet-bind-v1|%s|%s' "$NODE_ID" "$WALLET_ADDR" | SHA256)"
BINDING_HASH="0x$BINDING_HEX"
log "on-chain binding hash (anchored in StakeVault.bindingHash): $BINDING_HASH"

# ----------------------------------------------------------------------------
# 4. Deploy the three contracts
# ----------------------------------------------------------------------------
# Helper: run an acton script on testnet, capture the log, return the address it
# deployed (parsed from the stable "Deployed <Contract> to <addr> (...)" line).
deploy_addr() {  # deploy_addr <Contract> <script.tolk> <logfile>
  local contract="$1" script="$2" logf="$3"
  # Stream the deploy output to the log AND the terminal (stderr) for live
  # progress, but keep this function's STDOUT clean so the command-substitution
  # caller captures ONLY the parsed address (not the whole multiline trace log).
  ( cd "$TON_DIR" && acton script "$script" --net "$NET" --explorer tonviewer ) 2>&1 | tee "$logf" >&2
  awk -v c="$contract" '$0 ~ ("^Deployed " c " to ") {print $4; exit}' "$logf"
}

step "4. deploy contracts to $NET"
# 4a. StakeVault (also the receipt-jetton master). bindingHash is set at deploy.
log "deploying StakeVault ..."
VAULT_ADDR="$( STAKE_MIN="${STAKE_MIN:-0}" STAKE_BINDING_HASH="$BINDING_HASH" \
  deploy_addr StakeVault scripts/deploy_stake.tolk "$LOG_DIR/deploy_stake.log" )"
[[ -n "$VAULT_ADDR" ]] || die "failed to parse StakeVault address (see $LOG_DIR/deploy_stake.log)"
ok "StakeVault: $VAULT_ADDR"

# 4b. RecordAnchor. Low dispute bond so the optional dispute round is cheap.
log "deploying RecordAnchor ..."
ANCHOR_ADDR="$( ANCHOR_BOND_MIN="${ANCHOR_BOND_MIN:-100000000}" \
  deploy_addr RecordAnchor scripts/deploy_anchor.tolk "$LOG_DIR/deploy_anchor.log" )"
[[ -n "$ANCHOR_ADDR" ]] || die "failed to parse RecordAnchor address (see $LOG_DIR/deploy_anchor.log)"
ok "RecordAnchor: $ANCHOR_ADDR"

# 4c. JobEscrow — opened (deployed) WITH the locked B, HTLC-locked on the query
#     result hash. A fresh deadline each run yields a fresh, unsettled escrow.
ESCROW_DEADLINE=$(( $(date +%s) + ESCROW_WINDOW_SECS ))
log "deploying JobEscrow (B=$ESCROW_AMOUNT nanoton, deadline=$ESCROW_DEADLINE, lock=$RESULT_HASH) ..."
ESCROW_ADDR="$( ESCROW_AMOUNT="$ESCROW_AMOUNT" ESCROW_DEADLINE="$ESCROW_DEADLINE" \
  ESCROW_EXPECTED_HASH="$RESULT_HASH" ESCROW_TREASURY="$WALLET_ADDR" \
  deploy_addr JobEscrow scripts/deploy_escrow.tolk "$LOG_DIR/deploy_escrow.log" )"
[[ -n "$ESCROW_ADDR" ]] || die "failed to parse JobEscrow address (see $LOG_DIR/deploy_escrow.log)"
ok "JobEscrow: $ESCROW_ADDR"

# 4d. GlobalParams — the platform-wide economic-parameter contract (§12). The
#     admin is the deployer wallet (so the live scenario can update params in
#     place); its address is STABLE across updates. §12 defaults are applied
#     unless overridden via GP_* env vars.
log "deploying GlobalParams ..."
GLOBAL_PARAMS_ADDR="$( GP_FEE_RECIPIENT="$WALLET_ADDR" \
  deploy_addr GlobalParams scripts/deploy_global_params.tolk "$LOG_DIR/deploy_global_params.log" )"
[[ -n "$GLOBAL_PARAMS_ADDR" ]] || die "failed to parse GlobalParams address (see $LOG_DIR/deploy_global_params.log)"
ok "GlobalParams: $GLOBAL_PARAMS_ADDR"

# ----------------------------------------------------------------------------
# 5. Record the deployed addresses into generated config
# ----------------------------------------------------------------------------
step "5. record deployment config"
cat > "$GEN_ENV" <<ENV
# Generated by scripts/testnet_e2e.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Source this file to re-run the scenario or the ton-live Rust test:
#   set -a; . $OUT_DIR_REL/$NET.env; set +a
export TON_NETWORK="$NET"
export TON_TESTNET_RPC="$TON_TESTNET_RPC"
export TON_TESTNET_WALLET="$WALLET_ADDR"
export TON_TESTNET_VAULT_ADDR="$VAULT_ADDR"
export TON_TESTNET_ANCHOR_ADDR="$ANCHOR_ADDR"
export TON_TESTNET_ESCROW_ADDR="$ESCROW_ADDR"
export TON_TESTNET_GLOBAL_PARAMS_ADDR="$GLOBAL_PARAMS_ADDR"
export TON_TESTNET_RESULT_HASH="$RESULT_HASH"
export TON_TESTNET_BINDING_HASH="$BINDING_HASH"
ENV
ok "wrote $GEN_ENV"

cat > "$GEN_TOML" <<TOML
# Generated by scripts/testnet_e2e.sh — node [economics] snippet for the live
# testnet. Point your node config at it via P2P_CONFIG, or merge the keys.
#
# crates/config's [economics.<net>.contracts] block is now wired (the live TON
# client reads it via p2p_settlement::resolve_ton_wiring + economics.guard_mainnet),
# so the deployed addresses are recorded directly below for the active network.
[economics]
enabled         = true
settlement      = "onchain"
custody         = "noncustodial"
accounting_unit = "ton"
chain           = "ton"
network         = "$NET"
default_payment = "paid"
fee_recipient   = "$WALLET_ADDR"

[economics.$NET.contracts]
stake_vault   = "$VAULT_ADDR"
job_escrow    = "$ESCROW_ADDR"
record_anchor = "$ANCHOR_ADDR"
global_params = "$GLOBAL_PARAMS_ADDR"
TOML
ok "wrote $GEN_TOML"

# ----------------------------------------------------------------------------
# 6. Verify published bytecode against source
# ----------------------------------------------------------------------------
step "6. acton verify"
verify_one() {  # verify_one <Contract> <addr>
  local c="$1" a="$2"
  if ( cd "$TON_DIR" && acton verify "$c" --address "$a" --net "$NET" ) >"$LOG_DIR/verify_$c.log" 2>&1; then
    ok "$c verified"; checkmark "verify $c" 1
  else
    warn "$c verify did not complete (see $LOG_DIR/verify_$c.log) — may be a transient verifier-backend issue"
    checkmark "verify $c" 0
  fi
}
if [[ "$SKIP_VERIFY" == "1" ]]; then
  warn "SKIP_VERIFY=1 — skipping acton verify"
else
  verify_one StakeVault "$VAULT_ADDR"
  verify_one RecordAnchor "$ANCHOR_ADDR"
  verify_one JobEscrow "$ESCROW_ADDR"
  verify_one GlobalParams "$GLOBAL_PARAMS_ADDR"
fi

# ----------------------------------------------------------------------------
# 7. Live end-to-end scenario
# ----------------------------------------------------------------------------
step "7. live end-to-end scenario"
E2E_LOG="$LOG_DIR/e2e.log"
set +e
( cd "$TON_DIR" && \
  VAULT_ADDR="$VAULT_ADDR" ESCROW_ADDR="$ESCROW_ADDR" ANCHOR_ADDR="$ANCHOR_ADDR" \
  GLOBAL_PARAMS_ADDR="$GLOBAL_PARAMS_ADDR" \
  STAKE_DEPOSIT_AMOUNT="$STAKE_DEPOSIT_AMOUNT" \
  SETTLE_WINNER="$WALLET_ADDR" SETTLE_WINNER_AMOUNT="$SETTLE_WINNER_AMOUNT" \
  SETTLE_FEE="$SETTLE_FEE" SETTLE_PARTICIPANT="$WALLET_ADDR" \
  SETTLE_PARTICIPANT_AMOUNT="$SETTLE_PARTICIPANT_AMOUNT" SETTLE_RESULT_HASH="$RESULT_HASH" \
  ANCHOR_ROOT="$RESULT_HASH" ANCHOR_LEAF="$RESULT_HASH" \
  E2E_RUN_DISPUTE="$([[ "$E2E_RUN_DISPUTE" == "1" ]] && echo true || echo false)" \
  DISPUTE_BOND="${DISPUTE_BOND:-200000000}" \
  acton script scripts/e2e_testnet.tolk --net "$NET" --explorer tonviewer ) 2>&1 | tee "$E2E_LOG"
E2E_RC=${PIPESTATUS[0]}
set -e
[[ $E2E_RC -eq 0 ]] || warn "e2e scenario exited non-zero ($E2E_RC) — inspecting partial checks"

# Parse the ::CHECK:: markers the scenario prints.
STAKED_LINE="$(grep -oE '::CHECK::stake_deposit::staked=[0-9]+' "$E2E_LOG" | tail -1 || true)"
STAKED_VAL="${STAKED_LINE##*=}"
[[ -n "$STAKED_VAL" && "$STAKED_VAL" -gt 0 ]] && checkmark "stake deposit (staked=$STAKED_VAL)" 1 || checkmark "stake deposit" 0

# Stake-receipt jetton: minted 1:1 with the bond + transfer-locked (anti-exit).
JETTON="$(grep -oE '::CHECK::jetton::minted=[0-9]+::staked=[0-9]+::locked=(true|false)' "$E2E_LOG" | tail -1 || true)"
J_MINTED="$(printf '%s' "$JETTON" | sed -nE 's/.*minted=([0-9]+).*/\1/p')"
J_STAKED="$(printf '%s' "$JETTON" | sed -nE 's/.*staked=([0-9]+).*/\1/p')"
if [[ -n "$J_MINTED" && -n "$J_STAKED" && "$J_MINTED" == "$J_STAKED" && "$J_MINTED" -gt 0 && "$JETTON" == *"locked=true" ]]; then
  checkmark "stake-receipt jetton minted 1:1 + transfer-locked (minted=$J_MINTED)" 1
else
  checkmark "stake-receipt jetton minted 1:1 + transfer-locked" 0
fi

# A transfer attempt must be rejected (balance unchanged afterwards).
XFER="$(grep -oE '::CHECK::jetton_transfer::before=[0-9]+::after=[0-9]+::rejected=(true|false)' "$E2E_LOG" | tail -1 || true)"
[[ "$XFER" == *"rejected=true" ]] && checkmark "stake-receipt jetton transfer rejected (anti-exit)" 1 || checkmark "stake-receipt jetton transfer rejected" 0

SETTLED="$(grep -oE '::CHECK::escrow_settle::settled=(true|false)' "$E2E_LOG" | tail -1 || true)"
[[ "$SETTLED" == *"=true" ]] && checkmark "escrow settle (HTLC release)" 1 || checkmark "escrow settle" 0

ANCHORED="$(grep -oE '::CHECK::anchor_submit::epoch=[0-9]+' "$E2E_LOG" | tail -1 || true)"
[[ -n "$ANCHORED" ]] && checkmark "anchor epoch root (${ANCHORED##*::})" 1 || checkmark "anchor epoch root" 0

INCL="$(grep -oE '::CHECK::inclusion::verified=(true|false)' "$E2E_LOG" | tail -1 || true)"
[[ "$INCL" == *"=true" ]] && checkmark "inclusion proof verified on-chain" 1 || checkmark "inclusion proof" 0

# Newer scenarios emit an explicit ::CHECK::<name>::PASS|FAIL token: GlobalParams
# (admin update / non-admin rejection / governance blocklist), the Duckton TEP-64
# metadata on the freshly redeployed StakeVault, and the multi-leaf Merkle
# inclusion anchored + verified on-chain.
check_passfail() {  # check_passfail <marker_name> <summary_label>
  local marker="$1" label="$2" line
  line="$(grep -oE "::CHECK::${marker}::(PASS|FAIL)" "$E2E_LOG" | tail -1 || true)"
  if [[ "$line" == *"::PASS" ]]; then checkmark "$label" 1; else checkmark "$label" 0; fi
}
check_passfail globalparams_update    "GlobalParams admin update_params (persisted, address stable)"
check_passfail globalparams_nonadmin  "GlobalParams non-admin update_params rejected"
check_passfail globalparams_blocklist "GlobalParams governance blocklist round-trip"
check_passfail duckton_metadata       "Duckton TEP-64 metadata (name/symbol/decimals)"
check_passfail multileaf_inclusion    "multi-leaf Merkle inclusion verified on-chain"

if [[ "$E2E_RUN_DISPUTE" == "1" ]]; then
  DISP="$(grep -oE '::CHECK::dispute::id=[0-9]+::status=[0-9]+' "$E2E_LOG" | tail -1 || true)"
  [[ "$DISP" == *"status=1" ]] && checkmark "dispute upheld (${DISP##*::})" 1 || checkmark "dispute round" 0
fi

# On-chain binding-hash confirmation via the live RPC (read get_binding_hash).
step "   on-chain binding-hash confirmation (live RPC)"
RPC_VAULT_BIND="$(curl -s --max-time 30 \
  "${TON_TESTNET_RPC%/}/runGetMethod" \
  -H 'Content-Type: application/json' \
  ${TON_TESTNET_API_KEY:+-H "X-API-Key: $TON_TESTNET_API_KEY"} \
  -d "{\"address\":\"$VAULT_ADDR\",\"method\":\"get_binding_hash\",\"stack\":[]}" 2>/dev/null \
  | jq -r '.result.stack[0][1] // empty' 2>/dev/null || true)"
if [[ -n "$RPC_VAULT_BIND" ]]; then
  # Normalize both to lowercase hex without 0x for comparison.
  norm() { tr 'A-Z' 'a-z' | sed 's/^0x0*//; s/^0x//'; }
  if [[ "$(printf '%s' "$RPC_VAULT_BIND" | norm)" == "$(printf '%s' "$BINDING_HASH" | norm)" ]]; then
    ok "vault bindingHash on-chain matches the wallet<->node binding"
    checkmark "wallet-bind anchored on-chain" 1
  else
    warn "on-chain bindingHash ($RPC_VAULT_BIND) != expected ($BINDING_HASH)"
    checkmark "wallet-bind anchored on-chain" 0
  fi
else
  warn "could not read get_binding_hash via RPC (continuing) — check TON_TESTNET_RPC/API key"
fi

# ----------------------------------------------------------------------------
# 8. Optional: ton-live-gated Rust integration test against the live RPC
# ----------------------------------------------------------------------------
if [[ "$RUN_RUST_LIVE_TEST" == "1" ]]; then
  step "8. ton-live Rust integration test"
  set +e
  TON_TESTNET_RPC="$TON_TESTNET_RPC" TON_TESTNET_API_KEY="${TON_TESTNET_API_KEY:-}" \
    TON_TESTNET_VAULT_ADDR="$VAULT_ADDR" TON_TESTNET_ANCHOR_ADDR="$ANCHOR_ADDR" \
    TON_TESTNET_ESCROW_ADDR="$ESCROW_ADDR" \
    cargo test -p p2p-settlement --features ton-live --test testnet_live -- --nocapture 2>&1 | tee "$LOG_DIR/rust_live.log"
  RC=${PIPESTATUS[0]}
  set -e
  [[ $RC -eq 0 ]] && checkmark "ton-live Rust test" 1 || checkmark "ton-live Rust test" 0
fi

# ----------------------------------------------------------------------------
# Summary
# ----------------------------------------------------------------------------
checkmark "ton_proof binding crypto" "$BINDING_OK"
step "SUMMARY"
printf '%s\n' "Network:      $NET"
printf '%s\n' "Wallet:       $WALLET_ADDR"
printf '%s\n' "StakeVault:   $VAULT_ADDR"
printf '%s\n' "                $EXPLORER_BASE/$VAULT_ADDR"
printf '%s\n' "RecordAnchor: $ANCHOR_ADDR"
printf '%s\n' "                $EXPLORER_BASE/$ANCHOR_ADDR"
printf '%s\n' "JobEscrow:    $ESCROW_ADDR"
printf '%s\n' "                $EXPLORER_BASE/$ESCROW_ADDR"
printf '%s\n' "GlobalParams: $GLOBAL_PARAMS_ADDR"
printf '%s\n' "                $EXPLORER_BASE/$GLOBAL_PARAMS_ADDR"
printf '%s\n' "Result hash:  $RESULT_HASH"
printf '%s\n' "Config:       $GEN_ENV"
printf '%s\n' "              $GEN_TOML"
echo
printf '%s\n' "Checks:"
for line in "${SUMMARY[@]}"; do printf '  %s\n' "$line"; done
echo
if [[ "$FAILED" == "0" ]]; then
  printf '%s%s ALL CHECKS PASSED %s\n' "$C_BOLD" "$C_GRN" "$C_RESET"
  exit 0
else
  printf '%s%s SOME CHECKS FAILED %s (see logs in %s)\n' "$C_BOLD" "$C_RED" "$C_RESET" "$LOG_DIR"
  exit 1
fi
