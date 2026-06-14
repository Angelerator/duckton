#!/usr/bin/env bash
# Bring up the P2P DuckDB grid swarm.
#
#   run_swarm.sh <nodes> <seeds> [mem] [cpus]
#
# Generates the compose file then `docker compose up -d`. Output is terse; the
# full compose log is NOT streamed (100 containers => huge). Inspect later with:
#   docker compose -p p2pgrid logs <service>
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${PROJECT:-p2pgrid}"

NODES="${1:-100}"
SEEDS="${2:-3}"
MEM="${3:-110m}"
CPUS="${4:-0.08}"

COMPOSE="$HERE/compose.generated.yml"
python3 "$HERE/gen_compose.py" --nodes "$NODES" --seeds "$SEEDS" --mem "$MEM" --cpus "$CPUS" --out "$COMPOSE"

echo "==> docker compose up -d ($NODES nodes)…"
docker compose -p "$PROJECT" -f "$COMPOSE" up -d --remove-orphans >/tmp/p2pgrid_up.log 2>&1 || {
  echo "compose up FAILED — tail:"; tail -n 20 /tmp/p2pgrid_up.log; exit 1;
}
RUNNING=$(docker ps --filter "label=com.docker.compose.project=${PROJECT}" -q | wc -l | tr -d ' ')
echo "==> requested $NODES, running containers: $RUNNING"
