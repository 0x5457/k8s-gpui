#!/usr/bin/env bash
# Serialized UI screenshot session for k8s-gpui UI lanes.
#
# One display, one compositor, one focus: two app windows cannot be screenshotted
# at the same time, and a window that is not focused is not on the output. Every
# lane therefore takes a machine-wide lock for the whole run (launch -> keys ->
# grim -> kill), which serializes the *visual* verification while the code work
# stays parallel.
#
# Usage:
#   scripts/uishot.sh --lane NAME [options]
#
#   --lane NAME          lane id; owns the lock, the workspace and the state dirs
#   --app PATH           binary to run (default: target/main/debug/k8s-app)
#   --out DIR            artifact dir (default: /tmp/k8s-shots/<lane>/<ts>)
#   --context CTX        kubeconfig context (default: current kubectl context)
#   --theme dark|light   pre-seed the lane's settings.json (default: dark)
#   --workspace N        Hyprland workspace to move the window to (default: 30)
#   --wait SECONDS       settle time after the window appears (default: 4)
#   --step "KEYS|name"   send wtype keys, then shoot <step-index>-<name>.png.
#                        KEYS is a wtype argument list, e.g. "ctrl+shift+p",
#                        "-k escape", "-k Return", "shift+slash". Use "sleep:2"
#                        as KEYS to only wait.
#   --click "X,Y|name"   left-click at LOGICAL coordinates (the output is scaled;
#                        `hyprctl monitors` reports the scale). Steps and clicks
#                        share one sequence: all --step, then all --click.
#   --click-button BTN   ydotool button code for --click. Per `ydotool click
#                        --help`: 0xC0 = left click (down+up), 0xC1 = right click.
#                        Or set CLICK_BUTTON in the env.
#   --keep               leave the app running (prints the PID) instead of killing it
#   --lock-wait SECONDS  how long to wait for the machine-wide lock (default: 1800)
#
# Examples:
#   scripts/uishot.sh --lane table --step "ctrl+shift+p|palette" --step "-k escape|closed"
#   scripts/uishot.sh --lane table --theme light --step "-k Down -k Return|selected"
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock_dir="/tmp/k8s-gpui-ui"
mkdir -p "$lock_dir"

lane="lane"
app="$repo/target/main/debug/k8s-app"
out=""
context=""
theme="dark"
workspace=30
wait_s=4
lock_wait=1800
keep=0
declare -a steps=()
declare -a clicks=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --lane) lane="$2"; shift 2 ;;
        --app) app="$2"; shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --context) context="$2"; shift 2 ;;
        --theme) theme="$2"; shift 2 ;;
        --workspace) workspace="$2"; shift 2 ;;
        --wait) wait_s="$2"; shift 2 ;;
        --lock-wait) lock_wait="$2"; shift 2 ;;
        --keep) keep=1; shift ;;
        --step) steps+=("$2"); shift 2 ;;
        --click) clicks+=("$2"); shift 2 ;;
        -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

