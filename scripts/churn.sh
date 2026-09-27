#!/usr/bin/env bash
# Trigger frequent updates by restarting Deployments in batches.
# Usage: churn.sh [namespace] [rounds]
# Defaults: perf namespace and 50 rounds.
set -euo pipefail
ns="${1:-perf}"
rounds="${2:-50}"
for r in $(seq 1 "$rounds"); do
    kubectl -n "$ns" patch deploy --all --type merge \
        -p "{\"spec\":{\"template\":{\"metadata\":{\"annotations\":{\"churn\":\"$(date +%s)-$r\"}}}}}" >/dev/null
    echo "Round $r"
done
