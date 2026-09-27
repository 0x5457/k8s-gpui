#!/usr/bin/env bash
# Create a local kind test cluster.
#
# Usage:
#   kind-up.sh [name]        Create a one-node cluster with the default maxPods=110.
#   kind-up.sh --name X [--nodes M] [--max-pods N]
#                            Create a large performance-test cluster.
#
# --max-pods makes two required changes:
#   1. Increase kubelet maxPods on each node with a KubeletConfiguration patch.
#   2. Set kube-controller-manager node-cidr-mask-size=19. This gives each node 8190 Pod IP addresses.
#      The default /24 podCIDR has 254 addresses, so Pod 255 stays in ContainerCreating.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: kind-up.sh [name]
      kind-up.sh --name X [--nodes M] [--max-pods N]

  --name X       Cluster name (default: k8s-gpui-dev)
  --nodes M      Number of nodes. Node 1 is the control-plane. The rest are workers (default: 1).
  --max-pods N   Set kubelet maxPods and node-cidr-mask-size=19 (default: not set)
EOF
}

name="k8s-gpui-dev"
nodes=1
max_pods=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --name) name="${2:?--name needs a value}"; shift 2 ;;
        --nodes) nodes="${2:?--nodes needs a value}"; shift 2 ;;
        --max-pods) max_pods="${2:?--max-pods needs a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "Unknown option: $1" >&2; usage; exit 2 ;;
        *) name="$1"; shift ;;
    esac
done

[[ "$nodes" =~ ^[0-9]+$ && "$nodes" -ge 1 ]] || { echo "--nodes requires a positive integer" >&2; exit 2; }
if [[ -n "$max_pods" ]]; then
    [[ "$max_pods" =~ ^[0-9]+$ && "$max_pods" -ge 1 ]] || { echo "--max-pods requires a positive integer" >&2; exit 2; }
fi

if kind get clusters 2>/dev/null | grep -qx "$name"; then
    echo "Cluster $name already exists"
    exit 0
fi

config="$(mktemp -t "kind-$name-XXXXXX.yaml")"
trap 'rm -f "$config"' EXIT

{
    printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nname: %s\nnodes:\n' "$name"
    for ((i = 0; i < nodes; i++)); do
        role=worker
        ((i == 0)) && role=control-plane
        printf '  - role: %s\n' "$role"
        if [[ -n "$max_pods" ]]; then
            printf '    kubeadmConfigPatches:\n'
            printf '      - |\n'
            printf '        kind: KubeletConfiguration\n'
            printf '        maxPods: %s\n' "$max_pods"
            if ((i == 0)); then
                printf '      - |\n'
                printf '        kind: ClusterConfiguration\n'
                printf '        controllerManager:\n'
                printf '          extraArgs:\n'
                printf '            - name: node-cidr-mask-size\n'
                printf '              value: "19"\n'
            fi
        fi
    done
} > "$config"

wait_args=()
[[ -n "$max_pods" || "$nodes" -gt 1 ]] && wait_args=(--wait 5m)

kind create cluster --name "$name" --config "$config" "${wait_args[@]}"

# Large clusters need larger kernel tables. ARP/neigh gc_thresh3 defaults to 1024 and can overflow above 3000 Pods per node.
# inotify defaults to 128 instances. containerd and kubelet watch each Pod directory.
# Some sysctl keys do not exist in the container network namespace. Set only keys that exist.
if [[ -n "$max_pods" ]]; then
    while IFS= read -r node; do
        docker exec "$node" sh -c '
            for kv in \
                net.ipv4.neigh.default.gc_thresh1=8192 \
                net.ipv4.neigh.default.gc_thresh2=32768 \
                net.ipv4.neigh.default.gc_thresh3=65536 \
                fs.inotify.max_user_instances=8192 \
                fs.inotify.max_user_watches=1048576
            do
                key="/proc/sys/$(echo "${kv%%=*}" | tr . /)"
                [ -e "$key" ] && sysctl -q -w "$kv"
            done
            true
        '
        echo "Updated sysctl settings on $node"
    done < <(kind get nodes --name "$name")
fi

kubectl cluster-info --context "kind-$name"
