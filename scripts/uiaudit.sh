#!/usr/bin/env bash
# Audits the view layer against the UI brief's hard rules.
#
# Every check here is a rule a design system cannot enforce for you: a raw colour
# in view code, a spacing value that is not on the scale, a shadow on something
# that is not a floating overlay. The token layer enforces the first two by
# convention only, so they get grepped instead.
#
# Test modules are cut before the grep: a token quoted in an assertion is not a
# violation of anything, and a 7000-line test module buries the real hits.
#
# Usage: scripts/uiaudit.sh            # the whole view layer
#        scripts/uiaudit.sh HEAD       # only what this change touched
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

view=(
    crates/k8s-ui/src/shell
    crates/k8s-ui/src/panels
    crates/k8s-ui/src/table_view
    crates/k8s-ui/src/charts
    crates/k8s-ui/src/yaml_editor
    crates/k8s-ui/src/design.rs
    crates/k8s-ui/src/panels/common.rs
    crates/k8s-app/src
)

if [[ $# -ge 1 ]]; then
    mapfile -t files < <(git diff --name-only "$1" -- crates/ | grep '\.rs$' || true)
else
    mapfile -t files < <(find "${view[@]}" -name '*.rs' 2>/dev/null | sort)
fi

# One line-numbered stream of everything that is not a test module, so a hit can
# still be found in its file.
stream() {
    local f
    for f in "${files[@]}"; do
        [[ -f "$f" ]] || continue
        # A `tests.rs` / `*_tests.rs` is a `#[cfg(test)] mod` in its own file, so
        # it has no in-file marker to cut at.
        case "$f" in
            */tests.rs | *_tests.rs) continue ;;
        esac
        awk -v f="$f" '/#\[cfg\(test\)\]/{exit} {printf "%s:%d:%s\n", f, NR, $0}' "$f"
    done
}
cache="$(mktemp)"
trap 'rm -f "$cache"' EXIT
stream > "$cache"

echo "auditing ${#files[@]} file(s), $(wc -l < "$cache") lines of view code"

report() {
    local label="$1" pattern="$2" why="$3"
    local hits
    hits="$(grep -E "$pattern" "$cache" \
        | grep -vE ':[0-9]+: *//' \
        || true)"
    [[ -z "$hits" ]] && return 0
    echo
    echo "-- $label"
    echo "   $why"
    printf '%s\n' "$hits" | cut -c1-150 | head -40
    local n
    n="$(printf '%s\n' "$hits" | wc -l)"
    [[ "$n" -gt 40 ]] && echo "   ... and $((n - 40)) more"
}

# 1. Raw colour literals in view code. The theme file and the token layer are the
#    only places a hex, an rgb() or an hsla() belongs.
report 'raw colour literal in view code' \
    'rgba?\(|hsla?\(|0x[0-9a-fA-F]{8}|["'"'"']#[0-9a-fA-F]{6,8}' \
    'resolve through design::role::* / design::colors instead'

# 2. Ad-hoc spacing: a px() that is not a design:: constant and is not a
#    documented physical boundary. `px(0.)` is excluded because it is not a
#    spacing decision at all — it is the "let this shrink" idiom, and it appears
#    on every min_w/min_h in the crate.
report 'off-scale px() literal' \
    '\b(px|relative)\(([1-9][0-9]*\.?[0-9]*)\)[[:space:]]*\)' \
    'use design::space::*, design::radius::*, design::size::*, design::text::*'

# 3. Shadows outside the overlay set.
report 'shadow outside an overlay role' \
    'shadow_(sm|md|lg|xl|2xl|inner)\(' \
    'shadows belong to role::shadow::{popover,overlay,toast}; a card, panel, row or header must not cast one'

# 4. The primary button variant, the scarcest signal in the product.
report 'primary button variant' \
    '\.primary\(\)' \
    'primary is the one explicit commit in a decision area — not a toolbar action, an empty-state retry or decoration'

# 5. Gradients. This product has no gradient token.
report 'gradient' \
    'bg_(linear|radial)_gradient\(|text_linear_gradient\(|LinearGradient|RadialGradient' \
    'hierarchy comes from surface steps and hairlines'

# 6. Tracking outside the eyebrow/column-header case.
report 'letter spacing' \
    'letter_spacing\(|tracking_(tight|wide|wider|widest)\(' \
    'tracking belongs to the eyebrow and column-header case only'

# 7. Full-caps body copy.
report 'uppercase transform' \
    'uppercase\(\)|to_uppercase\(\)' \
    'an uppercase word inside a sentence reads as shouting'

echo
echo "-- done"