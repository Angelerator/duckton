#!/usr/bin/env bash
# Build the loadable Duckton DuckDB extension and append the metadata footer so it
# can be LOADed by the duckdb CLI.
#
# Usage:  scripts/build_extension.sh [--release]
# Output: <repo>/dist/duckton.duckdb_extension
#
# Then:   duckdb -unsigned -c "LOAD '<repo>/dist/duckton.duckdb_extension'; \
#                              SELECT * FROM p2p_info();"
set -euo pipefail

PROFILE_FLAG=""
PROFILE_DIR="debug"
if [[ "${1:-}" == "--release" ]]; then
  PROFILE_FLAG="--release"
  PROFILE_DIR="release"
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> building cdylib"
cargo build -p p2p-extension $PROFILE_FLAG

DYLIB=""
for cand in \
  "target/$PROFILE_DIR/libduckton.dylib" \
  "target/$PROFILE_DIR/libduckton.so" \
  "target/$PROFILE_DIR/duckton.dll"; do
  [[ -f "$cand" ]] && DYLIB="$cand" && break
done
[[ -n "$DYLIB" ]] || { echo "cdylib not found" >&2; exit 1; }

PLATFORM="$(duckdb -list -noheader -c 'PRAGMA platform;' | tr -d '[:space:]')"
echo "==> platform: $PLATFORM"

mkdir -p dist
python3 scripts/append_extension_metadata.py \
  -l "$DYLIB" \
  -n duckton \
  -p "$PLATFORM" \
  -dv v1.0.0 \
  -ev 0.1.0 \
  -o dist/duckton.duckdb_extension

echo "==> wrote dist/duckton.duckdb_extension"
echo "==> smoke test"
duckdb -unsigned -c "LOAD 'dist/duckton.duckdb_extension'; SELECT * FROM p2p_info();"