[[ "$app" = /* ]] || app="$repo/$app"
if [[ ! -x "$app" ]]; then
    echo "no such binary: $app" >&2
    echo "build it first:  CARGO_TARGET_DIR=target/<lane> mbx build -p k8s-app" >&2
    exit 2
fi

if [[ -z "$out" ]]; then
    out="/tmp/k8s-shots/$lane/$(date +%m%d-%H%M%S)"
fi
mkdir -p "$out"

# ── the machine-wide lock ────────────────────────────────────────────────────
exec 9>"$lock_dir/shot.lock"
if ! flock -w "$lock_wait" 9; then
    echo "TIMEOUT waiting for the screenshot lock ($lock_wait s)." >&2
    echo "Another lane is holding it. Retry later; do not screenshot without it." >&2
    exit 3
fi
echo "[lock] acquired (lane=$lane)"

# ── isolated state, so two lanes never fight over settings.json ──────────────
state="$lock_dir/state/$lane"
rm -rf "$state"
mkdir -p "$state"/{config/k8s-gpui,data,state,cache,ipc,runtime}
if [[ "$theme" == "light" ]]; then
    printf '{"theme":{"mode":"light","theme":"K8s Studio Light"}}\n' \
        > "$state/config/k8s-gpui/settings.json"
else
    printf '{"theme":{"mode":"dark","theme":"K8s Studio Dark"}}\n' \
        > "$state/config/k8s-gpui/settings.json"
fi

if [[ -z "$context" ]]; then
    context="$(kubectl config current-context 2>/dev/null || true)"
fi
[[ -n "$context" ]] || { echo "no kubectl context" >&2; exit 2; }
kubectl config view --raw --minify --context "$context" > "$state/kubeconfig" 2>/dev/null

export KUBECONFIG="$state/kubeconfig"
export XDG_CONFIG_HOME="$state/config"
export XDG_DATA_HOME="$state/data"
export XDG_STATE_HOME="$state/state"
export XDG_CACHE_HOME="$state/cache"
# XDG_RUNTIME_DIR is deliberately NOT redirected: Hyprland's socket lives there and
# gpui cannot reach the compositor without it. The app's own IPC is redirected by
# K8S_GPUI_IPC_DIR instead.
export K8S_GPUI_IPC_DIR="$state/ipc"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
    HYPRLAND_INSTANCE_SIGNATURE="$(ls "/run/user/$(id -u)/hypr" 2>/dev/null | head -1 || true)"
    export HYPRLAND_INSTANCE_SIGNATURE
fi

app_pid=""
cleanup() {
    if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
        kill "$app_pid" 2>/dev/null || true
        for _ in $(seq 1 20); do
            kill -0 "$app_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$app_pid" 2>/dev/null || true
    fi
    echo "[lock] released"
}
trap cleanup EXIT

echo "[run] $app (context=$context theme=$theme workspace=$workspace)"

# Clear out any `k8s-gpui` left behind by an earlier lane before launching.
#
# Two instances on one workspace is the failure this exists to stop, and it is
# silent: the compositor focuses whichever launched last, `grim` photographs
# whichever is on top, and a click lands on whichever is under the pointer. A run
# then reports a control that "does nothing" while a *different* window — often on
# a different cluster — quietly answers it. The evidence is three windows on
# workspace 30 with two different kubeconfig contexts in their titles, and a click
# that opened a tab on the instance nobody was photographing.
#
# Only the app class, never the whole process name: a build's own binary path
# differs per target directory, and an agent's `--keep` instance is still theirs.
stale="$(pgrep -f 'k8s-app' 2>/dev/null | grep -v "^${app_pid:-0}$" || true)"
if [[ -n "$stale" ]]; then
    stale_count="$(wc -w <<<"$stale")"
    echo "[stale] killing $stale_count earlier k8s-app instance(s): $stale"
    # shellcheck disable=SC2086
    kill $stale 2>/dev/null || true
    sleep 0.6
    # shellcheck disable=SC2086
    kill -9 $stale 2>/dev/null || true
fi

"$app" > "$out/app.log" 2>&1 < /dev/null &
app_pid=$!

win_field() {
    hyprctl clients -j 2>/dev/null | python3 -c "
import json,sys
pid=$app_pid
for c in json.load(sys.stdin):
    if c.get('class')=='k8s-gpui' and c.get('pid')==pid:
        print(c.get('$1','')); break
" 2>/dev/null || true
}

win_addr=""
for _ in $(seq 1 300); do
    kill -0 "$app_pid" 2>/dev/null || break
    win_addr="$(win_field address)"
    [[ -n "$win_addr" ]] && break
    sleep 0.1
done
if [[ -z "$win_addr" ]]; then
    echo "window never appeared; app log:" >&2
    tail -20 "$out/app.log" >&2 || true
    exit 1
fi

hyprctl dispatch "hl.dsp.window.move({ workspace = \"$workspace\", follow = true, window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
sleep 0.4
hyprctl dispatch "hl.dsp.window.fullscreen({ mode = \"fullscreen\", window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
for _ in 1 2 3 4 5; do
    hyprctl dispatch "hl.dsp.focus({ window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
    active="$(hyprctl activewindow -j 2>/dev/null | jq -r '.address // ""' 2>/dev/null || true)"
    [[ "$active" == "$win_addr" ]] && break
    sleep 0.3
done
sleep "$wait_s"

shot() { grim "$out/$1.png" >/dev/null 2>&1 || echo "grim failed for $1" >&2; }

# Send a chord. `wtype` has no `ctrl+shift+p` positional form — it takes plain
# text plus explicit `-M`/`-m` flags, and a `+`-joined chord is parsed as literal
# text to type. So `wtype "ctrl+shift+p"` typed the word "ctrl+shift+p" and every
# "keyboard" step in every lane was a no-op that looked like a product that
# ignores the keyboard. Translate a chord here instead, so `--step` accepts the
# form its own help text documents.
#
#   "ctrl+shift+p"  ->  -M ctrl -M shift -k p -m shift -m ctrl
#   "-k escape"     ->  passed through (already wtype syntax)
#   "hello"         ->  passed through (plain text)
press() {
    local chord="$1"
    [[ -z "$chord" ]] && return 0
    if [[ "$chord" == -* ]]; then
        # Already wtype syntax: split and pass through untouched.
        # shellcheck disable=SC2206
        wtype $chord >/dev/null 2>&1 || true
        return 0
    fi
    if [[ "$chord" == *+* ]]; then
        local mods=() key="" part
        local IFS='+'
        for part in $chord; do
            if [[ -z "$key" ]]; then
                mods+=("$part")
            else
                key="$part"
            fi
        done
        unset IFS
        [[ -z "$key" ]] && return 0
        local args=()
        for part in "${mods[@]}"; do args+=(-M "$part"); done
        args+=(-k "$key")
        for ((i=${#mods[@]}-1; i>=0; i--)); do args+=(-m "${mods[$i]}"); done
        wtype "${args[@]}" >/dev/null 2>&1 || true
        return 0
    fi
    wtype "$chord" >/dev/null 2>&1 || true
}

click_at() {
    local x="$1" y="$2" button="${CLICK_BUTTON:-0xC0}"
    # `--click` takes LOGICAL coordinates and halves them here.
    #
    # The pointer's absolute range is 960x540 while the screen is 1920x1080 logical,
    # so one input unit spans two logical pixels. Measured, not assumed:
    #
    #   input (100,100)  -> cursorpos (200,200)      x2
    #   input (800,500)  -> cursorpos (1600,1000)    x2
    #   input (900,540)  -> cursorpos (1800,1079)    x2, last value that is not clamped
    #   input (1000,600) -> cursorpos (1919,1079)    CLAMPED to the corner
    #
    # `cursorpos` reporting exactly 2x is why this reads as a no-op if you skip the
    # division: the pointer is where you asked, in physical terms, and the app is
    # told a logical position twice as large as the one you meant. And because
    # anything past input 960 lands in the corner rather than erroring, the symptom
    # is "every click hits the same thing" -- which looks like a coordinate mistake
    # rather than a driver range, and was chased three separate times.
    #
    x=$(( x / 2 ))
    y=$(( y / 2 ))
    # `-a` is the absolute move; a bare `mousemove` is *relative*, so every click
    # used to land at the running sum of all previous ones.
    ydotool mousemove -a "$x" "$y" >/dev/null 2>&1 || true
    sleep 0.3
    # 0xC0 is left-button down+up per `ydotool click --help`. This used to try
    # 0xC0 first and fall back to 1, and `ydotool click` exits 0 either way — so
    # the fallback never ran and the intent was carried by a kernel code (1 is
    # KEY_ESC) that is not a mouse button at all.
    ydotool click "$button" >/dev/null 2>&1 || true
    sleep "${STEP_SETTLE:-1.2}"
}


shot 00-startup
echo "[shot] $out/00-startup.png"

# Steps and clicks share one sequence so a run can click into a field and *then*
# type, which two separate loops could not express. Within a phase they keep
# their own order: all steps, then all clicks.
seq=()
for step in "${steps[@]+"${steps[@]}"}"; do seq+=("step:$step"); done
for click in "${clicks[@]+"${clicks[@]}"}"; do seq+=("click:$click"); done

idx=1
for item in "${seq[@]+"${seq[@]}"}"; do
    kind="${item%%:*}"
    body="${item#*:}"
    name="${body##*|}"
    if [[ "$kind" == step ]]; then
        keys="${body%%|*}"
        if [[ "$keys" == sleep:* ]]; then
            sleep "${keys#sleep:}"
        elif [[ -n "$keys" ]]; then
            # shellcheck disable=SC2206
            press ${keys}
            sleep "${STEP_SETTLE:-1.2}"
        fi
    else
        xy="${body%%|*}"
        click_at "${xy%%,*}" "${xy##*,}"
    fi
    printf -v fname '%02d-%s.png' "$idx" "$name"
    shot "$fname"
    echo "[shot] $out/$fname"
    idx=$((idx + 1))
done


{
    echo "lane:      $lane"
    echo "app:       $app"
    echo "context:   $context"
    echo "theme:     $theme"
    echo "out:       $out"
    echo "panics:    $(grep -ac 'panicked' "$out/app.log" || true)"
    echo "hang:      $(grep -ac '\[hang\] trigger=Threshold' "$out/app.log" || true) threshold, $(grep -ac '\[hang\] trigger=Budget' "$out/app.log" || true) budget"
    echo "cluster:   $(grep -a 'switched cluster to ' "$out/app.log" | tail -1 || echo '<never reached a cluster>')"
} | tee "$out/summary.txt"

if [[ "$keep" == 1 ]]; then
    trap - EXIT
    echo "[keep] app PID $app_pid still running; kill it when done."
    echo "[lock] released"
    exec 9>&-
else
    exit 0
fi
