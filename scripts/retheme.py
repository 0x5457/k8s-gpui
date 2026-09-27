#!/usr/bin/env python3
"""Rewrite the product theme's core palette onto the design's two ladders.

The product roles (`surface.*`, `fg.*`, `border.*`, `accent*`, `status.*`) are
written explicitly, because they are the decisions. The legacy Zed-schema keys
that `design.rs` and gpui-kit still read are projected from them, so the two can
never disagree about what a surface is.
"""

import json
import collections
import sys

PATH = "crates/k8s-app/assets/themes/k8s-studio.json"

# The two ladders. Dark carries height with lightness; Light carries it with a
# stroke and a shadow, because two steps of near-white on white are invisible.
LADDERS = {
    "K8s Studio Dark": {
        "surface.app": "#08090BFF",
        "surface.chrome": "#0C0D10FF",
        "surface.content": "#111216FF",
        "surface.raised": "#17181DFF",
        "surface.inset": "#050607FF",
        "surface.overlay": "#1B1D22FF",
        # White ink in dark, black ink in light. Reversed, a rule reads as a
        # highlight instead of an edge.
        "border.subtle": "#FFFFFF0F",
        "border.base": "#FFFFFF1A",
        "border.strong": "#FFFFFF29",
        "fg.primary": "#E8EAEDFF",
        "fg.secondary": "#9AA0A6FF",
        # One step lighter than the design's stated #6B7076, for the same reason
        # the light ladder is one step darker than its stated value: a rung that
        # clears the graphic floor by less than `QUIET_MIN_GAP` has nowhere to
        # put the rung below it. On the dark overlay `#6B7076` measured 3.38:1 and
        # `fg.disabled` solved to 2.98:1 there — the quietest role in the scale,
        # under its own 3:1 floor, on a surface a dialog is painted on.
        "fg.tertiary": "#70757DFF",
        "fg.disabled": "#474B50FF",
        "accent": "#4F8CFFFF",
        "accent.wash": "#4F8CFF1F",
        "accent.fg": "#FFFFFFFF",
        "status.success": "#3ECF8EFF",
        "status.warning": "#F5A524FF",
        "status.danger": "#F2555AFF",
        # Cyan, and not a blue. The accent is `#4F8CFF` — 219 degrees of hue —
        # and the fourth status channel was seeded at `#5B8DEF`, which is three
        # degrees away from it. Three degrees is not two channels, it is one
        # channel and the selection: a 6px info dot and a selected row's rail
        # were the same colour, and `UI-REDESIGN` §4.3 calls the accent scarce
        # precisely so a screen can spend it on two things. Cyan is the one hue
        # the palette left, and it is already the terminal's, so nothing new
        # enters the file.
        "status.info": "#22D3EEFF",
        "confidence.stale": "#B9944FFF",
        "confidence.unknown": "#8A97A8FF",
    },
    "K8s Studio Light": {
        "surface.app": "#F7F8FAFF",
        "surface.chrome": "#FBFBFCFF",
        "surface.content": "#FFFFFFFF",
        "surface.raised": "#FFFFFFFF",
        "surface.inset": "#F1F2F4FF",
        "surface.overlay": "#FFFFFFFF",
        "border.subtle": "#0F11140F",
        "border.base": "#0F11141A",
        "border.strong": "#0F111424",
        "fg.primary": "#101114FF",
        "fg.secondary": "#5B6069FF",
        # One step darker than the design's stated #8B9099, and it has to be.
        #
        # §1.4 gives four inks with `fg.disabled` the quietest, *and* holds every
        # one of them to a floor — 7:1 / 4.5:1 / 3:1 / and, for the quietest, 3:1
        # or there would be nothing to read. #8B9099 measures 3.21:1 on white, so
        # the rungs below it have nowhere to go: the disabled ink is pushed up to
        # 3:1 and lands *above* the tertiary, and two roles become one.
        #
        # The floor wins and the value moves, which is what the derivation layer
        # does to every other role. #848992 measures 3.52:1 and leaves the step.
        #
        # It did not, in fact, leave *enough* step. The derivation holds a role a
        # `QUIET_MAX_RATIO` quieter than the one above it, on all six surfaces, so
        # `fg.tertiary` has to clear the disabled ink's 3:1 floor by more than that
        # ratio on the *worst* of the six — or there is nowhere for `fg.disabled`
        # to stand. At #848992 the worst was `surface.inset` at 3.14:1 and the
        # disabled ink solved to 2.87:1 there and 2.98:1 on the dark overlay: the
        # quietest role in the scale, under its own floor, on a surface the app
        # paints.
        #
        # #787D86 measures 3.69:1 on that surface, which is the smallest step on
        # this ladder that leaves a band wider than the solver's own 1/256
        # lightness walk — and the pair it produces (4.13:1 against ~3.3:1) is a
        # step a reader can see, which 3.91 against 3.85 was not.
        "fg.tertiary": "#787D86FF",
        "fg.disabled": "#B4B8BEFF",
        "accent": "#2F6FEDFF",
        "accent.wash": "#2F6FED1A",
        "accent.fg": "#FFFFFFFF",
        "status.success": "#1F9D55FF",
        "status.warning": "#B45309FF",
        "status.danger": "#D92D20FF",
        # Cyan, and for the same reason as the dark rung above — except this one
        # was worse. Light's `status.info` was the accent's own value,
        # `#2F6FED`, character for character, so `status.info.wash` and
        # `accent.wash` were one hue at two alphas: an info chip and a selected
        # row were the same blue on the same surface. `UI-SPEC` §1.5 gives Light
        # a value for every other channel and says nothing at all about `info`,
        # so this is a gap being filled rather than a decision being reversed;
        # cyan is the hue the light ladder already uses for the terminal and it
        # sits 27 degrees and a quarter of the lightness range off the accent.
        "status.info": "#0E7490FF",
        # The observation-confidence channel (UI-REDESIGN §4.1). Two desaturated
        # seeds, and they are *not* the status hues: `stale` is an ochre well
        # below the warning orange, so a stale answer can never be mistaken for a
        # Pending pod, and `unknown` is a cool grey, not a channel at all.
        # `confidence::foreground` solves both against the core surfaces, which
        # is what closes the light `unknown` (4.02:1 on the light canvas).
        "confidence.stale": "#8A6400FF",
        "confidence.unknown": "#6B7887FF",
    },
}


