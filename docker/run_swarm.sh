#!/usr/bin/env bash
# Bring up the HETEROGENEOUS P2P DuckDB grid swarm.
#
#   run_swarm.sh [mem] [cpus]
#
# Role counts default to a host-sane (~16 host + 1 client) heterogeneous topology
# (3 seeds + 8 honest + 2 internal + 2 oom + 1 remote-only). Override any role
# count via env (SEEDS/HONEST/INTERNAL/OOM/REMOTE_ONLY) or pass extra gen_compose
# flags via GEN_ARGS. Output is terse; inspect a service with:
#   docker compose -p p2pgrid logs <service>
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${PROJECT:-p2pgrid}"

MEM="${1:-256m}"
CPUS="${2:-0.4}"
SEEDS="${SEEDS:-3}"
HONEST="${HONEST:-8}"
INTERNAL="${INTERNAL:-2}"
OOM="${OOM:-2}"
REMOTE_ONLY="${REMOTE_ONLY:-1}"

COMPOSE="$HERE/compose.generated.yml"
python3 "$HERE/gen_compose.py" \
  --seeds "$SEEDS" --honest "$HONEST" --internal "$INTERNAL" \
  --oom "$OOM" --remote-only "$REMOTE_ONLY" \
  --mem "$MEM" --cpus "$CPUS" --out "$COMPOSE" ${GEN_ARGS:-}

echo "==> docker compose up -d…"
docker compose -p "$PROJECT" -f "$COMPOSE" up -d --remove-orphans >/tmp/p2pgrid_up.log 2>&1 || {
  echo "compose up FAILED — tail:"; tail -n 20 /tmp/p2pgrid_up.log; exit 1;
}
RUNNING=$(docker ps --filter "label=com.docker.compose.project=${PROJECT}" -q | wc -l | tr -d ' ')
echo "==> running containers: $RUNNING"
