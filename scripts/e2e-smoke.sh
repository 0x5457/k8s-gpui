#!/usr/bin/env bash
# Run an end-to-end smoke test against a live kind cluster.
# The script uses an isolated kubeconfig and IPC directory, then checks the app and prints a summary.
#
# Usage:
#   scripts/e2e-smoke.sh [--cluster k8s-gpui-dev] [--context kind-k8s-gpui-dev]
#                        [--app target/debug/k8s-app] [--out /tmp/k8s-gpui-e2e]
#                        [--timeout 30] [--workspace 8] [--build-timeout 900]
#                        [--max-hang 1] [--keep-runs 3]
#                        [--allow-unreachable] [--no-build] [--no-interaction] [--keep]
#
# Notes:
#   - The script writes a minified kubeconfig for the target context and points the app
#     to it with KUBECONFIG.
#   - The app uses an IPC directory under the run directory, so it can run beside other sessions.
#   - The API must be reachable by default. --allow-unreachable tests the disconnected error state.
#   - The test rejects Threshold events and limits startup Budget events to --max-hang.
#     It samples startup before focus or fullscreen and only records later Budget events.
#   - Artifacts: $out/run-<cluster>-<timestamp>/{app.log,*.png,summary.txt}. The script keeps the latest three runs.
#   - Exit codes: 0 = PASS, 1 = FAIL, 2 = missing prerequisite.
set -euo pipefail

cluster="k8s-gpui-dev"
context=""
app="target/debug/k8s-app"
out="/tmp/k8s-gpui-e2e"
window_timeout=30
workspace=8
build_timeout=900
max_hang=1
keep_runs=3
allow_unreachable=0
do_build=1
do_interaction=1
keep_app=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cluster) cluster="${2:?--cluster needs a value}"; shift 2 ;;
        --context) context="${2:?--context needs a value}"; shift 2 ;;
        --app) app="${2:?--app needs a value}"; shift 2 ;;
        --out) out="${2:?--out needs a value}"; shift 2 ;;
        --timeout) window_timeout="${2:?--timeout needs a value}"; shift 2 ;;
        --workspace) workspace="${2:?--workspace needs a value}"; shift 2 ;;
        --build-timeout) build_timeout="${2:?--build-timeout needs a value}"; shift 2 ;;
        --max-hang) max_hang="${2:?--max-hang needs a value}"; shift 2 ;;
        --keep-runs) keep_runs="${2:?--keep-runs needs a value}"; shift 2 ;;
        --allow-unreachable) allow_unreachable=1; shift ;;
        --no-build) do_build=0; shift ;;
        --no-interaction) do_interaction=0; shift ;;
        --keep) keep_app=1; shift ;;
        -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
