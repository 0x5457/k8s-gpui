# K8s Studio

A fast, native desktop client for Kubernetes, built in Rust with [GPUI](https://gpui.rs).

Browse every resource in your clusters, watch it update live, and act on it —
logs, exec, port-forwards, YAML, metrics — without leaving the keyboard.

<p align="center">
  <img src="https://github.com/user-attachments/assets/b70d1e27-90cf-43c0-bb73-e059a2dce163" alt="K8s Studio — dark theme" width="880">
</p>
<p align="center">
  <img src="https://github.com/user-attachments/assets/88016df8-9c19-4cb8-8f2b-d7a52da73979" alt="K8s Studio — light theme" width="880">
</p>

## Features

- **Multi-cluster** — read kubeconfig contexts, switch clusters and namespaces instantly
- **Live resource tables** — every API kind, streamed and updated in real time
- **Overview dashboard** — workload health, cluster-wide CPU/memory, conditions, event stream
- **Logs & exec** — follow container logs, open an interactive shell in a pod
- **Port-forwards** — create and manage forwards from the UI
- **YAML editor** — inspect, edit and apply manifests with syntax highlighting
- **Command palette** — every action one keystroke away

## Building

```sh
cargo build -p k8s-app
cargo run -p k8s-app
```

Requires Rust 1.98+ and a valid `~/.kube/config` (or `$KUBECONFIG`).

## License

[GPL-3.0-or-later](LICENSE)