def project(style, dark):
    """Project the product roles onto every key the two theme systems read."""
    def tint(hex_color, alpha_hex):
        return hex_color[:7] + alpha_hex

    r = style
    app, chrome = r["surface.app"], r["surface.chrome"]
    content, raised = r["surface.content"], r["surface.raised"]
    inset, overlay = r["surface.inset"], r["surface.overlay"]
    ink = "#FFFFFF" if dark else "#0F1114"
    # A 4% tint of the ink is the hover; 8% is the press. Same hue as the text
    # it sits under, so a hovered row never changes colour, only depth.
    hover = tint(ink, "0A" if dark else "09")
    press = tint(ink, "14" if dark else "12")
    # The scrollbar thumb, in three steps, as *floors* rather than as alphas.
    #
    # UI-SPEC §4.16 sets rest >= 3:1, hover >= 4.5:1 and higher than rest, and
    # active higher than hover. The old alphas here (14% / 24% / 32%) are the ones
    # the spec records as the original bug: on the canvas the thumb is actually
    # painted on they measure 1.39:1, 2.00:1 and 2.76:1 in Dark. So the alphas are
    # solved, not chosen, and they are solved against `surface.inset` — the
    # terminal canvas, and the hardest surface in both appearances (in Light it is
    # *harder* than the white content plane, because a dark ink moved a fixed
    # amount further from a darker field keeps more of its ratio). Every other
    # surface the app paints a thumb on measures higher, so one solve covers the
    # table, the sidebar, the dock and the terminal.
    #
    # Measured on `surface.inset`: Dark 3.31 / 4.93 / 6.48, Light 3.36 / 4.96 /
    # 6.50. `k8s-term`'s `the_scrollbar_thumb_is_findable_and_hover_answers_rest`
    # measures the same surface and is what holds the pair.
    thumb = ("5E", "7A", "8F") if dark else ("7D", "9C", "B0")
    r.update(
        {
            "background": app,
            "panel.background": chrome,
            "editor.background": content,
            "elevated_surface.background": raised,
            # An input is a raised surface: one step "closer" than the content it
            # is typed into, which is what tells a reader it accepts typing.
            "surface.background": raised,
            "terminal.background": inset,
            "title_bar.background": chrome,
            "title_bar.inactive_background": chrome,
            "tab_bar.background": chrome,
            "tab.inactive_background": chrome,
            "border": r["border.base"],
            "border.variant": r["border.subtle"],
            "border.focused": r["accent"],
            "border.selected": r["accent"],
            "border.disabled": r["border.subtle"],
            "border.transparent": "#00000000",
            # The stroke an *input's* border actually ends up at, which is not
            # `border.base` and cannot be `border.base`.
            #
            # gpui-kit derives a dark-appearance input's fill and border from one
            # token and then scales that token: `input_background()` is
            # `input.mix_oklab(transparent, 0.3)`, and `transparent` is
            # `Hsla::transparent_black()`, so the mix is a 30% fade toward nothing
            # and the field is left with 70% of the stroke the theme asked for.
            # `UI-SPEC` §4.7 makes the input one of the few controls that *must*
            # have a stroke, and at 70% of `border.base` it measured 1.19:1 on the
            # dark content plane — under `border::MIN_RULE_CONTRAST`, i.e. a rule
            # that does not divide.
            #
            # So this key is its own key rather than a second name for
            # `border.base`: it is the value the theme asks for *so that the
            # composited result clears the floor*, and a second name would be a
            # number that lies about what it is. 0x38 composites to 1.56:1 in Dark
            # after the fade. The light appearance gets no fade at all, so its
            # value is the plain one that clears the same floor (1.46:1).
            "input.border": tint(ink, "38" if dark else "2D"),
            "text": r["fg.primary"],
            "text.muted": r["fg.secondary"],
            "text.placeholder": r["fg.tertiary"],
            "text.disabled": r["fg.disabled"],
            "text.accent": r["accent"],
            "icon.disabled": r["fg.disabled"],
            "icon.placeholder": r["fg.tertiary"],
            "icon.accent": r["accent"],
            "element.hover": hover,
            "element.active": press,
            "element.selected": r["accent.wash"],
            "ghost_element.background": "#00000000",
            "ghost_element.hover": hover,
            "ghost_element.active": press,
            "ghost_element.selected": r["accent.wash"],
            "ghost_element.disabled": press,
            "element.selection_background": r["accent.wash"],
            "drop_target.background": r["accent.wash"],
            "drop_target.border": r["accent"],
            "panel.focused_border": r["accent"],
            "pane.focused_border": r["accent"],
            "pane_group.border": r["border.subtle"],
            "panel.overlay.background": overlay,
            "editor.foreground": r["fg.primary"],
            "editor.gutter.background": content,
            "editor.subheader.background": content,
            # An indent guide is a structural hairline, so it is a tint of the
            # same ink the dividers are — not a colour of its own. The active
            # guide is a *position* cue, so it takes the accent at the same
            # fraction the cursor line's own wash does.
            "panel.indent_guide": tint(ink, "26"),
            "panel.indent_guide_hover": tint(ink, "40"),
            "panel.indent_guide_active": tint(r["accent"][:7], "66"),
            # The cursor line is a *position* cue, not a selection. Projecting the accent
            # wash onto it made it indistinguishable from a text selection, which is the one
            # pair of editor washes that has to be told apart.
            "editor.active_line.background": tint(ink, "0A" if dark else "08"),
            "editor.highlighted_line.background": content,
            "editor.invisible": r["fg.disabled"],
            "editor.wrap_guide": tint(ink, "14" if dark else "0D"),
            "editor.active_wrap_guide": tint(ink, "1F" if dark else "14"),
            "scrollbar.thumb.background": tint(ink, thumb[0]),
            "scrollbar.thumb.hover_background": tint(ink, thumb[1]),
            "scrollbar.thumb.active_background": tint(ink, thumb[2]),
            "scrollbar.thumb.border": "#00000000",
            "scrollbar.track.background": "#00000000",
            "scrollbar.track.border": "#00000000",
            "link_text.hover": r["accent"],
            "search.match_background": tint(r["status.warning"][:7], "40" if dark else "38"),
            "search.active_match_background": tint(r["accent"][:7], "4D"),
            "minimap.thumb.border": "#00000000",
        }
    )

    # The product role names, spelled out, plus their derived washes. Done
    # before the vocabulary roles below, which borrow from them.
    for name in ("success", "warning", "danger", "info"):
        r["status.%s.wash" % name] = tint(r["status.%s" % name][:7], "1F" if dark else "14")
        r["status.%s.border" % name] = tint(r["status.%s" % name][:7], "61" if dark else "4D")

    # Status: the four channels, plus the Git-vocabulary roles that ride on them.
    #
    # There is no fifth channel for "the cluster did not answer". That state is
    # `Confidence::Unknown` — its own ink, its own shape, and its own caption — and
    # an empty panel whose API server is gone is drawn from `error`, which is what
    # `docs/mockup/index.html` screen 5 shows. A `unreachable` triplet was carried
    # here for a state nobody read, and `StatusColors::parse` documented the dead
    # key as deliberate while reading `hint` for it; the key and the comment are
    # both gone, and `theme_contract` holds the conclusion.
    channels = {
        "success": r["status.success"],
        "warning": r["status.warning"],
        "error": r["status.danger"],
        "info": r["status.info"],
    }
    # Which wash a vocabulary role wears: the channel it is really reporting.
    wash_of = {
        "success": "success",
        "warning": "warning",
        "error": "danger",
        "info": "info",
    }
    for name, color in channels.items():
        r[name] = color
        wash = wash_of[name]
        if wash == "accent":
            r[name + ".background"] = r["accent.wash"]
        elif wash == "inset":
            # Nothing to report: the quiet roles borrow the field itself rather
            # than inventing a tint that would read as a fifth channel.
            r[name + ".background"] = r["surface.app"]
        else:
            r[name + ".background"] = r["status.%s.wash" % wash]
        r[name + ".border"] = color

    # Diff: a change has to read as a change and nothing else may.
    r["editor.diff_hunk.added.background"] = tint(r["status.success"][:7], "26" if dark else "1A")
    r["editor.diff_hunk.added.hollow_background"] = tint(r["status.success"][:7], "0D")
    r["editor.diff_hunk.added.hollow_border"] = tint(r["status.success"][:7], "4D")
    r["editor.diff_hunk.deleted.background"] = tint(r["status.danger"][:7], "26" if dark else "1A")
    r["editor.diff_hunk.deleted.hollow_background"] = tint(r["status.danger"][:7], "0D")
    r["editor.diff_hunk.deleted.hollow_border"] = tint(r["status.danger"][:7], "4D")
    r["editor.document_highlight.read_background"] = tint(r["accent"][:7], "1A")
    r["editor.document_highlight.write_background"] = tint(r["status.warning"][:7], "1A")
    r["editor.document_highlight.bracket_background"] = tint(r["accent"][:7], "1A")
    r["version_control.added"] = r["status.success"]
    r["version_control.modified"] = r["status.warning"]
    r["version_control.deleted"] = r["status.danger"]
    r["version_control.renamed"] = r["accent"]
    r["version_control.conflict"] = r["status.danger"]
    r["version_control.ignored"] = r["fg.tertiary"]

    # Terminal ink reads off the content surface; the terminal canvas is the
    # inset step so a log or a shell recedes instead of competing with the table.
    r["terminal.foreground"] = r["fg.secondary"]
    r["terminal.bright_foreground"] = r["fg.primary"]
    r["terminal.dim_foreground"] = r["fg.tertiary"]
    r["terminal.ansi.background"] = inset
    # The terminal's own cursor and selection. A cursor marks the insertion
    # point, which is a position, and the product's one position colour is the
    # accent; the selection behind it is the same accent as a wash. These are the
    # last two keys in the file that no reader had claimed, and they were the two
    # with the most room to drift from the ladder.
    r["players"] = [
        {
            "cursor": r["accent"],
            "background": r["accent"],
            "selection": r["accent.wash"],
        }
    ]
    ansi = {
        "black": r["fg.secondary"],
        "red": r["status.danger"],
        "green": r["status.success"],
        "yellow": r["status.warning"],
        "blue": r["accent"],
        "magenta": "#C084FCFF" if dark else "#9333EAFF",
        "cyan": "#22D3EEFF" if dark else "#0E7490FF",
        "white": r["fg.tertiary"],
    }
    for slot, value in ansi.items():
        r["terminal.ansi." + slot] = value
        r["terminal.ansi.bright_" + slot] = ansi[slot]
        r["terminal.ansi.dim_" + slot] = value

    # Keys this generator used to write and no code reads. Dropped rather than
    # left in the file, because a key the theme carries and the code never reads
    # is a key the next person will believe is live.
    for dead in DEAD_KEYS:
        r.pop(dead, None)


# `icon` and `icon.muted` were half of an unfinished migration off a colourless
# icon: `ThemeColors` reads `icon.disabled`, `icon.placeholder` and `icon.accent`,
# and nothing anywhere reads the other two, so they were a fourth and fifth
# spelling of `fg.primary` and `fg.secondary`. `unreachable` is in the note on the
# status channel above.
DEAD_KEYS = (
    "icon",
    "icon.muted",
    "unreachable",
    "unreachable.background",
    "unreachable.border",
)


def main():
    with open(PATH) as handle:
        document = json.load(handle, object_pairs_hook=collections.OrderedDict)
    for theme in document["themes"]:
        name = theme["name"]
        if name not in LADDERS:
            continue
        style = theme["style"]
        for key, value in LADDERS[name].items():
            style[key] = value
        project(style, name == "K8s Studio Dark")
    with open(PATH, "w") as handle:
        json.dump(document, handle, indent=2)
        handle.write("\n")
    print("rewrote", PATH)


if __name__ == "__main__":
    main()