[[ "$context" == "" ]] && context="kind-$cluster"
[[ "$app" = /* ]] || app="$repo/$app"

for tool in kind kubectl hyprctl grim python3; do
    command -v "$tool" >/dev/null || { echo "Required command not found: $tool" >&2; exit 2; }
done

# The shell can lose Wayland and Hyprland variables.
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
    HYPRLAND_INSTANCE_SIGNATURE="$(ls "/run/user/$(id -u)/hypr" 2>/dev/null | head -1 || true)"
    export HYPRLAND_INSTANCE_SIGNATURE
fi
[[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || { echo "Hyprland instance not found (/run/user/$(id -u)/hypr is empty)" >&2; exit 2; }

fails=()
notes=()
check() {
    local name="$1" ok="$2" detail="${3:-}"
    if [[ "$ok" == 1 ]]; then
        printf '[PASS] %s%s\n' "$name" "${detail:+ :: $detail}"
    else
        printf '[FAIL] %s%s\n' "$name" "${detail:+ :: $detail}"
        fails+=("$name")
    fi
}

echo "== Preflight checks =="
kind get clusters 2>/dev/null | grep -qx "$cluster" \
    || { echo "kind cluster $cluster does not exist. Run scripts/kind-up.sh --name $cluster first." >&2; exit 2; }
kubectl config get-contexts -o name 2>/dev/null | grep -qx "$context" \
    || { echo "Context $context is not in kubeconfig." >&2; exit 2; }

api_reachable=0
if timeout 10 kubectl --context "$context" get --raw=/readyz >/dev/null 2>&1; then
    api_reachable=1
fi
if [[ "$api_reachable" == 1 ]]; then
    echo "[PASS] kind API is reachable through $context"
else
    if [[ "$allow_unreachable" == 1 ]]; then
        echo "[WARN] kind API is unreachable through $context. --allow-unreachable continues with the disconnected error state."
        notes+=("api-unreachable")
    else
        echo "[FAIL] kind API is unreachable through $context. Start the cluster or add --allow-unreachable to test the disconnected error state." >&2
        exit 1
    fi
fi

if [[ "$do_build" == 1 ]]; then
    echo "== cargo build -p k8s-app (timeout ${build_timeout}s) =="
    if ! (cd "$repo" && timeout --foreground "$build_timeout" cargo build -p k8s-app); then
        echo "[FAIL] cargo build -p k8s-app failed or timed out after $build_timeout s" >&2
        exit 1
    fi
else
    [[ -x "$app" ]] || { echo "--no-build was set, but the app does not exist: $app" >&2; exit 2; }
    echo "== Skip build (--no-build), app=$app =="
fi

mkdir -p "$out"
run="$out/run-$cluster-$(date +%m%d-%H%M%S)"
mkdir -p "$run" "$run/ipc"
kubeconfig="$run/kubeconfig"
kubectl config view --raw --minify --context "$context" > "$kubeconfig"
echo "Run directory: $run"
echo "Kubeconfig: $kubeconfig"

# Keep the latest N runs. Run artifacts are small, but long tests can fill /tmp.
if (( keep_runs >= 1 )); then
    while IFS= read -r old; do
        rm -rf "$old"
    done < <(ls -1dt "$out"/run-* 2>/dev/null | tail -n +$((keep_runs + 1)) || true)
fi

app_pid=""
cleanup() {
    if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
        kill "$app_pid" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$app_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$app_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

export KUBECONFIG="$kubeconfig"
export K8S_GPUI_IPC_DIR="$run/ipc"

echo "== Start app =="
nohup "$app" > "$run/app.log" 2>&1 < /dev/null &
app_pid=$!
echo "App PID: $app_pid"

win_field() {
    hyprctl clients -j 2>/dev/null | python3 -c "
import json,sys
pid=$app_pid
for c in json.load(sys.stdin):
    if c.get('class')=='k8s-gpui' and c.get('pid')==pid:
        print(c.get('$1','')); break
" 2>/dev/null || true
}

echo "== Wait for window (class=k8s-gpui, PID=$app_pid, timeout ${window_timeout}s) =="
t0=$(date +%s)
win_addr=""
while :; do
    if ! kill -0 "$app_pid" 2>/dev/null; then
        echo "App exited after startup" >&2
        break
    fi
    win_addr="$(win_field address)"
    [[ -n "$win_addr" ]] && break
    (( $(date +%s) - t0 >= window_timeout )) && break
    sleep 0.1
done
win_ms=$(( ($(date +%s) - t0) * 1000 ))
startup_threshold=0
startup_budget=0
title=""

if [[ -n "$win_addr" ]]; then
    check "Window appeared" 1 "address=$win_addr after ${win_ms}ms"
else
    check "Window appeared" 0 "Window did not appear within ${window_timeout}s (app alive=$([[ -d /proc/$app_pid ]] && echo yes || echo no))"
fi

if [[ -n "$win_addr" ]]; then
    # Sample startup events before focus and fullscreen add more Budget events.
    # The one-second polling thread reports the first-frame event. Wait up to three seconds for it.
    for _ in $(seq 1 15); do
        [[ "$(grep -ac '\[hang\] trigger=Budget' "$run/app.log" || true)" -ge 1 ]] && break
        sleep 0.2
    done
    startup_threshold=$(grep -ac '\[hang\] trigger=Threshold' "$run/app.log" || true)
    startup_budget=$(grep -ac '\[hang\] trigger=Budget' "$run/app.log" || true)

    hyprctl dispatch "hl.dsp.window.move({ workspace = \"$workspace\", follow = true, window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
    hyprctl dispatch "hl.dsp.window.fullscreen({ mode = \"fullscreen\", window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
    hyprctl dispatch "hl.dsp.focus({ window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
    sleep 2
fi

shot() { grim "$run/$1.png" >/dev/null 2>"$run/grim-$1.err" || true; }
shot 01-startup

title="$(win_field title)"
if [[ -n "$win_addr" && "$title" == *"$context"* ]]; then
    check "Window title contains the cluster name" 1 "title=$title"
elif [[ -n "$win_addr" ]]; then
    check "Window title contains the cluster name" 0 "title=$title (expected to contain $context)"
fi

interaction="skipped"
palette_diff="n/a"
closed_diff="n/a"
if [[ -n "$win_addr" && "$do_interaction" == 1 ]]; then
    echo "== Interaction: Ctrl+Shift+P command palette -> Esc =="
    active=""
    for _ in 1 2 3; do
        active="$(hyprctl activewindow -j 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('address',''))" 2>/dev/null || true)"
        [[ "$active" == "$win_addr" ]] && break
        hyprctl dispatch "hl.dsp.focus({ window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
        sleep 0.3
    done
    if [[ "$active" != "$win_addr" ]]; then
        interaction="skipped-focus-lost"
        echo "[WARN] Window did not get focus (active=$active). Skip interaction."
    else
        wtype -M ctrl -M shift -k p -m shift -m ctrl >/dev/null 2>&1 || true
        sleep 0.8
        shot 02-palette
        wtype -k escape >/dev/null 2>&1 || true
        sleep 0.5
        shot 03-palette-closed
        interaction="sent"
        if cmp -s "$run/01-startup.png" "$run/02-palette.png"; then
            palette_diff="same"
        else
            palette_diff="changed"
        fi
        if cmp -s "$run/01-startup.png" "$run/03-palette-closed.png"; then
            closed_diff="same"
        else
            closed_diff="changed"
        fi
    fi
fi

sleep 1
alive=0
kill -0 "$app_pid" 2>/dev/null && alive=1

panics=$(grep -ac 'panicked' "$run/app.log" || true)
hang_threshold=$(grep -ac '\[hang\] trigger=Threshold' "$run/app.log" || true)
hang_budget=$(grep -ac '\[hang\] trigger=Budget' "$run/app.log" || true)

check "App stayed alive until the end" "$alive" "PID=$app_pid"
check "No panic" "$([[ "$panics" == 0 ]] && echo 1 || echo 0)" "panicked=$panics"
check "No Threshold hang events" "$([[ "$hang_threshold" == 0 ]] && echo 1 || echo 0)" "threshold=$hang_threshold startup=$startup_threshold"
check "Startup Budget events <= $max_hang" "$([[ "$startup_budget" -le "$max_hang" ]] && echo 1 || echo 0)" "startup_budget=$startup_budget"
notes+=("hang budget: startup=$startup_budget total=$hang_budget")
# The app logs the context it settled on once the cluster session resolves, so
# this is the line that proves startup actually reached a cluster.
check "Startup log contains the cluster line" "$(grep -aq "switched cluster to " "$run/app.log" && echo 1 || echo 0)" "$(grep -a 'switched cluster to ' "$run/app.log" | tail -1)"
check "Screenshot is not empty" "$([[ -s "$run/01-startup.png" ]] && echo 1 || echo 0)" "01-startup.png $(stat -c %s "$run/01-startup.png" 2>/dev/null || echo 0)B"

if [[ "$keep_app" == 1 ]]; then
    echo "== --keep: App PID: $app_pid. Stop it with: kill $app_pid =="
    app_pid=""
fi

size_mb=$(du -sm "$run" 2>/dev/null | awk '{print $1}')
if [[ "${size_mb:-0}" -gt 100 ]]; then
    notes+=("run-dir-${size_mb}MB")
fi

result=PASS
[[ ${#fails[@]} -eq 0 ]] || result=FAIL

{
    echo "=== e2e-smoke $cluster @ $(date '+%F %T') ==="
    echo "context:     $context (api_reachable=$api_reachable)"
    echo "app:         $app ($(stat -c '%y' "$app" 2>/dev/null || echo '?'))"
    echo "window:      ${win_addr:-<none>} after ${win_ms}ms"
    echo "title:       ${title:-<none>}"
    echo "interaction: $interaction (palette_diff=$palette_diff closed_diff=$closed_diff)"
    echo "hang:        threshold=$hang_threshold budget_startup=$startup_budget budget_total=$hang_budget (max startup $max_hang)"
    echo "panic:       $panics"
    echo "run size:    ${size_mb:-0}MB"
    echo "notes:       ${notes[*]:-<none>}"
    echo "checks:      ${#fails[@]} failed"
    for f in "${fails[@]:-}"; do
        [[ -n "$f" ]] && echo "  - FAIL: $f"
    done
    echo "artifacts:   $run"
    echo "RESULT:      $result"
} | tee "$run/summary.txt"

echo
echo "Clean up: rm -rf $out"
[[ "$result" == PASS ]] && exit 0 || exit 1
