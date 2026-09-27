#!/usr/bin/env bash
# Measure a release app against a live cluster: startup, full sync, frame time, RSS, and count consistency.
#
# Usage:
#   scripts/perf-measure.sh [--scenario idle|scroll|storm] [--context kind-k8s-gpui-big]
#                           [--app target/release/k8s-app] [--out DIR] [--settle 10]
#                           [--scroll-keys 200] [--storm-rounds 1]
#
# Requires: grim, tesseract, hyprctl with Lua support, wtype, and python3.
# Compatibility names: storm = Frequent Updates; --storm-rounds = Test Updates rounds.
# Each scenario runs one app instance. Frame histograms cover the window lifetime.
# Output: $out/summary.txt, app.log, and first-screen screenshots.
# stdout prints the same summary.
set -euo pipefail

scenario=idle
context=kind-k8s-gpui-big
app=target/release/k8s-app
out=""
settle=10
scroll_keys=200
storm_rounds=1
sync_timeout=180

while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario) scenario="${2:?Missing value for --scenario}"; shift 2 ;;
        --context) context="${2:?Missing value for --context}"; shift 2 ;;
        --app) app="${2:?Missing value for --app}"; shift 2 ;;
        --out) out="${2:?Missing value for --out}"; shift 2 ;;
        --settle) settle="${2:?Missing value for --settle}"; shift 2 ;;
        --scroll-keys) scroll_keys="${2:?Missing value for --scroll-keys}"; shift 2 ;;
        --storm-rounds) storm_rounds="${2:?Missing value for --storm-rounds}"; shift 2 ;;
        -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

