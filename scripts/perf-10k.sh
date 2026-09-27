#!/usr/bin/env bash
# Create a 10,000-Pod performance test environment.
# The script creates a large kind cluster, seeds Pods, waits until they are Running, and prints the next step.
#
# Usage:
#   scripts/perf-10k.sh [--name k8s-gpui-big] [--nodes 3] [--max-pods 5000]
#                       [--pods 10000] [--per 50] [--ns perf] [--timeout 30m]
#                       [--kubeconfig /tmp/opencode/perf-10k/kubeconfig-big]
#
# Notes:
#   - The script restores the default kubeconfig current-context after kind create.
#   - The generated kubeconfig contains only the target cluster. Set KUBECONFIG for seeding, tests, and the app.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
name="k8s-gpui-big"
nodes=3
max_pods=5000
pods=10000
per=50
ns=perf
timeout="30m"
kubeconfig="/tmp/opencode/perf-10k/kubeconfig-$name"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --name) name="${2:?Missing value for --name}"; shift 2 ;;
        --nodes) nodes="${2:?Missing value for --nodes}"; shift 2 ;;
        --max-pods) max_pods="${2:?Missing value for --max-pods}"; shift 2 ;;
        --pods) pods="${2:?Missing value for --pods}"; shift 2 ;;
        --per) per="${2:?Missing value for --per}"; shift 2 ;;
        --ns) ns="${2:?Missing value for --ns}"; shift 2 ;;
        --timeout) timeout="${2:?Missing value for --timeout}"; shift 2 ;;
        --kubeconfig) kubeconfig="${2:?Missing value for --kubeconfig}"; shift 2 ;;
        -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

context="kind-$name"
prev_context="$(kubectl config current-context 2>/dev/null || true)"

timeout_to_seconds() {
    local value="$1" unit="${1: -1}" number="${1%?}"
    case "$unit" in
        s) echo "$number" ;;
        m) echo $(( number * 60 )) ;;
        h) echo $(( number * 3600 )) ;;
        *) echo "$value" ;;
    esac
}

"$here/kind-up.sh" --name "$name" --nodes "$nodes" --max-pods "$max_pods"

# Create a kubeconfig with only the target cluster. The app reads it through ClusterRegistry::load_default.
mkdir -p "$(dirname "$kubeconfig")"
kubectl config view --raw --minify --context "$context" > "$kubeconfig"
echo "Kubeconfig: $kubeconfig (current-context=$(kubectl --kubeconfig "$kubeconfig" config current-context))"

# Restore the global current-context changed by kind create.
if [[ -n "$prev_context" && "$prev_context" != "$context" ]]; then
    kubectl config use-context "$prev_context" >/dev/null
    echo "Restored default kubeconfig current-context = $prev_context"
fi

export KUBECONFIG="$kubeconfig"
existing="$(kubectl get pods -n "$ns" --no-headers 2>/dev/null | wc -l || true)"
if (( existing >= pods )); then
    echo "ns/$ns already has $existing Pods. Skip seed."
else
    "$here/seed-pods.sh" "$pods" "$per" "$ns"
fi

echo "Wait for $pods Pods to reach Running (timeout $timeout)..."
deadline=$(( $(date +%s) + $(timeout_to_seconds "$timeout") ))
while :; do
    running="$(kubectl get pods -n "$ns" --field-selector=status.phase=Running --no-headers 2>/dev/null | wc -l)"
    total="$(kubectl get pods -n "$ns" --no-headers 2>/dev/null | wc -l)"
    printf '\r  Running %s / %s' "$running" "$total"
    (( running >= pods )) && break
    if (( $(date +%s) >= deadline )); then
        echo
        echo "Timed out: only $running of $pods Pods reached Running. Remaining Pod states:"
        kubectl get pods -n "$ns" --no-headers | awk '{print $3}' | sort | uniq -c
        exit 1
    fi
    sleep 5
done
echo
echo "10,000-Pod performance test is ready. Next steps:"
echo "  cargo build --release -p k8s-app   # Required for the test. Debug builds are about eight times slower."
echo "  ./scripts/perf-measure.sh --context $context --kubeconfig $kubeconfig"
echo "  # Or run manually: KUBECONFIG=$kubeconfig ./target/release/k8s-app"
echo "Clean up: kind delete cluster --name $name"
