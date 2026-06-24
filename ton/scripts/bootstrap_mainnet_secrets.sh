#!/usr/bin/env bash
#
# Bootstrap the Duckton mainnet deployer wallet and store its secrets in Infisical.
#
# Everything happens LOCALLY. The mnemonic is generated on this machine and pushed
# straight into Infisical — it is NEVER printed to the screen. Only the PUBLIC
# address (safe to share / fund) is shown.
#
# PREREQUISITES (run these first):
#   acton --version            # Acton installed: https://ton-blockchain.github.io/acton/docs/installation
#   infisical --version        # Infisical CLI:  https://infisical.com/docs/cli/overview
#   infisical login            # log in as yourself (interactive) — needs write access
#   # .infisical.json at the repo root pins the project (duckton-eu-1w), so no --projectId needed.
#
# USAGE:
#   bash ton/scripts/bootstrap_mainnet_secrets.sh [TREASURY_ADDRESS]
#   # or set GP_FEE_RECIPIENT in the env instead of passing TREASURY_ADDRESS.
#
set -euo pipefail

WALLET="${DUCKTON_WALLET_NAME:-deployer}"
ENVSLUG="${INFISICAL_ENV:-prod}"
TREASURY="${1:-${GP_FEE_RECIPIENT:-}}"

# --- preflight -------------------------------------------------------------
command -v acton >/dev/null 2>&1 || {
  echo "ERROR: 'acton' not found. Install: https://ton-blockchain.github.io/acton/docs/installation" >&2
  exit 1
}
command -v infisical >/dev/null 2>&1 || {
  echo "ERROR: 'infisical' CLI not found. Install: https://infisical.com/docs/cli/overview" >&2
  exit 1
}

# Confirm we can talk to Infisical (project comes from .infisical.json). If this
# fails, run `infisical login` first.
if ! infisical secrets --env="$ENVSLUG" >/dev/null 2>&1; then
  echo "ERROR: cannot read Infisical secrets for env='$ENVSLUG'." >&2
  echo "       Run 'infisical login' first, and make sure .infisical.json is at the repo root." >&2
  exit 1
fi

# --- 1) ensure the wallet exists (tolerate "already exists") ---------------
echo "==> ensuring wallet '$WALLET' exists"
if new_out="$(acton wallet new --name "$WALLET" --version v5r1 2>&1)"; then
  printf '%s\n' "$new_out"
elif printf '%s' "$new_out" | grep -qi "already exists"; then
  echo "    '$WALLET' already exists — reusing it"
else
  printf '%s\n' "$new_out" >&2
  exit 1
fi

# --- 2) push the mnemonic to Infisical WITHOUT printing it -----------------
# The wallet name is a POSITIONAL arg: `acton wallet export-mnemonic [NAME]`.
echo "==> storing mnemonic in Infisical (not shown on screen)"
MNEMONIC="$(acton wallet export-mnemonic "$WALLET")"
infisical secrets set "TON_DEPLOYER_MNEMONIC=$MNEMONIC" --env="$ENVSLUG" >/dev/null
unset MNEMONIC
echo "    stored TON_DEPLOYER_MNEMONIC (env=$ENVSLUG)"

# --- 3) optional treasury / fee recipient ----------------------------------
if [ -n "$TREASURY" ]; then
  infisical secrets set "GP_FEE_RECIPIENT=$TREASURY" --env="$ENVSLUG" >/dev/null
  echo "    stored GP_FEE_RECIPIENT (env=$ENVSLUG)"
else
  echo "    (no treasury passed — set it later with:"
  echo "       infisical secrets set \"GP_FEE_RECIPIENT=<addr>\" --env=$ENVSLUG )"
fi

# --- 4) print the PUBLIC address to fund -----------------------------------
echo
echo "============================================================"
echo " Deployer wallet ready. FUND THIS MAINNET ADDRESS with ~2-3 TON:"
echo "------------------------------------------------------------"
acton wallet list | grep -w "$WALLET" || acton wallet list
echo "------------------------------------------------------------"
echo " After funding, deploy GlobalParams to mainnet with:"
echo "   cd ton && infisical run --env=$ENVSLUG -- \\"
echo "     acton script scripts/deploy_global_params.tolk --net mainnet"
echo "============================================================"
