#!/usr/bin/env bash
# Add latency and packet loss to the kind API server path with tc netem.
#
# Usage:
#   netem-latency.sh apply <150ms|300ms|300ms-loss> [cluster]
#   netem-latency.sh clean [cluster]
#   netem-latency.sh status [cluster]
#
# Profiles use round-trip time (RTT). Each direction receives half of the delay.
#   150ms        75ms per direction -> measured RTT ~150ms
#   300ms        150ms per direction -> measured RTT ~300ms
#   300ms-loss   150ms per direction + 1% loss
#
# Implementation:
#   - This host has no passwordless sudo. The script runs `tc` as root in the kind
#     control-plane container.
#   - The container shares the API server network namespace. The script shapes client
#     traffic through eth0. Kubelet traffic uses local routes.
#   - Egress uses the eth0 root qdisc. An ingress qdisc redirects traffic to ifb0.
#   - The script prints every command. Run `clean` after testing. Cleanup removes
#     the netem qdiscs and ifb0, then verifies that they are absent.
#
# Defaults:
#   cluster  k8s-gpui-dev
#
# Example:
#   scripts/netem-latency.sh apply 300ms k8s-gpui-dev
#   scripts/netem-latency.sh status k8s-gpui-dev
#   scripts/netem-latency.sh clean k8s-gpui-dev
# Run these commands from the repository root.

set -euo pipefail

usage() {
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

action="${1:-}"
case "$action" in
    apply|clean|status) ;;
    -h|--help) usage ;;
    *) usage ;;
esac

delay=""
loss=""
if [[ "$action" == "apply" ]]; then
    profile="${2:-}"
    shift 2 || usage
    cluster="${1:-k8s-gpui-dev}"
    case "$profile" in
        150ms) delay="75ms"; loss="0" ;;
        300ms) delay="150ms"; loss="0" ;;
        300ms-loss) delay="150ms"; loss="1%" ;;
        *) echo "Unknown profile: ${profile:-missing profile} (150ms / 300ms / 300ms-loss)" >&2; exit 2 ;;
    esac
else
    shift 1 || true
    cluster="${1:-k8s-gpui-dev}"
fi

command -v docker >/dev/null || { echo "Required command not found: docker" >&2; exit 1; }
command -v kind >/dev/null || { echo "Required command not found: kind" >&2; exit 1; }

node="$(kind get nodes --name "$cluster" 2>/dev/null | grep -E 'control-plane' | head -1 || true)"
[[ -n "$node" ]] || node="$(kind get nodes --name "$cluster" 2>/dev/null | head -1 || true)"
if [[ -z "$node" ]]; then
    echo "No node found for cluster $cluster. kind get nodes returned no nodes." >&2
    exit 1
fi

run_in_node() {
    echo "+ docker exec $node sh -c '$1'" >&2
    docker exec "$node" sh -c "$1"
}

cleanup_qdiscs() {
    run_in_node '
        tc qdisc del dev eth0 ingress 2>/dev/null || true
        tc qdisc del dev eth0 root 2>/dev/null || true
        ip link del ifb0 2>/dev/null || true
    '
}

case "$action" in
    apply)
        echo "== Remove old netem qdiscs and ifb0 (idempotent) =="
        cleanup_qdiscs
        echo "== Apply profile=$profile delay=$delay per direction loss=$loss =="
        netem="netem delay $delay"
        [[ "$loss" != "0" ]] && netem="$netem loss $loss"
        run_in_node "
            set -e
            ip link add ifb0 type ifb
            ip link set ifb0 up
            tc qdisc add dev ifb0 root $netem
            tc qdisc add dev eth0 root $netem
            tc qdisc add dev eth0 handle ffff: ingress
            tc filter add dev eth0 parent ffff: protocol ip u32 match u32 0 0 action mirred egress redirect dev ifb0
        "
        echo "== Verify qdiscs and ifb0 =="
        run_in_node 'tc qdisc show dev eth0; tc qdisc show dev ifb0'
        echo "After testing, run: $0 clean $cluster"
        ;;
    clean)
        echo "== Remove netem qdiscs and ifb0 =="
        cleanup_qdiscs
        echo "== Verify qdisc and ifb0 removal =="
        remaining="$(run_in_node 'tc qdisc show dev eth0; ip -o link show ifb0 2>/dev/null || true')"
        echo "$remaining"
        if echo "$remaining" | grep -qE 'netem|ifb0'; then
            echo "Cleanup failed: a netem qdisc or ifb0 remains" >&2
            exit 1
        fi
        echo "Cleanup complete: eth0 has no netem qdisc and ifb0 is absent"
        ;;
    status)
        run_in_node 'tc qdisc show dev eth0; tc qdisc show dev ifb0 2>/dev/null || true'
        if command -v kubectl >/dev/null; then
            echo "== kubectl /version round trip (includes local process time) =="
            start=$(date +%s.%N)
            kubectl --context "kind-$cluster" get --raw=/version >/dev/null 2>&1 || true
            end=$(date +%s.%N)
            awk -v s="$start" -v e="$end" 'BEGIN { printf "elapsed: %.3fs\n", e - s }'
        fi
        ;;
esac
