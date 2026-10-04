#!/usr/bin/env bash
# One serialized UI review pass over the finished rework.
#
# The screenshot harness takes a machine-wide lock for the whole run, so every
# review shot has to happen in one invocation — otherwise two runs race for the
# compositor and a shot photographs whichever window happens to be on top.
#
# Usage: scripts/uireview.sh <lane> <binary> [theme]
#   scripts/uireview.sh rev target/review/debug/k8s-app dark
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lane="${1:-rev}"
app="${2:-$repo/target/review/debug/k8s-app}"
theme="${3:-dark}"

# `secondary` is Super on Linux, which is what the keymap file binds. The
# sequences below are the ones the reader actually uses.
#
# uishot's --step parses `KEYS|name`: one pipe, so a settle sleep after a
# chord is its own `sleep:N` step rather than a middle field the parser
# silently drops (which is what `chord|sleep:2|name` did — the shot fired
# right after the chord, and half of them photographed a surface that had
# not finished opening).
"$repo/scripts/uishot.sh" \
    --lane "$lane" \
    --app "$app" \
    --theme "$theme" \
    --wait 5 \
    --out "/tmp/k8s-shots/$lane-final" \
    --step "sleep:1|01-pods" \
    --step "super+shift+m|~" \
    --step "sleep:2|02-namespace-switcher" \
    --step "-k escape|03-switcher-closed" \
    --step "super+shift+c|~" \
    --step "sleep:2|04-context-switcher" \
    --step "-k escape|05-switcher-closed" \
    --step "super+shift+p|~" \
    --step "sleep:2|06-command-palette" \
    --step "-k escape|07-palette-closed" \
    --step "-k Down|~" \
    --step "sleep:1|08-row-selected" \
    --step "super+alt+i|~" \
    --step "sleep:2|09-describe" \
    --step "super+alt+e|~" \
    --step "sleep:2|10-events" \
    --step "super+alt+f|~" \
    --step "sleep:2|11-forwards" \
    --step "super+shift+v|~" \
    --step "sleep:3|12-logs" \
    --step "super+j|~" \
    --step "sleep:1|13-dock-closed" \
    --step "super+shift+o|~" \
    --step "sleep:3|14-overview" \
    --step "super+,|~" \
    --step "sleep:4|15-settings" \
    --step "-k escape|~" \
    --step "sleep:2|16-settings-closed" \
    --step "super+alt+b|~" \
    --step "sleep:1|17-inspector-closed" \
    --step "alt-t|~" \
    --step "sleep:3|18-light" \
    --step "alt-t|~" \
    --step "sleep:3|19-dark-again"