case "$scenario" in idle|scroll|storm) ;; *) echo "Invalid --scenario value: $scenario. Use idle, scroll, or storm." >&2; exit 2 ;; esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
[[ "$app" = /* ]] || app="$repo/$app"
kubeconfig="/tmp/opencode/perf-10k/kubeconfig-${context#kind-}"
[[ -f "$kubeconfig" ]] || { echo "Required kubeconfig is missing: $kubeconfig. Run perf-10k.sh to create a live cluster." >&2; exit 1; }
[[ -x "$app" ]] || { echo "App does not exist: $app" >&2; exit 1; }

export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
    export HYPRLAND_INSTANCE_SIGNATURE="$(ls /run/user/1000/hypr 2>/dev/null | head -1)"
fi

[[ -n "$out" ]] || out="/tmp/opencode/perf-10k/measure-$(date +%m%d-%H%M%S)-$scenario"
mkdir -p "$out"
log="$out/app.log"
fifo="$out/app.fifo"
rm -f "$fifo" && mkfifo "$fifo"

export KUBECONFIG="$kubeconfig"
expected=$(kubectl get pods -A --no-headers | wc -l)
echo "Expected Pods in the live cluster (kubectl -A): $expected"

now_ms() { date +%s%3N; }
t0=$(now_ms)

# Add millisecond timestamps to log lines. The FIFO reader avoids a pipe PID lookup.
python3 -u -c 'import sys,time
for line in sys.stdin: print(f"{time.time()*1000:.0f} {line}", end="")' < "$fifo" > "$log" &
ts_pid=$!
K8S_GPUI_FRAME_STATS=1 K8S_GPUI_DEBUG_OVERLAY=1 stdbuf -oL -eL "$app" > "$fifo" 2>&1 &
app_pid=$!

cleanup() {
    kill "$app_pid" 2>/dev/null || true
    sleep 1
    kill -9 "$app_pid" 2>/dev/null || true
    kill "$ts_pid" 2>/dev/null || true
    rm -f "$fifo"
}
trap cleanup EXIT

# Wait for the window with hyprctl clients polling and record its address.
win_addr=""
while :; do
    if ! kill -0 "$app_pid" 2>/dev/null; then
        echo "App exited before the window appeared:" >&2; sed -n '1,20p' "$log" >&2; exit 1
    fi
    win_addr=$(hyprctl clients -j 2>/dev/null | python3 -c "
import json,sys
for c in json.load(sys.stdin):
    if c['class']=='k8s-gpui' and '$context' in c['title']:
        print(c['address']); break
" 2>/dev/null || true)
    [[ -n "$win_addr" ]] && break
    if (( $(now_ms) - t0 > 30000 )); then
        echo "Window did not appear within 30s" >&2
        exit 1
    fi
    sleep 0.05
done
t_window=$(now_ms)
echo "Window $win_addr appeared after $((t_window - t0))ms (PID $app_pid)"

# Move to workspace 9 and enter fullscreen. The shared desktop needs a fixed workspace for screenshots and key input.
hyprctl dispatch "hl.dsp.window.move({ workspace = \"9\", follow = true, window = \"address:$win_addr\" })" >/dev/null
hyprctl dispatch "hl.dsp.window.fullscreen({ mode = \"fullscreen\", window = \"address:$win_addr\" })" >/dev/null
sleep 0.5

win_geom() {
    hyprctl clients -j | python3 -c "
import json,sys
for c in json.load(sys.stdin):
    if c['address']=='$win_addr':
        print(c['at'][0], c['at'][1], c['size'][0], c['size'][1], c['fullscreen']); break
"
}

geom=$(win_geom)
read -r wx wy ww wh wfs <<<"$geom"
scale=$(hyprctl monitors -j | python3 -c "import json,sys; print(int(json.load(sys.stdin)[0]['scale']))")
echo "Window geometry (logical): $wx,$wy ${ww}x${wh} fullscreen=$wfs scale=$scale"

# Toolbar count area: right of the status badge (count + RTT + debug overlay).
strip_x=$((wx + 230)); strip_y=$((wy + 70)); strip_w=520; strip_h=60
interference=0

active_is_mine() {
    hyprctl activewindow -j 2>/dev/null | grep -q "\"address\": \"$win_addr\""
}

ocr_count() {
    grim -g "$((strip_x * scale)),$((strip_y * scale)) $((strip_w * scale))x$((strip_h * scale))" "$out/strip.png" 2>/dev/null || return 1
    tesseract "$out/strip.png" stdout --psm 7 -c tessedit_char_whitelist=0123456789,/syncing 2>/dev/null \
        | tr '\n' ' ' | sed 's/[^0-9,/syncing]//g'
}

t_first_row=""
t_full=""
last_count=""
rss_peak=0
while :; do
    now=$(now_ms)
    if ! kill -0 "$app_pid" 2>/dev/null; then
        echo "App exited" >&2
        break
    fi
    # Restore focus if another process takes it, and count the event.
    if ! active_is_mine; then
        interference=$((interference + 1))
        hyprctl dispatch "hl.dsp.focus({ window = \"address:$win_addr\" })" >/dev/null 2>&1 || true
        sleep 0.1
        continue
    fi
    geom=$(win_geom)
    read -r nwx nwy nww nwh nfs <<<"$geom"
    if [[ -n "${nww:-}" && "$nww" != "$ww" ]]; then
        ww=$nww
        strip_x=$((nwx + 230))
    fi
    if [[ -r "/proc/$app_pid/status" ]]; then
        rss=$(awk '/^VmRSS/{print $2}' "/proc/$app_pid/status")
        if (( rss > rss_peak )); then rss_peak=$rss; fi
    fi
    text=$(ocr_count || true)
    count=$(echo "$text" | grep -oE '[0-9][0-9,]*' | head -1 || true)
    if [[ -n "$count" ]]; then
        count_num=$(( 10#${count//,/} ))
        if [[ -z "$t_first_row" && "$count_num" -gt 0 ]]; then
            t_first_row=$now
            cp "$out/strip.png" "$out/first-row-strip.png" 2>/dev/null || true
            grim "$out/first-row.png" 2>/dev/null || true
        fi
        if [[ "$count_num" -ge "$expected" && "$text" != *sync* ]]; then
            t_full=$now
            last_count=$count_num
            break
        fi
        last_count=$count_num
    fi
    if (( now - t0 > sync_timeout * 1000 )); then
        echo "Sync timed out. Last count=$last_count" >&2
        break
    fi
    sleep 0.05
done

if [[ -n "$t_full" ]]; then
    echo "first_row=$((t_first_row - t0))ms full_sync=$((t_full - t0))ms count=$last_count interference=$interference"
else
    echo "Sync did not complete (last count $last_count / $expected, interference=$interference)"
fi

# Run the scenario with the current window geometry.
geom=$(win_geom)
read -r wx wy ww wh wfs <<<"$geom"
case "$scenario" in
    idle) sleep "$settle" ;;
    scroll)
        sleep 3
        # Click the middle of the table to focus it, then send Down keys to scroll.
        /tmp/opencode/uinput_mouse click $((wx + ww / 3)) $((wy + wh / 2)) left >/dev/null 2>&1 || true
        sleep 0.5
        args=()
        for ((i = 0; i < scroll_keys; i++)); do args+=(-k Down); done
        wtype -d 15 "${args[@]}" >/dev/null 2>&1 || true
        sleep 2
        ;;
    storm)
        sleep 3
        "$here/churn.sh" perf "$storm_rounds" >/dev/null 2>&1 || true
        sleep 45
        ;;
esac

# Read peak RSS from VmHWM, then stop the process.
if [[ -r "/proc/$app_pid/status" ]]; then
    vmhwm=$(awk '/^VmHWM/{print $2}' "/proc/$app_pid/status")
    rss_peak=${vmhwm:-$rss_peak}
fi
cleanup
trap - EXIT

# Parse the log.
frames_line=$(grep -a "^[0-9]* \[frames\] draw" "$log" | tail -1 | sed 's/^[0-9]* //' || true)
dirty_line=$(grep -a "^[0-9]* \[frames\] dirty" "$log" | tail -1 | sed 's/^[0-9]* //' || true)
input_line=$(grep -a "^[0-9]* \[frames\] input" "$log" | tail -1 | sed 's/^[0-9]* //' || true)
hang_count=$(grep -ac "^[0-9]* \[hang\] trigger" "$log" || true)
hang_events=$(grep -a "^[0-9]* \[hang\] trigger" "$log" | tail -5 || true)
final_count=$(kubectl get pods -A --no-headers | wc -l)

scenario_label="${scenario/storm/Frequent Updates}"
{
    echo "=== perf-measure $scenario_label on live cluster $context @ $(date '+%F %T') ==="
    echo "app:        $app"
    echo "app PID:    $app_pid"
    echo "app mtime:  $(stat -c '%y' "$app")"
    echo "scenario:   $scenario_label (scroll_keys=$scroll_keys)"
    echo "Test Updates: $storm_rounds rounds"
    echo "window:     ${t_window:+$((t_window - t0))ms after launch}"
    echo "first row:  ${t_first_row:+$((t_first_row - t0))ms}"
    echo "full sync:  ${t_full:+$((t_full - t0))ms}"
    echo "row count:  app=$last_count kubectl=$expected (after=$final_count)"
    echo "interference: $interference focus changes caused by another process (shared desktop)"
    echo "rss peak:   ${rss_peak}k = $((rss_peak / 1024))MB"
    echo "frames:     $frames_line"
    echo "            $dirty_line"
    echo "            $input_line"
    echo "hang events: $hang_count"
    echo "$hang_events" | sed 's/^/            /'
} | tee "$out/summary.txt"
