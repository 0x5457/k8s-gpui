#!/usr/bin/env bash
# Generate synthetic load for a large-cluster performance test.
# Usage: seed-pods.sh [total-pods] [replicas-per-deployment] [namespace]
#                        [node-selector] [deployment-prefix] [toleration-key]
#
# Defaults: 10000 Pods, 50 replicas per Deployment, perf namespace, no node
#           selector, perf prefix, and no toleration.
# Set node-selector to key=value, such as k8s-gpui.dev/tier=fake.
# Leave it empty to omit nodeSelector. A toleration-key adds a NoSchedule toleration
# with the Exists operator.
set -euo pipefail
total="${1:-10000}"
per="${2:-50}"
ns="${3:-perf}"
node_selector="${4:-}"
prefix="${5:-perf}"
toleration_key="${6:-}"
deployments=$(( (total + per - 1) / per ))

kubectl get ns "$ns" >/dev/null 2>&1 || kubectl create ns "$ns"

selector_block=""
if [[ -n "$node_selector" ]]; then
    key="${node_selector%%=*}"
    value="${node_selector#*=}"
    selector_block="      nodeSelector:
        $key: \"$value\""
fi
toleration_block=""
if [[ -n "$toleration_key" ]]; then
    toleration_block="      tolerations:
        - key: $toleration_key
          operator: Exists
          effect: NoSchedule"
fi

for i in $(seq 1 "$deployments"); do
    shard=$((i % 16))
    cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $prefix-$i
  namespace: $ns
  labels: { app: $prefix, shard: "$shard" }
spec:
  replicas: $per
  selector: { matchLabels: { app: $prefix, shard: "$shard" } }
  template:
    metadata: { labels: { app: $prefix, shard: "$shard" } }
    spec:
$selector_block
$toleration_block
      containers:
        - name: pause
          image: registry.k8s.io/pause:3.10
          resources: { requests: { cpu: 1m, memory: 8Mi } }
EOF
done | kubectl apply -f - >/dev/null

echo "Seeded approximately $total Pods in namespace $ns ($deployments Deployments, $per replicas each, prefix=$prefix)"
echo "Delete the namespace with: kubectl delete ns $ns"
