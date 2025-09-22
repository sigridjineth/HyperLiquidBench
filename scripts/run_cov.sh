#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'USAGE'
Usage: scripts/run_cov.sh [PLAN_SPEC] [-- <hl-runner args>]

Environment variables:
  OUT_DIR   Override run output directory (default: runs/<timestamp>)
  NETWORK   Override --network flag (default: testnet)

Examples:
  scripts/run_cov.sh dataset/tasks/hl_perp_basic_01.jsonl:1
  OUT_DIR=runs/demo NETWORK=mainnet scripts/run_cov.sh dataset/tasks/plan.json -- --builder-code demo123
USAGE
  exit 0
fi

PLAN_SPEC=${1:-dataset/tasks/hl_perp_basic_01.jsonl:1}
if [[ $# -gt 0 ]]; then
  shift
fi

OUT_DIR=${OUT_DIR:-"runs/$(date +%Y%m%d-%H%M%S)"}
NETWORK=${NETWORK:-testnet}

cargo run -p hl-runner -- \
  --plan "$PLAN_SPEC" \
  --out "$OUT_DIR" \
  --network "$NETWORK" \
  "$@"
