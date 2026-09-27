//! Design tokens and semantic mappings for dimensions, typography, status, and resource icons.
//!
//! View code uses theme colors and shared tokens instead of raw colors or one-off spacing.

use gpui_kit::assets::IconName;
use gpui_kit::{App, Hsla, Pixels, SharedString};

// ── Theme bridge ─────────────────────────────────────────────────────────────
//
// gpui-kit's theme is shadcn-based and carries none of the Zed-style roles the
// design tokens below are written against. The product theme file
// (`k8s-studio.json`) is the single source of those roles: it is parsed once
// into [`ThemeColors`] / [`StatusColors`] per appearance, and every token
// function reads the active pair through [`colors`] / [`status_colors`].

/// The product theme file, compiled into the binary.
pub const PRODUCT_THEME_JSON: &str = include_str!("../../k8s-app/assets/themes/k8s-studio.json");

/// Keys the product theme must not carry, because nothing reads them.
///
/// `scripts/retheme.py` drops exactly this list when it regenerates the file, and
/// `the_theme_file_names_exactly_the_keys_the_app_reads` asserts it, so a key
/// cannot come back by being written by hand into a generated file.
///
/// `icon` and `icon.muted` were the first half of a migration off a colourless
/// icon: `ThemeColors` reads `icon.disabled`, `icon.placeholder` and
/// `icon.accent`, so the other two were a fourth and a fifth spelling of
/// `fg.primary` and `fg.secondary`. `unreachable` named a state the app does not
/// have a channel for — see `theme_contract`. `syntax` carried thirty-eight
/// tokens per appearance for a highlighter that is not written: the YAML editor
/// colours search matches and parse errors, and nothing else.
///
/// The thirty-six skin channels were each parsed, each refined for contrast, and
/// each read by nothing: the nine vocabulary statuses (a created or renamed
/// object, a predictive rollout) and their two washes each, plus nine surfaces
/// and editor inks. They are the reason this list exists in this form — a colour
/// that only a `pub` struct field holds is invisible to `dead_code`, because a
/// write counts as a use.
#[cfg(test)]
const DEAD_THEME_KEYS: [&str; 43] = [
    "&str; 24] = [",
    "conflict",
    "conflict.background",
    "conflict.border",
    "created",
    "created.background",
    "created.border",
    "deleted",
    "deleted.background",
    "deleted.border",
    "editor.active_line_number",
    "editor.code_lens.foreground",
    "editor.hover_line_number",
    "editor.line_number",
    "element.background",
    "element.disabled",
    "hidden",
    "hidden.background",
    "hidden.border",
    "hint",
    "hint.background",
    "hint.border",
    "icon",
    "icon.muted",
    "ignored",
    "ignored.background",
    "ignored.border",
    "modified",
    "modified.background",
    "modified.border",
    "panel.overlay.hover",
    "predictive",
    "predictive.background",
    "predictive.border",
    "renamed",
    "renamed.background",
    "renamed.border",
    "syntax",
    "tab.active_background",
    "toolbar.background",
    "unreachable",
    "unreachable.background",
    "unreachable.border",
];

/// Theme colors in the roles the design tokens read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeColors {
    pub background: Hsla,
    pub panel_background: Hsla,
    pub editor_background: Hsla,
    pub surface_background: Hsla,
    pub elevated_surface_background: Hsla,
    pub element_selected: Hsla,
    pub element_hover: Hsla,
    pub element_active: Hsla,
    pub border_focused: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_placeholder: Hsla,
    pub text_disabled: Hsla,
    pub text_accent: Hsla,
    /// The seed ink for [`Confidence::Unknown`]: "I could not reach a verdict".
    ///
    /// Named by the theme rather than seeded in code, because a channel a skin is
    /// meant to own cannot be spelled as a Rust constant — see
    /// [`confidence::foreground`].
    pub confidence_unknown: Hsla,
    /// The seed ink for [`Confidence::Stale`]: "the answer is older than the
    /// refresh interval".
    pub confidence_stale: Hsla,
    pub editor_active_line_background: Hsla,
    pub element_selection_background: Hsla,
    pub search_match_background: Hsla,
    pub search_active_match_background: Hsla,
    pub terminal_background: Hsla,
    pub tab_bar_background: Hsla,
    pub title_bar_background: Hsla,
    pub title_bar_inactive_background: Hsla,
    pub editor_gutter_background: Hsla,
    pub editor_subheader_background: Hsla,
    pub editor_highlighted_line_background: Hsla,
    pub panel_overlay_background: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub border_disabled: Hsla,
    pub panel_focused_border: Hsla,
    pub pane_focused_border: Hsla,
    pub pane_group_border: Hsla,
    pub scrollbar_thumb_border: Hsla,
    pub scrollbar_track_border: Hsla,
    pub minimap_thumb_border: Hsla,
    pub icon_disabled: Hsla,
    pub icon_placeholder: Hsla,
    pub icon_accent: Hsla,
    pub debugger_accent: Hsla,
    pub drop_target_border: Hsla,
    pub border_selected: Hsla,
    pub editor_foreground: Hsla,
    pub terminal_foreground: Hsla,
    pub terminal_bright_foreground: Hsla,
    pub terminal_dim_foreground: Hsla,
}

/// Status colors in the roles the design tokens read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusColors {
    pub success: Hsla,
    pub success_background: Hsla,
    pub success_border: Hsla,
    pub warning: Hsla,
    pub warning_background: Hsla,
    pub warning_border: Hsla,
    pub error: Hsla,
    pub error_background: Hsla,
    pub error_border: Hsla,
    pub info: Hsla,
    pub info_background: Hsla,
    pub info_border: Hsla,
}

/// Which appearance the active theme was parsed from.
///
/// The app follows the system appearance, so this enum is not where the
/// *product's* default lives. It is where the *fallback* is: a reader that asks
/// for a colour before anything has chosen an appearance — a test, a component
/// constructed before `set_appearance` runs — gets the `#[default]` variant, and
/// `ThemeBridge::parse` reaches `Dark` for a theme file that names no appearance,
/// so the derive and the parse agree about what an un-chosen theme is.
///
/// Dark, and dark for one reason: a desktop client asked for a colour before it
/// has an appearance should not open a white window. The system appearance wins
/// the moment it is known — `theme::set_mode` calls [`set_appearance`] — so this
/// is the colour of the frames before the first frame, not a preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    Light,
    #[default]
    Dark,
}

// ── Semantic role layer ──────────────────────────────────────────────────────
//
// Layer 1 is the skin: whatever tokens a theme file happens to carry. Layer 2 is
// this struct — the twenty named roles the view code is allowed to read — and
// layer 3 is component state, which is a role plus a state, never a new colour.
//
// The rule that makes the skin system worth keeping is that no role can be
// missing. A role resolves from the theme's own key when it has one and is
// otherwise *derived* from a role that is present, so a theme that names nothing
// still renders the design instead of rendering transparent black. The failure
// this replaces is [`color_of`], where an absent key became `Hsla::default()` —
// transparent black, no warning, and an element that simply was not there.
//
// It used to say "all thirteen shipped themes" here, and that was true of an
// earlier architecture: a Zed-schema theme set with a dozen skins beside the
// product's own. The product now ships *two* appearances out of one file, and
// `k8s-ui/theme.json` — which still carries the typography and accessibility
// contract the floors are quoted from — carries no themes at all. The layer is
// still load-bearing, because a theme that names nothing still has to render
// (`a_theme_with_no_product_roles_still_resolves_every_role`), but the number
// was a number from a previous repository.

/// The product's semantic roles, one value per name, for one appearance.
///
/// Nothing outside this module constructs one: `Roles::parse` is the only
/// constructor, and it always fills every field, so a role that reads
/// transparent black is a parse bug rather than a missing token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Roles {
    // SURFACE — five steps of height, plus the layer that floats above them.
    pub surface_app: Hsla,
    pub surface_chrome: Hsla,
    pub surface_content: Hsla,
    pub surface_raised: Hsla,
    pub surface_inset: Hsla,
    pub surface_overlay: Hsla,
    // FG — the four steps of ink, quietest last.
    pub fg_primary: Hsla,
    pub fg_secondary: Hsla,
    pub fg_tertiary: Hsla,
    pub fg_disabled: Hsla,
    // BORDER — the only three strokes the product draws.
    pub border_subtle: Hsla,
    pub border_base: Hsla,
    pub border_strong: Hsla,
    // ACCENT — the scarce resource: the selection, its wash, and the ink on it.
    pub accent: Hsla,
    pub accent_wash: Hsla,
    pub accent_fg: Hsla,
    // STATUS — four channels, each with a mark ink, a word ink, a wash and a
    // hairline derived from it.
    //
    // The two inks are the *same hue at two lightnesses*, which is the whole
    // difference between a red dot and a red word, and the reason they are two
    // roles rather than one: a 6px mark and a 12px label do not read at the same
    // contrast, and asking one colour to do both buys the mark's legibility at the
    // word's expense. §1.5 says `success` is "只用于点与小面积" and §4.4's status
    // cell puts that same token on a `13/500` word — which is the contradiction
    // this pair resolves. See [`STATUS_MARK_MIN_CONTRAST`].
    pub success: Hsla,
    pub success_word: Hsla,
    pub success_wash: Hsla,
    pub success_border: Hsla,
    pub warning: Hsla,
    pub warning_word: Hsla,
    pub warning_wash: Hsla,
    pub warning_border: Hsla,
    pub danger: Hsla,
    pub danger_word: Hsla,
    pub danger_wash: Hsla,
    pub danger_border: Hsla,
    pub info: Hsla,
    pub info_word: Hsla,
    pub info_wash: Hsla,
    pub info_border: Hsla,
}

/// Alpha of a status wash painted behind text.
///
/// A wash is not a background the reader reads ink off by intent, it is a tint
/// that says "this is the problem" without competing with the text on it, so it
/// is a fraction of the channel's own hue rather than a second colour.
const STATUS_WASH_ALPHA: f32 = 0.12;
/// Alpha of a status hairline, which has to be findable without being loud.
const STATUS_BORDER_ALPHA: f32 = 0.38;
/// Alpha of the selection wash. The strongest accent use in the product, and
/// still only a tint: the selected row's own 2px rail is what says "selected".
const ACCENT_WASH_ALPHA: f32 = 0.12;

/// Lightness step the overlay derivation walks in, as a fraction of the full
/// range. A hundredth of the range is below a rounding step of the composite
/// these are measured against, so a finer walk could only return the same value.
const DERIVATION_STEP: f32 = 1.0 / 64.0;

/// A hair of headroom above a solved floor.
///
/// A value solved to sit *exactly* on a threshold is a value that falls off it the
/// moment a float rounds differently, so every rule and wash is solved one
/// hundredth above the floor it is held to.
const RULE_HEADROOM: f32 = 0.01;

/// Walks a colour's lightness `away` steps away from `from`, and keeps the step
/// that stands furthest from `to`.
///
/// Used for the two derivations the design calls for by name — "overlay is
/// raised, a step further from the field" and "strong border is base, a step
/// stronger" — and for nothing else. A derivation that cannot be expressed as a
/// move along one axis would be a second colour, and a second colour is a theme
/// decision, not a fallback.
fn derived_lightness(base: Hsla, from: Hsla, to: Hsla, away: f32) -> Hsla {
    let from = from.alpha(1.0);
    let to = to.alpha(1.0);
    // The direction is the one the caller asked for, not the one that points at
    // the surface. Deriving the direction from `to` walks *toward* the field
    // being escaped, which turns "one step above raised" into "one step below
    // it" and puts the floating surface underneath the one it floats over.
    let direction = if away >= 0.0 { 1.0 } else { -1.0 };
    let target = (from.l + direction * away.abs()).clamp(0.0, 1.0);
    let mut best = base.alpha(1.0);
    let mut best_contrast = calculate_contrast_ratio(best, to);
    for step in 0..=(1.0 / DERIVATION_STEP) as usize {
        let lightness = (from.l + direction * step as f32 * DERIVATION_STEP).clamp(0.0, 1.0);
        let candidate = Hsla {
            l: lightness,
            ..base
        }
        .alpha(1.0);
        let measured = calculate_contrast_ratio(candidate, to);
        if measured > best_contrast {
            best_contrast = measured;
            best = candidate;
        }
        if (lightness - target).abs() < DERIVATION_STEP / 2.0 {
            break;
        }
    }
    best
}

/// Walks a role's lightness away from `ceiling` until it clears `minimum` on
/// every one of `surfaces`, staying quieter than `ceiling` throughout.
///
/// This is the "one step below" derivation, and the reason it exists is that
/// *quieter* is a relationship: a role is not quiet in absolute terms, it is
/// quiet relative to the role above it. Reading a theme's `fg.disabled` straight
/// out gave a 1.99:1 value on a white surface — quieter than `fg.tertiary`, and
/// invisible. Starting the walk at `fg.tertiary` and moving toward the surfaces
/// guarantees both halves: it clears the floor, and it cannot rise to meet the
/// role it is supposed to sit under.
///
/// A slice rather than a fixed-size array because the surface list grew: the
/// answer to "where does a panel divider go" is three surfaces and the answer to
/// "where does ink go" is all six, and one signature could not hold both.
fn enabled_ink(preferred: Hsla, ceiling: Hsla, surfaces: &[Hsla], minimum: f32) -> Hsla {
    quieter_color_on_all(preferred, ceiling, surfaces, minimum)
}

/// Ink that reads as white or as black on `background`, whichever clears more
/// contrast.
///
/// A filled button's label is the one place the product needs a foreground that
/// is not itself a named role, because it has to work on the accent *and* on the
/// danger red, and those two do not share a polarity on every theme.
fn ink_on(background: Hsla) -> Hsla {
    let white = Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: 1.,
    };
    let black = gpui_kit::black();
    if calculate_contrast_ratio(white, background) >= calculate_contrast_ratio(black, background) {
        white
    } else {
        black
    }
}

/// Raises a translucent rule's alpha until it is visible on `surfaces`.
///
/// The design sets a stroke's *intent* as an alpha — "a six percent white line" —
/// and separately sets a floor: a rule nobody can see is not a rule. On the dark
/// content surface a 6% white line measures 1.15:1, under the 1.2:1 the contract
/// holds every rule to, which means the intent and the floor disagree. The floor
/// wins, because in the dark appearance the 1px divider is the *only* thing
/// separating two panels.
fn rule_on(surfaces: &[Hsla], ink: Hsla, requested: f32, minimum: f32) -> Hsla {
    let opaque = ink.alpha(1.0);
    if requested >= 1.0 || contrast_ratio(opaque, surfaces[0]) < minimum {
        return opaque;
    }
    // The candidate has to be measured *composited*. A 6% black rule is its own
    // colour, and judging it by its own luminance rather than by what it looks
    // like painted is how a "solved" divider ends up back at the alpha the solve
    // was supposed to raise.
    let meets = |alpha: f32| {
        let candidate = opaque.opacity(alpha);
        surfaces
            .iter()
            .all(|surface| contrast_ratio(candidate, *surface) >= minimum)
    };
    if meets(requested) {
        return opaque.opacity(requested);
    }
    // Bisection for the *smallest* alpha that clears the floor. The predicate is
    // monotone — more ink can only add contrast against a surface the ink is
    // darker than — so the answer is a boundary, and the two branches point at
    // it: a candidate that clears says "the answer is at or below this", one
    // that does not says "at or above". Running them the other way converges on
    // full opacity, which is a stroke you can see and cannot ignore, and is the
    // opposite of what a divider is for.
    let mut below = requested;
    let mut above = 1.0;
    for _ in 0..12 {
        let candidate = (below + above) / 2.0;
        if meets(candidate) {
            above = candidate;
        } else {
            below = candidate;
        }
    }
    opaque.opacity(above)
}

/// The neutral ladder a theme that says nothing falls back to.
///
/// This is the derivation layer's floor. It has to be a *ladder* and not a single
/// value, because the legacy roles it derives *from* are themselves parsed with a
/// transparent-black fallback: on a bare theme the chain `product key → legacy
/// role → nothing` ends at transparent black, and filling every hole with the
/// same grey would pass a "no role is transparent" test while rendering an
/// interface with no structure in it at all.
///
/// The values are not the product's ladder. They are what "a neutral surface
/// ladder" means, written once so that a theme carrying no product roles gets a
/// readable interface instead of a blank one. The product theme replaces every
/// one of them; so does any skin a user brings.
fn seed(appearance: Appearance) -> SeedLadder {
    match appearance {
        // Dark: lightness carries height, so the ladder climbs.
        Appearance::Dark => SeedLadder {
            inset: Hsla {
                h: 0.,
                s: 0.,
                l: 0.04,
                a: 1.,
            },
            app: Hsla {
                h: 0.,
                s: 0.,
                l: 0.07,
                a: 1.,
            },
            chrome: Hsla {
                h: 0.,
                s: 0.,
                l: 0.10,
                a: 1.,
            },
            content: Hsla {
                h: 0.,
                s: 0.,
                l: 0.13,
                a: 1.,
            },
            raised: Hsla {
                h: 0.,
                s: 0.,
                l: 0.17,
                a: 1.,
            },
            overlay: Hsla {
                h: 0.,
                s: 0.,
                l: 0.22,
                a: 1.,
            },
            primary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.90,
                a: 1.,
            },
            secondary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.66,
                a: 1.,
            },
            tertiary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.48,
                a: 1.,
            },
        },
        // Light: lightness barely separates anything, so the ladder climbs in
        // small steps and the stroke does the work — the same division of labour
        // the product's own two appearances use.
        Appearance::Light => SeedLadder {
            inset: Hsla {
                h: 0.,
                s: 0.,
                l: 0.90,
                a: 1.,
            },
            app: Hsla {
                h: 0.,
                s: 0.,
                l: 0.95,
                a: 1.,
            },
            chrome: Hsla {
                h: 0.,
                s: 0.,
                l: 0.97,
                a: 1.,
            },
            content: Hsla {
                h: 0.,
                s: 0.,
                l: 1.00,
                a: 1.,
            },
            raised: Hsla {
                h: 0.,
                s: 0.,
                l: 1.00,
                a: 1.,
            },
            overlay: Hsla {
                h: 0.,
                s: 0.,
                l: 1.00,
                a: 1.,
            },
            primary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.10,
                a: 1.,
            },
            secondary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.36,
                a: 1.,
            },
            tertiary: Hsla {
                h: 0.,
                s: 0.,
                l: 0.52,
                a: 1.,
            },
        },
    }
}

/// The lightness of a colour, as a seed step the overlay derivation can take.
fn lightness_of(color: Hsla) -> Hsla {
    Hsla {
        l: color.l,
        ..color
    }
    .alpha(1.0)
}

/// The six surfaces and three inks a bare theme falls back to.
struct SeedLadder {
    inset: Hsla,
    app: Hsla,
    chrome: Hsla,
    content: Hsla,
    raised: Hsla,
    overlay: Hsla,
    primary: Hsla,
    secondary: Hsla,
    tertiary: Hsla,
}

impl Roles {
    /// Resolves every role for one appearance.
    ///
    /// `style` is the theme's own token map, read for the product role keys it
    /// happens to carry. `colors` and `status` are the already-parsed legacy
    /// roles, which is what the derivation layer falls back to: a theme that
    /// knows nothing about the new roles still gets a complete, coherent set.
    fn parse(
        style: &serde_json::Map<String, serde_json::Value>,
        colors: &ThemeColors,
        status: &StatusColors,
        appearance: Appearance,
    ) -> Self {
        // A key that is present, a string, and not fully transparent. Anything
        // else is treated as absent, so a theme that spells a role `#00000000`
        // gets the derivation rather than an invisible element.
        let named = |key: &str| -> Option<Hsla> {
            style
                .get(key)
                .and_then(parse_hsla)
                .filter(|color| !color.is_transparent())
        };
        // A *fill* is opaque. A theme may hand the fill roles a low alpha — a
        // common way of writing "let the surface through" — and honouring it
        // would make a panel or a button that is drawn and cannot be seen. A
        // wash and a hairline are the roles that are allowed to be translucent,
        // and they are read through the same door deliberately.
        let fill = |key: &str, fallback: Hsla| named(key).unwrap_or(fallback).alpha(1.0);
        // A legacy role only counts as a fallback if it is a real colour, so the
        // chain cannot re-enter the transparent black it is meant to replace.
        let solid = |legacy: Hsla, floor: Hsla| {
            if legacy.a > 0.0 && !(legacy.l == 0.0 && legacy.s == 0.0) {
                legacy.alpha(1.0)
            } else {
                floor
            }
        };
        let seed = seed(appearance);

        // SURFACE. `surface.app` is the window field between panes, so it is the
        // legacy canvas; `content` is the table and YAML plane, the one surface
        // the eye rests on for hours.
        let surface_app = fill("surface.app", solid(colors.background, seed.app));
        let surface_chrome = fill(
            "surface.chrome",
            solid(colors.panel_background, seed.chrome),
        );
        let surface_content = fill(
            "surface.content",
            solid(colors.editor_background, seed.content),
        );
        let surface_raised = fill(
            "surface.raised",
            solid(colors.elevated_surface_background, seed.raised),
        );
        // Inset is the darkest step: the terminal and the log body, which are
        // content that recedes rather than content that is read.
        let surface_inset = fill("surface.inset", solid(colors.editor_background, seed.inset));
        // Overlay is the one surface that floats. Where a theme does not name it,
        // it is raised, one step further from the field behind it.
        let surface_overlay = fill(
            "surface.overlay",
            derived_lightness(
                solid(colors.elevated_surface_background, seed.raised),
                solid(colors.elevated_surface_background, seed.raised),
                surface_content,
                0.06,
            )
            .max(lightness_of(seed.overlay)),
        );

        // FG. Tertiary is the quietest ink there is, so it derives from
        // placeholder rather than from muted, which would put two roles on one
        // value.
        let fg_primary = fill("fg.primary", solid(colors.text, seed.primary));
        let fg_secondary = fill("fg.secondary", solid(colors.text_muted, seed.secondary));
        let fg_tertiary = fill("fg.tertiary", solid(colors.text_placeholder, seed.tertiary));
        // Disabled is the one role that is not simply read. It is the quietest
        // ink in the scale by definition, and a theme that names a value for it
        // can name one that is invisible: K8s Studio's `#B4B8BE` measures 1.99:1
        // against a white content surface, under the 3:1 the contract holds every
        // other role to.
        //
        // So the solve runs on the theme's *named* value as well as on the
        // derived one. A theme naming a role is a statement of intent, not a
        // guarantee — and this is the one role where the intent and the floor
        // disagree often enough that trusting it is not an option.
        //
        // It was missed here for a long time and the cost was concrete: a panel
        // drew a lock badge nobody could see, and a log viewer drew a timestamp
        // nobody could read.
        //
        // All six surfaces, not the three the dividers use. Three is the answer
        // to "where does a panel divider go", which is a different question: a
        // lock badge is not a divider, and the two surfaces it had never been
        // measured on were `surface.inset` — the terminal and log body — and
        // `surface.overlay`, which is what a dialog and a toast are painted on.
        // Both measured under the floor in both appearances while this solve was
        // still reading only three, and the contract test was reading the same
        // three, so the violation was invisible from both ends at once.
        let ladder = [
            surface_app,
            surface_chrome,
            surface_content,
            surface_raised,
            surface_inset,
            surface_overlay,
        ];
        let fg_disabled = enabled_ink(
            fill("fg.disabled", solid(colors.text_disabled, seed.tertiary)),
            fg_tertiary,
            &ladder,
            Floors::of(false).disabled,
        );

        // BORDER. A base stroke that no theme carries derives from the legacy
        // `border`, and the two derived steps bracket it: subtle quieter, strong
        // stronger.
        let border_base = fill("border.base", solid(colors.border, seed.primary));
        // A legacy `border` is a solid grey. The product draws strokes as
        // translucent ink so one value works on every surface it can land on, so
        // the solid becomes the ink and the alpha lives in the two steps.
        let ink = if named("border.base").is_some() {
            border_base
        } else {
            match appearance {
                Appearance::Dark => gpui_kit::white(),
                Appearance::Light => gpui_kit::black(),
            }
        };
        let request = |key: &str, default: f32| named(key).map(|color| color.a).unwrap_or(default);
        // Every surface a divider can actually be drawn against: all six.
        //
        // It was three, on the argument that three is the answer to "where does a
        // panel edge go" — the chrome above it, the content below it, and the
        // field the pane sits in — and the solve and the contract check agreed on
        // those three, so the omission was invisible from both ends at once. It
        // is the same shape as the `fg.disabled` miss two roles earlier: two
        // surfaces nobody looked at, and one of them is the terminal canvas,
        // which `panels::dock::render_terminal_pane` puts a `border.subtle` on
        // directly. A 6% white line is 1.194:1 on `surface.inset` in Dark — under
        // the floor, on the one surface where a rule is the only thing bounding
        // anything.
        //
        // All six, and the same six the contract grades, because a solver that
        // checks fewer surfaces than the check that grades it is a solver that
        // can be graded as passing while the product is not.
        let divider_surfaces = ladder;
        // A hair of headroom above the floor, for the same reason the washes get
        // theirs: a value solved to sit *exactly* on a threshold is a value that
        // falls off it the moment a float rounds differently.
        let rule = |requested: f32, floor: f32| {
            rule_on(&divider_surfaces, ink, requested, floor + RULE_HEADROOM)
        };
        // Subtle is the anchor, and the other two are spread off it rather than
        // each solved independently.
        //
        // Solved independently they collapse: the design asks for 6% and 10%
        // white, which is 1.15:1 and 1.26:1, and pushing the first up to the
        // 1.2:1 floor lands it *on* the second. Two roles, one stroke, and a
        // component that reaches for the wrong one has no way to know. Solving
        // the floor first and stepping the other two off the result guarantees
        // the three are tellable, which is the only reason there are three.
        let border_subtle = rule(request("border.subtle", 0.06), 1.2);
        let spread = |factor: f32, floor: f32, declared: f32| {
            let floor_alpha = rule_on(&divider_surfaces, ink, 1.0, floor + RULE_HEADROOM).a;
            rule(declared.max(floor_alpha * factor), floor)
        };
        let border_base = if named("border.base").is_some() {
            // A theme that names its own base stroke keeps it, but not below the
            // step above it: a base that is quieter than the subtle is a base
            // that reads as an accidental transparency.
            rule(request("border.base", 0.10), 1.45)
        } else {
            spread(1.6, 1.45, request("border.base", 0.10))
        };
        let border_strong = if named("border.strong").is_some() {
            rule(request("border.strong", 0.16), 1.9)
        } else {
            spread(2.6, 1.9, request("border.strong", 0.16))
        };

        // ACCENT. The theme's focus colour is the accent it already has, so a
        // theme with no product accent still gets a coherent one.
        let accent = fill("accent", solid(colors.border_focused, seed.primary));
        let accent_wash = pick_wash(&named, "accent.wash", accent, ACCENT_WASH_ALPHA);
        let accent_fg = fill("accent.fg", ink_on(accent));

        // STATUS. Four channels; each wash and hairline is its own hue at a
        // fraction of an alpha, so adding a fifth channel later cannot mean
        // inventing two more colours.
        //
        // Two inks per channel, both starting from the theme's own value, so the
        // pair can never be two different channels: the *word* ink is the same
        // colour as the *mark* ink before either is solved, and the only thing
        // that separates them afterwards is the floor each one is held to.
        let channel = |name: &str, fallback: Hsla, floor: Hsla| {
            let base = fill(&format!("status.{name}"), solid(fallback, floor));
            (
                base,
                base,
                pick_wash(
                    &named,
                    &format!("status.{name}.wash"),
                    base,
                    STATUS_WASH_ALPHA,
                ),
                pick_wash(
                    &named,
                    &format!("status.{name}.border"),
                    base,
                    STATUS_BORDER_ALPHA,
                ),
            )
        };
        // The four channels are distinct even with no theme: a warning and a
        // danger that share a colour would make Failed and Pending the same
        // word, which is the one thing the status column exists to prevent.
        let channel_seed = |shift: f32| Hsla {
            h: shift,
            s: 0.55,
            l: if appearance == Appearance::Dark {
                0.68
            } else {
                0.38
            },
            a: 1.0,
        };
        let (success, success_word, success_wash, success_border) =
            channel("success", status.success, channel_seed(0.33));
        let (warning, warning_word, warning_wash, warning_border) =
            channel("warning", status.warning, channel_seed(0.09));
        let (danger, danger_word, danger_wash, danger_border) =
            channel("danger", status.error, channel_seed(0.0));
        let (info, info_word, info_wash, info_border) =
            channel("info", status.info, channel_seed(0.58));

        Self {
            surface_app,
            surface_chrome,
            surface_content,
            surface_raised,
            surface_inset,
            surface_overlay,
            fg_primary,
            fg_secondary,
            fg_tertiary,
            fg_disabled,
            border_subtle,
            border_base,
            border_strong,
            accent,
            accent_wash,
            accent_fg,
            success,
            success_word,
            success_wash,
            success_border,
            warning,
            warning_word,
            warning_wash,
            warning_border,
            danger,
            danger_word,
            danger_wash,
            danger_border,
            info,
            info_word,
            info_wash,
            info_border,
        }
    }

    /// The six surfaces of the role ladder, in the order they are named.
    ///
    /// One list, used by the refinement and by the contract, because three
    /// different lists is how a role came to sit under its own floor on two
    /// surfaces while the test that grades it was reading the other four.
    fn all_surfaces(&self) -> Vec<Hsla> {
        vec![
            self.surface_app,
            self.surface_chrome,
            self.surface_content,
            self.surface_raised,
            self.surface_inset,
            self.surface_overlay,
        ]
    }

    /// The surfaces the quietest ink is held to.
    ///
    /// The six role surfaces, and not [`Roles::text_surfaces`]' ten. The four
    /// extra ones are the three row washes and the selection wash: states the
    /// *table* paints over its content plane. A disabled lock badge, a
    /// placeholder and a separator dot are none of them a row state, and asking a
    /// role to clear its floor on top of a 20%-accent wash as well as on the plane
    /// underneath it is what made the two quietest rungs unsatisfiable: on the
    /// light selected row `fg.tertiary` lands on the 3:1 floor, and a rung a
    /// `QUIET_MAX_RATIO` step below a role that is *at* the floor cannot itself be
    /// at it.
    fn quiet_surfaces(&self) -> Vec<Hsla> {
        self.all_surfaces()
    }

    /// The surfaces a role can be read on, taken from the role layer's own values.
    ///
    /// Not [`core_surfaces`]: that is the skin's flat list, and the role layer has
    /// six steps of surface plus the three row washes and the selection wash on
    /// top. A caption in the sidebar, a cell on a hovered row and a label on the
    /// selected row are three different problems, and solving the ink against one
    /// of them and reading it on the other two is how a role ends up legible in
    /// the contract and invisible in the product.
    fn text_surfaces(&self) -> Vec<Hsla> {
        let content = self.surface_content;
        let mut surfaces = self.all_surfaces();
        surfaces.extend([
            composite_surface(content, self.accent_wash),
            composite_surface(content, self.accent.opacity(state::HOVER_ALPHA)),
            composite_surface(content, self.accent.opacity(ROW_FOCUS_ALPHA)),
            composite_surface(content, self.accent.opacity(ROW_SELECTED_ALPHA)),
        ]);
        surfaces
    }

    /// Hold every role in this set to its floor.
    ///
    /// The parse already derives every role against the *default* floors, and
    /// [`ThemeBridge::parse`] runs this once on the way out, so `refine(false)` is
    /// a no-op on the shipped appearance by construction — which is the property
    /// that makes it safe to run on every appearance change, and that
    /// `refining_with_the_setting_off_changes_nothing` holds. What the extra run
    /// buys is the surfaces the parse does not solve against — the three row
    /// washes and the selection wash — and the roles the parse reads without a
    /// solve at all: the four status channels and the accent.
    ///
    /// A theme's named value is the *starting point* and the floor is the floor,
    /// for the reason [`Roles::parse`] already gives for `fg.disabled`: naming a
    /// role is a statement of intent, and intent is not a guarantee. Nothing here
    /// ever lowers a role's contrast, so running the pass twice cannot make the
    /// interface worse than running it once.
    fn refine(&mut self, increased: bool) {
        let floors = Floors::of(increased);

        // ACCENT FIRST, because everything below is measured against a wash built
        // out of it. Solved in this order the second time as well: the first
        // version solved the inks against washes made from the accent's *pre-solve*
        // lightness, and the accent then moved lighter, which made the painted
        // selection wash lighter than the ink had been solved for — the ink cleared
        // 7:1 on a surface that was never painted and read 4.55:1 on the one that
        // was. A solve that feeds itself a value it is about to change is not a
        // solve.
        //
        // The accent is solved against every surface that is *not* built out of the
        // accent, for the reason [`accent_surfaces`] gives: the selection wash and
        // the cursor wash are the accent's own alpha, so solving the accent
        // against them hands the solver its own output. The rail is a graphic
        // held to the graphic floor on the washes instead, which is the
        // renderer's job and not the accent's.
        let accent_on = self.all_surfaces();
        self.accent = graphic_on_all(self.accent, &accent_on, floors.mark);
        // The label on a filled accent button. Re-derived *only* when the theme's
        // own choice no longer clears the floor on the accent it is printed on:
        // `accent.fg` is a decision the spec made — white on `#2F6FED` and on
        // `#4F8CFF` — and `ink_on` would flip both of them to black for the sake
        // of 0.03:1, which is a re-design, not a repair.
        if calculate_contrast_ratio(self.accent_fg, self.accent) < floors.mark {
            self.accent_fg = ink_on(self.accent);
        }
        // The selection wash keeps the theme's *alpha* and takes the solved
        // accent's hue, which is what [`pick_wash`] says a wash is: a fraction of
        // its own channel. Leaving the hue behind is how a selection rail and the
        // row under it end up two different blues.
        self.accent_wash = self.accent.opacity(self.accent_wash.a);

        let surfaces = self.text_surfaces();
        let quiet = self.quiet_surfaces();

        // INK. Solved in ladder order from the top down, so each role is measured
        // against the role it has to stay under: tertiary against secondary, and
        // disabled against tertiary, which is the one pair where "quieter" is a
        // hard requirement rather than a preference.
        self.fg_primary = adjusted_color_on_all(self.fg_primary, &surfaces, floors.primary);
        self.fg_secondary = adjusted_color_on_all(self.fg_secondary, &surfaces, floors.secondary);
        self.fg_tertiary = adjusted_color_on_all(self.fg_tertiary, &surfaces, floors.tertiary);
        // The quietest rung holds the order *per surface*, not just on the one
        // that binds. `quieter_color_on_all`'s own predicate compares the two
        // worst readings, which is enough to tell the colours apart and not
        // enough to keep them in order: the light `fg.disabled` measured the same
        // 3.74:1 as `fg.tertiary` on `surface.app` while the two were a full gap
        // apart on the plane that decided the walk. So the band is applied to every
        // surface, which is what "quieter" is supposed to mean.
        self.fg_disabled =
            quieter_color_on_all(self.fg_disabled, self.fg_tertiary, &quiet, floors.disabled);

        // BORDER. A hair of headroom over the floor, and the three steps spread
        // off the one below them rather than solved independently — the same
        // argument [`Roles::parse`] makes, and for the same reason: solved
        // independently they collapse onto one stroke.
        //
        // The ink comes from the roles rather than from the appearance, which is
        // what keeps a *tinted* rule tinted: the product draws `#0F1114` at an
        // alpha, and solving that against pure black returns a different colour
        // than the theme asked for at the same contrast. A theme whose stroke is
        // a solid grey has no tint to keep, and polarity is the only signal there
        // is — the same two cases [`Roles::parse`] reads.
        let ink = if self.border_base.a < 1.0 {
            self.border_base.alpha(1.0)
        } else if self.surface_content.l <= 0.5 {
            gpui_kit::white()
        } else {
            gpui_kit::black()
        };
        let dividers = self.all_surfaces();
        let rule =
            |requested: f32, floor: f32| rule_on(&dividers, ink, requested, floor + RULE_HEADROOM);
        self.border_subtle = rule(self.border_subtle.a, floors.rule);
        let measured = |value: Hsla| contrast_ratio(value, self.surface_content);
        self.border_base = rule(
            self.border_base.a,
            (floors.rule).max(measured(self.border_subtle) * BORDER_STEP_RATIO),
        );
        self.border_strong = rule(
            self.border_strong.a,
            (floors.rule).max(measured(self.border_base) * BORDER_STEP_RATIO),
        );

        // STATUS. Four channels, two floors, and the washes left where the theme
        // put them: a wash is a tint, and raising an ink to the floor never
        // requires moving the tint under it.
        //
        // The split is the fix. The four channels were one role each, solved
        // against the surfaces *and* the channel's own wash at the body-text
        // floor, which under Increase Contrast is 7:1. And 7:1 on a near-black
        // plane can only be bought with lightness: the solve walks HSL's `l` and
        // leaves `h` and `s` alone, so at a fixed saturation a red lifted that
        // far up is a pink. Measured, Dark `danger` went `#F2555A` → `#F36065`
        // at rest and → `#F89FA2` with Increase Contrast on — salmon where the
        // spec says red — and Light `warning` went amber → `#733506`, brown
        // where the spec says amber. The setting was *reducing* the one signal
        // it exists to strengthen: the ink got more readable and less
        // recognisable, and nothing measured recognisability.
        //
        // So the two uses are two roles. A **mark** — §4.4's 6px dot, the
        // summary strip's 3px bar, a 2px rail — is a graphic and is held to
        // [`Floors::channel_mark`], which is the graphic floor in *both* modes.
        // Under Increase Contrast that floor is 4.5:1, and the mark inks are
        // already above it, so the setting leaves them exactly where they were.
        //
        // A **word** is body text and is held to [`Floors::channel_word`], which
        // is 4.5:1 at rest and 7:1 under the setting. It solves against the
        // channel's own wash as well as the surfaces under it, because "semantic
        // ink on a semantic wash" is the combination the product ships rather than
        // a hypothetical: the table's inline error bar, the dock's follow-paused
        // note, the Overview's issue chips and a pending delete badge all put a
        // status-coloured label on a wash of the same channel. The washes are
        // low-alpha, so the composite is only a little off the surface under it —
        // which is exactly why the gap went unnoticed: a channel that clears
        // 4.5:1 on white and 4.27:1 on an 8% wash of itself looks correct in
        // every isolated measurement.
        //
        // The mark ink deliberately does *not* solve against the wash. A dot is
        // never printed on a wash of its own channel, and paying for a surface
        // that never appears is what pushed Dark `danger` one step off the spec's
        // own `#F2555A` in the first place. `UI-SPEC` §1.5 gives that value, and
        // with the wash dropped from the list it is the value that ships.
        //
        // Written out four times rather than through a closure because each call
        // reads one channel while writing another, and a closure over `self`
        // cannot be built out of eight overlapping borrows.
        let mark_surfaces = self.all_surfaces();
        let success_word_on = self.channel_surfaces(self.success_wash);
        let warning_word_on = self.channel_surfaces(self.warning_wash);
        let danger_word_on = self.channel_surfaces(self.danger_wash);
        let info_word_on = self.channel_surfaces(self.info_wash);
        self.success = adjusted_color_on_all(self.success, &mark_surfaces, floors.channel_mark);
        self.warning = adjusted_color_on_all(self.warning, &mark_surfaces, floors.channel_mark);
        self.danger = adjusted_color_on_all(self.danger, &mark_surfaces, floors.channel_mark);
        self.info = adjusted_color_on_all(self.info, &mark_surfaces, floors.channel_mark);
        self.success_word =
            adjusted_color_on_all(self.success_word, &success_word_on, floors.channel_word);
        self.warning_word =
            adjusted_color_on_all(self.warning_word, &warning_word_on, floors.channel_word);
        self.danger_word =
            adjusted_color_on_all(self.danger_word, &danger_word_on, floors.channel_word);
        self.info_word = adjusted_color_on_all(self.info_word, &info_word_on, floors.channel_word);
    }

    /// The surfaces a status channel's ink has to clear, which is every surface
    /// the ink can be read on *plus* the channel's own wash painted over the four
    /// a wash is ever laid down on.
    ///
    /// A wash is a tint, so the composite is a fifth kind of surface: not named
    /// anywhere in the role ladder and not one of the row states, and it is where
    /// the app puts a status word more often than anywhere else. The four hosts
    /// are the content plane a table's notice bar sits on, the chrome a dock note
    /// sits on, and the two raised steps a chip and a dialog sit on — the last two
    /// are the same white in the light appearance, so this costs nothing there and
    /// is the whole margin in the dark one.
    fn channel_surfaces(&self, wash: Hsla) -> Vec<Hsla> {
        let mut surfaces = self.text_surfaces();
        for base in [
            self.surface_content,
            self.surface_chrome,
            self.surface_raised,
            self.surface_overlay,
        ] {
            surfaces.push(composite_surface(base, wash));
        }
        surfaces
    }
}

/// The floors one appearance's roles are held to.
///
/// The standard set is the contract's own, role by role: body ink is 7:1 because
/// it is read for hours, secondary ink is the body-text floor because it carries
/// a healthy status word, and the two quiet rungs are the graphic floor because a
/// count and a group head are marks, not sentences.
///
/// [`Floors::increased`] raises every *ink* to 7:1 and every mark and rule to
/// 4.5:1, which is the promise `theme.json`'s own accessibility contract makes —
/// `accessibility.increase_contrast.text_min_contrast: 7.0` and
/// `graphic_min_contrast: 4.5`, read at startup by `settings.rs` — and the promise
/// `refine_theme_with_contrast` already keeps to the skin — the skin pushes
/// `text`, `text_muted`, `text_placeholder` and `text_accent` to the text floor,
/// so a role layer that left `fg.tertiary` at 4.5 would be the one half disagreeing
/// with the other about what the setting means.
///
/// `disabled` is the one ink that does *not* follow, in either mode. WCAG 1.4.3
/// exempts "text that is part of an inactive user interface component", and the
/// role's whole definition is that it is the quietest ink in the scale: raising it
/// to 7:1 under Increase Contrast would make a disabled control the loudest thing
/// in a mode whose entire purpose is that the things that matter are the readable
/// ones — and it would leave nowhere for it to sit under the role above it. It
/// still rises, to the graphic floor, and it still has to clear a gap under the
/// role above it.
struct Floors {
    primary: f32,
    secondary: f32,
    tertiary: f32,
    disabled: f32,
    mark: f32,
    rule: f32,
    /// The ink a status channel's *mark* is drawn in — a 6px dot, a 3px bar.
    ///
    /// The graphic floor in both modes, which under Increase Contrast means
    /// 4.5:1 rather than 7:1 and therefore *no movement at all*: the four mark
    /// inks already clear it, so the setting cannot spend a hue on them.
    channel_mark: f32,
    /// The ink a status *word* is drawn in, on a wash of its own channel.
    ///
    /// Body text, so it follows the setting — but it is a second role reading the
    /// same hue, not the same role at a higher floor, which is what keeps the
    /// light ladder from having to buy 7:1 with darkness.
    channel_word: f32,
}

impl Floors {
    fn of(increased: bool) -> Self {
        if increased {
            Self {
                primary: INCREASED_CONTRAST_TEXT_MIN,
                secondary: INCREASED_CONTRAST_TEXT_MIN,
                tertiary: INCREASED_CONTRAST_TEXT_MIN,
                disabled: INCREASED_CONTRAST_GRAPHIC_MIN,
                mark: INCREASED_CONTRAST_GRAPHIC_MIN,
                rule: INCREASED_CONTRAST_GRAPHIC_MIN,
                channel_mark: STATUS_MARK_MIN_CONTRAST,
                channel_word: INCREASED_CONTRAST_TEXT_MIN,
            }
        } else {
            Self {
                primary: INCREASED_CONTRAST_TEXT_MIN,
                secondary: TEXT_MIN_CONTRAST,
                tertiary: MARKER_MIN_CONTRAST,
                disabled: DISABLED_TEXT_MIN_CONTRAST,
                mark: MARKER_MIN_CONTRAST,
                rule: border::MIN_RULE_CONTRAST,
                channel_mark: STATUS_MARK_MIN_CONTRAST,
                channel_word: TEXT_MIN_CONTRAST,
            }
        }
    }
}

/// How much further apart two rules have to be, as a ratio of their measured
/// contrast on the content plane.
///
/// The contract's own threshold is 1.05 and this is where the spread comes from:
/// [`Roles::parse`] solves the standard floors to `floor + RULE_HEADROOM`, so a
/// refined step that asked for exactly 1.05 would fail on a rounding difference
/// rather than on a real collapse.
const BORDER_STEP_RATIO: f32 = 1.08;

/// A translucent role: the theme's value if it named one, the channel at the
/// documented alpha otherwise.
///
/// Split from the fill path because the two have opposite rules — a fill is
/// forced opaque, a wash is only ever a wash — and one `pick` covering both would
/// have to guess which it was doing.
fn pick_wash(named: &impl Fn(&str) -> Option<Hsla>, key: &str, channel: Hsla, alpha: f32) -> Hsla {
    named(key).unwrap_or_else(|| channel.opacity(alpha))
}

/// The default accent pool, shared by both appearances.
///
/// The K8s Studio theme defines no `accents` list, so the pool is the crate
/// default: the Tailwind step-9 brand hues, the same list the theme crate
/// shipped before the migration.
const DEFAULT_ACCENTS: [Hsla; 13] = [
    Hsla {
        h: 0.584,
        s: 1.0,
        l: 0.553,
        a: 1.0,
    }, // blue #0090FF
    Hsla {
        h: 0.062,
        s: 0.949,
        l: 0.545,
        a: 1.0,
    }, // orange #F76B15
    Hsla {
        h: 0.893,
        s: 0.598,
        l: 0.551,
        a: 1.0,
    }, // pink #D6409F
    Hsla {
        h: 0.244,
        s: 0.757,
        l: 0.661,
        a: 1.0,
    }, // lime #BDEE63
    Hsla {
        h: 0.761,
        s: 0.522,
        l: 0.471,
        a: 1.0,
    }, // purple #8E4EC6
    Hsla {
        h: 0.108,
        s: 1.0,
        l: 0.618,
        a: 1.0,
    }, // amber #FFC53D
    Hsla {
        h: 0.439,
        s: 0.564,
        l: 0.375,
        a: 1.0,
    }, // jade #29A383
    Hsla {
        h: 0.042,
        s: 0.808,
        l: 0.535,
        a: 1.0,
    }, // tomato #E54D2E
    Hsla {
        h: 0.544,
        s: 0.852,
        l: 0.392,
        a: 1.0,
    }, // cyan #00A2C7
    Hsla {
        h: 0.119,
        s: 0.294,
        l: 0.576,
        a: 1.0,
    }, // gold #978365
    Hsla {
        h: 0.294,
        s: 0.571,
        l: 0.408,
        a: 1.0,
    }, // grass #46A758
    Hsla {
        h: 0.678,
        s: 0.744,
        l: 0.506,
        a: 1.0,
    }, // indigo #3E63DD
    Hsla {
        h: 0.667,
        s: 0.667,
        l: 0.553,
        a: 1.0,
    }, // iris #5B5BD6
];

/// The parsed product theme: both appearances, their status colors, the accent
/// pool, and the appearance the tokens currently read.
#[derive(Clone)]
pub struct ThemeBridge {
    appearance: Appearance,
    light: ThemeColors,
    dark: ThemeColors,
    light_status: StatusColors,
    dark_status: StatusColors,
    light_roles: Roles,
    dark_roles: Roles,
    accents: Vec<Hsla>,
}

impl gpui_kit::Global for ThemeBridge {}

impl Default for ThemeBridge {
    fn default() -> Self {
        Self::parse(PRODUCT_THEME_JSON)
    }
}

/// The parsed product theme, built once.
static DEFAULT_BRIDGE: std::sync::LazyLock<ThemeBridge> =
    std::sync::LazyLock::new(|| ThemeBridge::parse(PRODUCT_THEME_JSON));

fn parse_hsla(value: &serde_json::Value) -> Option<Hsla> {
    let text = value.as_str()?;
    let hex = text.strip_prefix('#')?;
    let rgba = u32::from_str_radix(hex, 16).ok()?;
    Some(Hsla::from(gpui_kit::rgba(rgba)))
}

fn color_of(style: &serde_json::Map<String, serde_json::Value>, key: &str) -> Hsla {
    style.get(key).and_then(parse_hsla).unwrap_or_default()
}

impl ThemeColors {
    fn parse(style: &serde_json::Map<String, serde_json::Value>) -> Self {
        Self {
            background: color_of(style, "background"),
            panel_background: color_of(style, "panel.background"),
            editor_background: color_of(style, "editor.background"),
            surface_background: color_of(style, "surface.background"),
            elevated_surface_background: color_of(style, "elevated_surface.background"),
            element_selected: color_of(style, "element.selected"),
            element_hover: color_of(style, "element.hover"),
            element_active: color_of(style, "element.active"),
            border_focused: color_of(style, "border.focused"),
            text: color_of(style, "text"),
            text_muted: color_of(style, "text.muted"),
            text_placeholder: color_of(style, "text.placeholder"),
            text_disabled: color_of(style, "text.disabled"),
            text_accent: color_of(style, "text.accent"),
            confidence_unknown: style
                .get("confidence.unknown")
                .and_then(parse_hsla)
                .unwrap_or_else(|| color_of(style, "icon.placeholder")),
            confidence_stale: style
                .get("confidence.stale")
                .and_then(parse_hsla)
                .unwrap_or_else(|| color_of(style, "text.accent")),
            editor_active_line_background: color_of(style, "editor.active_line.background"),
            element_selection_background: color_of(style, "element.selection_background"),
            search_match_background: color_of(style, "search.match_background"),
            search_active_match_background: color_of(style, "search.active_match_background"),
            terminal_background: color_of(style, "terminal.background"),
            tab_bar_background: color_of(style, "tab_bar.background"),
            title_bar_background: color_of(style, "title_bar.background"),
            title_bar_inactive_background: color_of(style, "title_bar.inactive_background"),
            editor_gutter_background: color_of(style, "editor.gutter.background"),
            editor_subheader_background: color_of(style, "editor.subheader.background"),
            editor_highlighted_line_background: color_of(
                style,
                "editor.highlighted_line.background",
            ),
            panel_overlay_background: style
                .get("panel.overlay.background")
                .and_then(parse_hsla)
                .unwrap_or_else(|| color_of(style, "surface.background")),
            border: color_of(style, "border"),
            border_variant: color_of(style, "border.variant"),
            border_disabled: color_of(style, "border.disabled"),
            panel_focused_border: color_of(style, "panel.focused_border"),
            pane_focused_border: color_of(style, "pane.focused_border"),
            pane_group_border: color_of(style, "pane_group.border"),
            scrollbar_thumb_border: color_of(style, "scrollbar.thumb.border"),
            scrollbar_track_border: color_of(style, "scrollbar.track.border"),
            minimap_thumb_border: style
                .get("minimap.thumb.border")
                .and_then(parse_hsla)
                .unwrap_or_else(|| color_of(style, "scrollbar.thumb.border")),
            icon_disabled: color_of(style, "icon.disabled"),
            icon_placeholder: color_of(style, "icon.placeholder"),
            icon_accent: color_of(style, "icon.accent"),
            debugger_accent: style
                .get("debugger.accent")
                .and_then(parse_hsla)
                .unwrap_or_else(|| color_of(style, "icon.accent")),
            drop_target_border: color_of(style, "drop_target.border"),
            border_selected: color_of(style, "border.selected"),
            editor_foreground: color_of(style, "editor.foreground"),
            terminal_foreground: color_of(style, "terminal.foreground"),
            terminal_bright_foreground: color_of(style, "terminal.bright_foreground"),
            terminal_dim_foreground: color_of(style, "terminal.dim_foreground"),
        }
    }
}

impl StatusColors {
    fn parse(style: &serde_json::Map<String, serde_json::Value>) -> Self {
        let status = |key: &str| color_of(style, key);
        Self {
            success: status("success"),
            success_background: status("success.background"),
            success_border: status("success.border"),
            warning: status("warning"),
            warning_background: status("warning.background"),
            warning_border: status("warning.border"),
            error: status("error"),
            error_background: status("error.background"),
            error_border: status("error.border"),
            info: status("info"),
            info_background: status("info.background"),
            info_border: status("info.border"),
        }
    }
}

impl ThemeBridge {
    fn parse(json: &str) -> Self {
        let file: serde_json::Value = serde_json::from_str(json).expect("product theme JSON");
        let mut light = None;
        let mut dark = None;
        let mut light_status = None;
        let mut dark_status = None;
        let mut light_roles = None;
        let mut dark_roles = None;
        for theme in file["themes"].as_array().expect("theme list") {
            let name = theme["name"].as_str().expect("theme name");
            let appearance = match name {
                "K8s Studio Light" => Appearance::Light,
                "K8s Studio Dark" => Appearance::Dark,
                _ => continue,
            };
            let style = theme["style"].as_object().expect("theme style");
            let colors = ThemeColors::parse(style);
            let status = StatusColors::parse(style);
            // The app refines the active appearance on every theme change before a
            // window reads a role, so the roles the contract grades and the roles
            // the app paints have to be the same values. Parsing without the
            // default pass left `theme_contract` reading a set the app never
            // displayed: the parse solves against the six surfaces and the pass
            // against those plus the four row washes, and the light `fg.tertiary`
            // clears 3:1 on a bare content plane and lands on 3.00:1 on a selected
            // row.
            let mut roles = Roles::parse(style, &colors, &status, appearance);
            roles.refine(false);
            if appearance == Appearance::Light {
                light = Some(colors);
                light_status = Some(status);
                light_roles = Some(roles);
            } else {
                dark = Some(colors);
                dark_status = Some(status);
                dark_roles = Some(roles);
            }
        }
        Self {
            appearance: Appearance::default(),
            light: light.expect("K8s Studio Light theme"),
            dark: dark.expect("K8s Studio Dark theme"),
            light_status: light_status.expect("K8s Studio Light status"),
            dark_status: dark_status.expect("K8s Studio Dark status"),
            light_roles: light_roles.expect("K8s Studio Light roles"),
            dark_roles: dark_roles.expect("K8s Studio Dark roles"),
            accents: DEFAULT_ACCENTS.to_vec(),
        }
    }

    fn colors(&self) -> &ThemeColors {
        match self.appearance {
            Appearance::Light => &self.light,
            Appearance::Dark => &self.dark,
        }
    }

    fn status(&self) -> &StatusColors {
        match self.appearance {
            Appearance::Light => &self.light_status,
            Appearance::Dark => &self.dark_status,
        }
    }

    fn roles(&self) -> &Roles {
        match self.appearance {
            Appearance::Light => &self.light_roles,
            Appearance::Dark => &self.dark_roles,
        }
    }

    fn accent_color(&self, index: u32) -> Hsla {
        self.accents[index as usize % self.accents.len()]
    }

    /// Refine the active appearance's skin *and* its role layer.
    ///
    /// Both halves, and the second one used to be missing. The refinement pass
    /// writes `ThemeColors` and `StatusColors` — the skin — and then stopped,
    /// while 479 call sites in the crate read `role::*` and only 72 read
    /// `colors()`. `Increase Contrast` therefore moved the colour of a table cell
    /// that no component draws, and left every surface, divider, focus rail and
    /// status channel the app actually paints exactly where it was. The comment
    /// in `theme::set_mode` claimed the opposite, which is the kind of claim that
    /// survives only because nobody checked it.
    ///
    /// [`Roles::refine`] is the second half, and it runs on the roles the skin
    /// refinement just produced, so the two halves cannot disagree about a
    /// floor: the ink that clears 7:1 on the solved washes is the ink the
    /// component reads.
    fn refine(&mut self, increased: bool) {
        let mut colors = *self.colors();
        let mut status = *self.status();
        refine_theme_with_contrast(&mut colors, &mut status, &mut self.accents, increased);
        let mut roles = *self.roles();
        roles.refine(increased);
        match self.appearance {
            Appearance::Light => {
                self.light = colors;
                self.light_status = status;
                self.light_roles = roles;
            }
            Appearance::Dark => {
                self.dark = colors;
                self.dark_status = status;
                self.dark_roles = roles;
            }
        }
    }
}

fn bridge(cx: &App) -> &ThemeBridge {
    cx.try_global::<ThemeBridge>().unwrap_or(&DEFAULT_BRIDGE)
}

/// The active theme's colors.
pub fn colors(cx: &App) -> &ThemeColors {
    bridge(cx).colors()
}

/// The active theme's status colors.
pub fn status_colors(cx: &App) -> &StatusColors {
    bridge(cx).status()
}

/// The active theme's semantic roles.
pub fn roles(cx: &App) -> &Roles {
    bridge(cx).roles()
}

/// The product roles for one appearance, read from the compiled theme.
///
/// The same values [`roles`] hands a running app, reachable without one. A theme
/// contract is a property of the file, and checking it through a live `App` would
/// mean a test that can only fail once a window exists.
pub fn product_roles(appearance: Appearance) -> &'static Roles {
    match appearance {
        Appearance::Light => &DEFAULT_BRIDGE.light_roles,
        Appearance::Dark => &DEFAULT_BRIDGE.dark_roles,
    }
}

/// The appearance the active theme was parsed from.
pub fn appearance(cx: &App) -> Appearance {
    bridge(cx).appearance
}

/// The accent hue for one series index, solved against the active theme's
/// surfaces by the caller.
pub fn accent_color(cx: &App, index: u32) -> Hsla {
    bridge(cx).accent_color(index)
}

/// Choose which appearance the design tokens read.
pub fn set_appearance(cx: &mut App, appearance: Appearance) {
    let mut bridge = cx
        .try_global::<ThemeBridge>()
        .cloned()
        .unwrap_or_else(|| DEFAULT_BRIDGE.clone());
    bridge.appearance = appearance;
    cx.set_global(bridge);
}

/// Refine the active theme in place, the way the app's startup refinement did
/// before the migration.
pub fn refine_active_theme(cx: &mut App) {
    let increased = crate::settings::increase_contrast_enabled(cx);
    let mut bridge = cx
        .try_global::<ThemeBridge>()
        .cloned()
        .unwrap_or_else(|| DEFAULT_BRIDGE.clone());
    bridge.refine(increased);
    cx.set_global(bridge);
}

/// WCAG 2.0 contrast ratio between two colours.
pub fn calculate_contrast_ratio(foreground: Hsla, background: Hsla) -> f32 {
    let luminance = |color: Hsla| -> f32 {
        let rgba: gpui_kit::Rgba = color.into();
        let linear = |component: f32| {
            if component <= 0.03928 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgba.r) + 0.7152 * linear(rgba.g) + 0.0722 * linear(rgba.b)
    };
    let (lighter, darker) = {
        let (fg, bg) = (luminance(foreground), luminance(background));
        if fg > bg { (fg, bg) } else { (bg, fg) }
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// The one place view code names a colour.
///
/// Six families, thirty roles, and no literal: `border` is a name here and a
/// value in the theme file, which is what lets a skin keep working through a
/// redesign that only ever names these — and what lets a skin that names none of
/// them still render through the derivation layer.
///
/// Layer 3 — component state — is a role plus a state and is deliberately *not*
/// here. Hover, press and selection are computed by [`state`] from the role a
/// component draws, so a component cannot invent a shade.
pub mod role {
    use super::{Severity, roles};
    use gpui_kit::{App, Hsla};

    // SURFACE — five steps of height, then the layer that floats above them.
    //
    // The ladder is deliberately shallow. Measured on the product's own dark
    // ladder, adjacent steps differ by 1.018–1.056:1, which is below anything a
    // reader can see, so these values carry the *hint* of height and the 1px
    // divider carries the boundary. A component that needs a boundary draws
    // [`border_subtle`]; one that needs a hint reads one of these.
    pub fn surface_app(cx: &App) -> Hsla {
        roles(cx).surface_app
    }
    pub fn surface_chrome(cx: &App) -> Hsla {
        roles(cx).surface_chrome
    }
    pub fn surface_content(cx: &App) -> Hsla {
        roles(cx).surface_content
    }
    pub fn surface_raised(cx: &App) -> Hsla {
        roles(cx).surface_raised
    }
    pub fn surface_inset(cx: &App) -> Hsla {
        roles(cx).surface_inset
    }
    pub fn surface_overlay(cx: &App) -> Hsla {
        roles(cx).surface_overlay
    }

    /// The scrim a modal or popover lays over the content behind it.
    ///
    /// A modal which obscures its previous context makes people lose track of the
    /// task they suspended. A scrim states the modality without needing a heavier
    /// border, and it keeps the yellow status washes behind a dialog from
    /// out-shouting the dialog.
    ///
    /// Always toward black. A scrim that lightens in dark mode lifts the content
    /// toward the dialog's own value, so the dialog stops being the brightest
    /// thing on screen and the app reads as a grey card on a grey field.
    /// Darkening works in both appearances and keeps the modal on top.
    pub fn surface_backdrop(cx: &App) -> Hsla {
        surface_backdrop_at(cx, super::BACKDROP_ALPHA)
    }

    /// The same scrim at a strength the caller chooses.
    ///
    /// [`surface_backdrop`] is right for a surface that owns the whole window and
    /// must leave nothing legible behind it. It is *too* strong for an overlay the
    /// reader is about to dismiss straight back into the session it came from:
    /// measured on this product's own dark ladder, the token's 0.45 takes the app
    /// behind it from `rgb(17,18,22)` to `rgb(6,7,8)`, which erases the toolbar,
    /// the resource tree and the current view. So the strength is an argument and
    /// the colour stays the token's: one number describes every overlay in the
    /// product, and `panels::search` and the command palette pass the same one so
    /// the two window-wide overlays dim their context identically.
    ///
    /// **Translucent, and that is the whole point.** This darkens toward black
    /// rather than compositing the theme's own canvas underneath. Compositing
    /// would return an *opaque* colour, because the canvas is opaque and
    /// `Hsla::blend` keeps the base's alpha — and an opaque "scrim" is a black
    /// sheet laid over the window, so the app behind it does not dim, it
    /// disappears. That is the loss this function exists to prevent, and no
    /// alpha value can then undo it.
    pub fn surface_backdrop_at(_cx: &App, ink: f32) -> Hsla {
        gpui_kit::black().opacity(ink)
    }

    // FG — four steps of ink, quietest last.
    pub fn fg_primary(cx: &App) -> Hsla {
        roles(cx).fg_primary
    }
    pub fn fg_secondary(cx: &App) -> Hsla {
        roles(cx).fg_secondary
    }
    pub fn fg_tertiary(cx: &App) -> Hsla {
        roles(cx).fg_tertiary
    }
    pub fn fg_disabled(cx: &App) -> Hsla {
        roles(cx).fg_disabled
    }

    // BORDER — the only three strokes the product draws: an input, an overlay,
    // and the 1px line between two panels. Nothing else earns a border.
    pub fn border_subtle(cx: &App) -> Hsla {
        roles(cx).border_subtle
    }
    pub fn border_base(cx: &App) -> Hsla {
        roles(cx).border_base
    }
    pub fn border_strong(cx: &App) -> Hsla {
        roles(cx).border_strong
    }

    // ACCENT — the scarce resource. A screen may spend it on two things: the
    // current selection, and the screen's one primary action.
    pub fn accent(cx: &App) -> Hsla {
        roles(cx).accent
    }
    pub fn accent_wash(cx: &App) -> Hsla {
        roles(cx).accent_wash
    }
    pub fn accent_fg(cx: &App) -> Hsla {
        roles(cx).accent_fg
    }

    // STATUS — four channels, and nothing decorative uses them.
    //
    // Two inks per channel. `*` is the **mark** — a 6px dot, a 3px bar, a 2px
    // rail — and `*_word` is a status *word* beside one, or a label on a wash of
    // the same channel. They are the same hue at two lightnesses, and they are
    // held to two different floors, which is what stops Increase Contrast from
    // having to buy 7:1 with a hue. See [`STATUS_MARK_MIN_CONTRAST`].
    pub fn success(cx: &App) -> Hsla {
        roles(cx).success
    }
    pub fn success_word(cx: &App) -> Hsla {
        roles(cx).success_word
    }
    pub fn success_wash(cx: &App) -> Hsla {
        roles(cx).success_wash
    }
    pub fn success_border(cx: &App) -> Hsla {
        roles(cx).success_border
    }
    pub fn warning(cx: &App) -> Hsla {
        roles(cx).warning
    }
    pub fn warning_word(cx: &App) -> Hsla {
        roles(cx).warning_word
    }
    pub fn warning_wash(cx: &App) -> Hsla {
        roles(cx).warning_wash
    }
    pub fn warning_border(cx: &App) -> Hsla {
        roles(cx).warning_border
    }
    pub fn danger(cx: &App) -> Hsla {
        roles(cx).danger
    }
    pub fn danger_word(cx: &App) -> Hsla {
        roles(cx).danger_word
    }
    pub fn danger_wash(cx: &App) -> Hsla {
        roles(cx).danger_wash
    }
    pub fn danger_border(cx: &App) -> Hsla {
        roles(cx).danger_border
    }
    pub fn info(cx: &App) -> Hsla {
        roles(cx).info
    }
    pub fn info_word(cx: &App) -> Hsla {
        roles(cx).info_word
    }
    pub fn info_wash(cx: &App) -> Hsla {
        roles(cx).info_wash
    }
    pub fn info_border(cx: &App) -> Hsla {
        roles(cx).info_border
    }

    /// The four status channels, addressed by the health channel's own severity.
    ///
    /// One mapping for the whole product, because the alternative is five
    /// components each deciding what "healthy" looks like — and they all
    /// disagreed. A healthy cluster's node count printed green on the Overview
    /// while its pod rows were grey in the table, and a running port forward
    /// wore a green check. The reader was being told two different things about
    /// the same object on the same screen.
    ///
    /// The inversion is deliberate: `Success` is *not* a colour here. It is the
    /// absence of one, and the absence of one is secondary ink — legible, present,
    /// and not a signal.
    pub fn status_for(severity: Severity, cx: &App) -> Hsla {
        match severity {
            Severity::Success => fg_secondary(cx),
            Severity::Warning => warning(cx),
            Severity::Error => danger(cx),
            Severity::Info => info(cx),
            Severity::Neutral => fg_primary(cx),
            Severity::Muted => fg_tertiary(cx),
        }
    }

    /// The same mapping, for a severity the caller is about to spell out.
    ///
    /// §4.4's status cell is the reason this exists: `6px dot + 6px gap +
    /// 13/500 word`, with the dot and the word in the same channel. Under
    /// Increase Contrast those are two different roles, and reading the word out
    /// of the mark role is what put a 7:1 solve on a colour a 6px dot wears — the
    /// solve lightened the hue until the dark `danger` measured `#F89FA2`.
    ///
    /// `Success` is grey in both maps, and for the same reason: healthy is the
    /// absence of a status channel. The two greys differ, though — the mark is
    /// `fg.tertiary` and the word is `fg.secondary`, which is what §4.4 says and
    /// the only place in the product where a healthy row spends two rungs of the
    /// ink ladder on one cell.
    pub fn status_word_for(severity: Severity, cx: &App) -> Hsla {
        match severity {
            Severity::Success => fg_secondary(cx),
            Severity::Warning => warning_word(cx),
            Severity::Error => danger_word(cx),
            Severity::Info => info_word(cx),
            Severity::Neutral => fg_primary(cx),
            Severity::Muted => fg_tertiary(cx),
        }
    }
}

/// Shadows, which exist only for the surfaces that genuinely float.
///
/// Five values, no more: a shadow on a row or a card is the single loudest
/// signal that a product was decorated rather than designed, and the light
/// appearance punishes it hardest because a light shadow on a light surface turns
/// to dirt.
pub mod shadow {
    use super::{Appearance, appearance, role};
    use gpui_kit::{App, BoxShadow, Hsla, black, px};

    fn layer(cx: &App, light: f32, dark: f32) -> Hsla {
        black().opacity(if appearance(cx) == Appearance::Light {
            light
        } else {
            dark
        })
    }

    /// A menu, a popover, a tooltip: close to the surface it sits on.
    pub fn popover(cx: &App) -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(8.), layer(cx, 0.10, 0.40)).blur_radius(px(24.)),
            BoxShadow::new(px(0.), px(1.), layer(cx, 0.06, 0.30)),
        ]
    }

    /// A palette or a dialog: further from the field, so it casts further.
    pub fn overlay(cx: &App) -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(16.), layer(cx, 0.14, 0.55)).blur_radius(px(48.)),
            BoxShadow::new(px(0.), px(2.), layer(cx, 0.08, 0.40)),
        ]
    }

    /// A toast: transient, and it has to clear the dock's edge.
    pub fn toast(cx: &App) -> Vec<BoxShadow> {
        vec![BoxShadow::new(px(0.), px(12.), layer(cx, 0.12, 0.50)).blur_radius(px(32.))]
    }

    /// The surface a shadow lands on, so a component reads the pair rather than
    /// deciding which of them it is standing on.
    pub fn host(cx: &App) -> Hsla {
        role::surface_overlay(cx)
    }
}

/// Component state — layer 3.
///
/// A state is a role plus a state and never a new colour. Three of the four are
/// *nothing* happening, which is the point: a control at rest has the same ink as
/// its neighbours, and only the two states that mean something cost a wash.
///
/// The durations are not here. They are [`motion`], and they are four numbers for
/// the whole product, because "how long does a hover take" is exactly the kind of
/// decision that goes wrong when each component gets to make it.
pub mod state {
    use super::{role, shadow};
    use gpui_kit::{App, BoxShadow, Hsla};

    /// Alpha of the hover wash, over whatever the element already sits on.
    ///
    /// Not a token of its own because it is not a colour: the same four percent
    /// of the *local* ink is the hover on a near-white row and on a near-black
    /// one. Reading it from the surface the element actually sits on is what
    /// makes it invisible-in-the-right-way in both appearances — a single fixed
    /// value reads as a tint of white on a dark row and as nothing at all on a
    /// light one.
    pub const HOVER_ALPHA: f32 = 0.04;
    /// Alpha of the press wash. A press is a hover that has been committed, so it
    /// is the same tint stepped once, not a new hue.
    pub const PRESS_ALPHA: f32 = 0.09;
    /// Alpha of the focus ring's outer glow. The ring itself is the accent; this
    /// is the `0 0 0 3px accent@18%` around it.
    pub const FOCUS_RING_ALPHA: f32 = 0.18;
    /// Alpha of the disabled wash.
    pub const DISABLED_ALPHA: f32 = 0.40;

    /// The hover wash for a control on `surface`, drawn in `ink`.
    ///
    /// Takes the ink rather than reading it, so a control that sits on a raised
    /// surface inside a selected row uses *its* ink and not the row's — the one
    /// case where a single global answer is wrong.
    pub fn hover_on(surface: Hsla, ink: Hsla) -> Hsla {
        super::composite_surface(surface, ink.opacity(HOVER_ALPHA))
    }

    /// The press wash, the same tint stepped once.
    pub fn press_on(surface: Hsla, ink: Hsla) -> Hsla {
        super::composite_surface(surface, ink.opacity(PRESS_ALPHA))
    }

    /// The hover wash for a control on `surface`, in the product's own ink.
    pub fn hover(cx: &App, surface: Hsla) -> Hsla {
        hover_on(surface, role::fg_primary(cx))
    }

    /// The press wash for a control on `surface`, in the product's own ink.
    pub fn press(cx: &App, surface: Hsla) -> Hsla {
        press_on(surface, role::fg_primary(cx))
    }

    /// The ink a disabled control's label is drawn in.
    ///
    /// The design says "opacity .40" and this is that: the element's own ink,
    /// pushed toward the surface it sits on. `role::fg_disabled` is a *named
    /// role* for content that is permanently unavailable — a placeholder, a
    /// separator dot — and using it for a temporarily disabled button is how the
    /// quietest ink in the scale ends up carrying a control.
    pub fn disabled(cx: &App, surface: Hsla) -> Hsla {
        let ink = role::fg_secondary(cx);
        super::composite_surface(surface, ink.opacity(DISABLED_ALPHA))
    }

    /// The focus ring, as a stroke and a glow.
    ///
    /// **Keyboard focus only.** A ring that appears on a click is the line between
    /// a considered interface and an amateur one: it says "you are typing" while
    /// the pointer is still in the user's hand. gpui 0.6.6 ships no
    /// `focus_visible` helper, so the *origin* has to be tracked by the region
    /// that owns the focusable elements — every interactive region carries a
    /// `keyboard_focus` cell for exactly this.
    pub fn focus_ring(cx: &App) -> (Hsla, Hsla) {
        (role::accent(cx), glow(cx))
    }

    /// The focus ring's outer glow, from the ring's own ink.
    ///
    /// Takes the hue so a component that rings something other than the accent —
    /// a destructive confirmation, say — gets its own glow rather than the
    /// accent's.
    pub fn glow_for(ring: Hsla) -> Hsla {
        ring.opacity(FOCUS_RING_ALPHA)
    }

    /// The focus ring's outer glow, for the product's accent.
    pub fn glow(cx: &App) -> Hsla {
        glow_for(role::accent(cx))
    }

    /// The surface a floating element sits on, and the shadow that lifts it.
    pub fn floating(cx: &App) -> (Hsla, Vec<BoxShadow>) {
        (shadow::host(cx), shadow::popover(cx))
    }
}

pub mod text {
    use gpui_kit::{FontWeight, Pixels, px};

    /// Regular. Body copy, table cells, the value in a key/value pair.
    pub const REGULAR: FontWeight = FontWeight::NORMAL;
    /// Medium. A table's primary field, buttons, labels.
    pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
    /// Semibold. Titles, section heads, table headers.
    pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;

    /// The one number a surface exists to communicate.
    ///
    /// Reserved for a ratio or count a reader should get before anything else,
    /// such as a cluster's ready-to-total health. Using it for a section heading
    /// would spend the strongest signal in the scale on structure.
    pub const DISPLAY: Pixels = px(28.);
    pub const DISPLAY_LINE_HEIGHT: Pixels = px(32.);

    /// Panel titles, object names, the word on a dialog button.
    pub const TITLE: Pixels = px(15.);
    pub const TITLE_LINE_HEIGHT: Pixels = px(20.);

    /// A table's primary field, and a button's label.
    pub const SUBTITLE: Pixels = px(13.);
    pub const SUBTITLE_LINE_HEIGHT: Pixels = px(18.);

    /// Running prose, and any label that is neither of the above.
    pub const BODY: Pixels = px(13.);
    pub const BODY_LINE_HEIGHT: Pixels = px(18.);

    /// A key/value key, a secondary label.
    pub const LABEL: Pixels = px(12.);
    pub const LABEL_LINE_HEIGHT: Pixels = px(16.);

    /// Section heads and table headers — tracked out and uppercased, and *only*
    /// there. An uppercase word inside a sentence reads as shouting.
    pub const CAPTION: Pixels = px(11.);
    pub const CAPTION_LINE_HEIGHT: Pixels = px(14.);

    /// The status bar, a badge, a count.
    pub const MICRO: Pixels = px(10.);
    pub const MICRO_LINE_HEIGHT: Pixels = px(12.);

    /// Monospace. A UID, an IP, a port — the short machine-shaped values.
    pub const MONO_XS: Pixels = px(11.);
    pub const MONO_XS_LINE_HEIGHT: Pixels = px(16.);

    /// Monospace. Code columns, YAML, logs.
    pub const MONO_SM: Pixels = px(12.);
    pub const MONO_SM_LINE_HEIGHT: Pixels = px(18.);
}

/// Focus roles. Keyboard focus is a position cue, not a surface wash, so it is
/// named separately from the selection roles and never borrows their colors.
pub mod focus {
    use gpui_kit::{App, Hsla};

    /// The border that replaces a transparent edge while the element holds
    /// keyboard focus. The accent, because the focus ring is one of the four
    /// places the accent is allowed to appear.
    pub fn border(cx: &App) -> Hsla {
        super::role::accent(cx)
    }
}

/// Overlay roles the editor composites over its base surface.
///
/// These are washes, not theme surfaces: the editor blends them onto the editor
/// background at paint time, so the composite is the surface the text tokens
/// actually sit on. Naming them keeps the renderer, the contrast refinement, and
/// the tests solving the same composite instead of a stand-in that happens to
/// look similar.
///
/// Every role comes in two layers: the overlay the renderer hands to a `.bg()`
/// and the surface the text ends up on. The renderer owns the overlay, so it
/// reads the overlay instead of repeating the theme lookup and the blend.
pub mod editor_wash {
    use super::{StatusColors, ThemeColors};
    use gpui_kit::{App, Hsla};

    use super::{composite_surface, increased_contrast_colors};

    /// Alpha of the inline diagnostic tint, matching the editor renderer.
    pub const DIAGNOSTIC_ALPHA: f32 = 0.16;

    /// Wash under the cursor line.
    ///
    /// A few percent of the *ink*, not the accent: the cursor line says "this is
    /// where I am", and an accent-tinted line reads as "this is selected". The
    /// value comes from the theme, which is where a skin gets to say how much of
    /// its own text it wants back as a wash.
    pub fn active_line_overlay(cx: &App) -> Hsla {
        active_line_overlay_for(super::colors(cx))
    }

    /// Wash under a line that carries an inline diagnostic.
    ///
    /// Diagnostic is a separate role from the cursor line: a diagnostic that
    /// shares the cursor wash cannot be told apart from "this is where I am".
    pub fn diagnostic_overlay(cx: &App) -> Hsla {
        diagnostic_overlay_for(super::colors(cx), super::status_colors(cx))
    }

    /// Surface the cursor-line text sits on.
    pub fn active_line(cx: &App) -> Hsla {
        composite_surface(super::role::surface_content(cx), active_line_overlay(cx))
    }

    /// Surface the diagnostic text sits on.
    pub fn diagnostic(cx: &App) -> Hsla {
        composite_surface(super::role::surface_content(cx), diagnostic_overlay(cx))
    }

    pub(crate) fn active_line_overlay_for(colors: &ThemeColors) -> Hsla {
        increased_contrast_colors(colors, colors.editor_active_line_background)
    }

    pub(crate) fn diagnostic_overlay_for(colors: &ThemeColors, status: &StatusColors) -> Hsla {
        increased_contrast_colors(colors, status.error_background.opacity(DIAGNOSTIC_ALPHA))
    }
}

pub mod text_selection {
    use gpui_kit::{App, Hsla};

    use super::composite_surface;

    pub fn background(cx: &App) -> Hsla {
        super::colors(cx).element_selection_background
    }

    pub fn foreground_on(cx: &App, base_surface: Hsla) -> Hsla {
        let colors = super::colors(cx);
        let selection = composite_surface(base_surface, background(cx));
        super::text_on_for_mode(
            selection,
            colors.text,
            colors.text_accent,
            crate::settings::increase_contrast_enabled(cx),
        )
    }

    pub fn foreground(cx: &App) -> Hsla {
        foreground_on(cx, super::role::surface_content(cx))
    }
}

pub mod search_match {
    use super::ThemeColors;
    use gpui_kit::{App, Hsla};

    use super::composite_surface;

    const MATCH_FALLBACK_ALPHA: f32 = 0.21;
    const ACTIVE_FALLBACK_ALPHA: f32 = 0.19;
    const ACTIVE_FALLBACK_ALPHA_ALT: f32 = 0.17;

    pub(crate) fn resolve_background(colors: &ThemeColors) -> Hsla {
        if colors.search_match_background == colors.element_selection_background {
            colors.text_accent.opacity(MATCH_FALLBACK_ALPHA)
        } else {
            colors.search_match_background
        }
    }

    pub(crate) fn resolve_active_background(colors: &ThemeColors) -> Hsla {
        let selection = colors.element_selection_background;
        let match_background = resolve_background(colors);
        let mut active = colors.search_active_match_background;
        if active == selection || active == match_background {
            active = colors.text_accent.opacity(ACTIVE_FALLBACK_ALPHA);
        }
        if active == selection || active == match_background {
            active = colors.text_accent.opacity(ACTIVE_FALLBACK_ALPHA_ALT);
        }
        active
    }

    pub(crate) fn refine_colors(colors: &mut ThemeColors) {
        let selection = colors.element_selection_background;
        if colors.search_match_background == selection {
            colors.search_match_background = colors.text_accent.opacity(MATCH_FALLBACK_ALPHA);
        }
        let match_background = colors.search_match_background;
        if colors.search_active_match_background == selection
            || colors.search_active_match_background == match_background
        {
            colors.search_active_match_background =
                colors.text_accent.opacity(ACTIVE_FALLBACK_ALPHA);
        }
    }

    pub fn background(cx: &App) -> Hsla {
        resolve_background(super::colors(cx))
    }

    pub fn active_background(cx: &App) -> Hsla {
        resolve_active_background(super::colors(cx))
    }

    pub fn foreground_on(cx: &App, base_surface: Hsla, active: bool) -> Hsla {
        let colors = super::colors(cx);
        let overlay = if active {
            active_background(cx)
        } else {
            background(cx)
        };
        let match_surface = composite_surface(base_surface, overlay);
        super::text_on_for_mode(
            match_surface,
            colors.text,
            colors.text_accent,
            crate::settings::increase_contrast_enabled(cx),
        )
    }

    pub fn foreground(cx: &App) -> Hsla {
        foreground_on(cx, super::role::surface_content(cx), false)
    }

    pub fn active_foreground(cx: &App) -> Hsla {
        foreground_on(cx, super::role::surface_content(cx), true)
    }
}

pub mod search {
    pub use super::search_match::{
        active_background, active_foreground, background, foreground, foreground_on,
    };
}

/// Motion budgets.
///
/// Four durations and one rule, and the rule is the important part: **motion is
/// for state, not for storytelling.** A view switch is a cut. A panel does not
/// fly in. There is no spring anywhere except a drag the user is holding, because
/// a bounce is a flourish and this product spends its polish budget on consistency
/// instead.
///
/// A restrained interface is easy to over-animate, because nothing else in it
/// moves — so the temptation is to make the one moving thing *bounce*. That is how
/// a native tool starts to feel like a web page. And every one of the four is a
/// *repeat* rather than a transition: a loading marker turning and a caret
/// blinking. A view switch, a sort and a filter commit take [`motion::INSTANT`], and a
/// fifth value would be a budget for an animation nobody spends, so a new
/// transition has to add its token here rather than inline a number.
/// A loading placeholder's opacity after a wait of `waited`, breathing on a cycle of `breathe`.
///
/// A triangle rather than a sine, because a triangle is a fraction of a modulo and a sine is a
/// call into `f32::sin`: this runs once per frame for a whole skeleton, and the shape difference
/// between the two is invisible across four percent of alpha.
///
/// The phase comes from the *wait* rather than from a per-row animation clock, and that is the
/// part that matters. gpui 0.6.6 exposes no visual-only animation loop, so the alternative is
/// every row running its own `with_animate` to move four percent of alpha - a per-frame cost in a
/// table that is virtualized precisely because 10,000 rows have to stay cheap. One value per
/// frame keeps the motion and drops the bill.
///
/// The floor and the peak are the band's own numbers and the period is the caller's, because the
/// panel and the table are deliberately not breathing in step.
const SKELETON_ALPHA: f32 = 0.05;
const SKELETON_ALPHA_PEAK: f32 = 0.09;

pub fn skeleton_alpha(waited: std::time::Duration, breathe: std::time::Duration) -> f32 {
    let cycle = waited.as_nanos() as f32 / breathe.as_nanos() as f32;
    let phase = cycle - cycle.floor();
    let swing = if phase < 0.5 {
        phase * 2.0
    } else {
        (1.0 - phase) * 2.0
    };
    SKELETON_ALPHA + (SKELETON_ALPHA_PEAK - SKELETON_ALPHA) * swing
}

pub mod motion {
    use std::time::Duration;

    /// A view switch, a sort, a filter commit. No transition at all.
    ///
    /// This is the one that gets argued about. A page that fades in looks like a
    /// web page because a web page is what fades in, and the reader pays for it
    /// every time they switch. Native tools cut.
    pub const INSTANT: Duration = Duration::from_millis(0);
    /// Hover, press, a selection settling. The workhorse.
    pub const FAST: Duration = Duration::from_millis(90);
    /// Something moving to a new place, such as a panel appearing.
    pub const NORMAL: Duration = Duration::from_millis(140);
    /// A toast arriving, and the slowest thing in the product.
    pub const SLOW: Duration = Duration::from_millis(200);

    /// One turn of the loading marker.
    ///
    /// Slow enough to read as "working" rather than "flickering", and only
    /// started while something is genuinely being waited on.
    pub const LOADING: Duration = Duration::from_millis(1_200);
}

pub mod border {
    use gpui_kit::{Pixels, px};

    pub const LINE: Pixels = px(1.);
    pub const HIT: Pixels = px(20.);
    pub const FOCUS_RAIL: Pixels = px(2.);
    pub const TABLE_FOCUS_RAIL: Pixels = px(3.);
    /// The minimum contrast an interactive boundary must reach, regardless of
    /// how quiet the resting state is. Applies to resize handles, focus rails,
    /// and anything a pointer has to find.
    pub const INTERACTIVE_MIN_CONTRAST: f32 = 3.0;

    /// The quietest a structural rule may be, as a contrast ratio against the
    /// surface it sits on.
    ///
    /// `DESIGN.md` §3.5 wants structural dividers low contrast, and low
    /// contrast is not the same as invisible: the shared table's row rules used
    /// to land at 1.02:1 on a light stripe and stop carrying any rhythm at all.
    /// There is deliberately no matching upper bound. An earlier
    /// `DIVIDER_MAX_CONTRAST` promised one and nothing maintained it — every
    /// 1px line drawn with the control-boundary role sat far above it — and a
    /// ceiling nobody enforces is worse than no ceiling, because reading the
    /// constant reads as a guarantee.
    pub const MIN_RULE_CONTRAST: f32 = 1.2;
}

/// Shared number formatting so counts read the same on every surface.
pub mod format {
    /// Formats a count with thousands separators: `1234` -> `1,234`.
    pub fn count(value: usize) -> String {
        let digits = value.to_string();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3);
        for (index, ch) in digits.chars().enumerate() {
            // `digits.len() - index` counts the digits still to the right, so it is never zero
            // inside the loop and a separator lands before every third digit from the end.
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        out
    }

    /// A count with its noun, so `0 sessions` and `1 session` both read right.
    ///
    /// Five different nouns for "how many things this list holds" and a plural
    /// form dropped in seven of ten places is what made a count look like a
    /// different fact on every surface it appeared on.
    pub fn count_with_noun(value: usize, singular: &str, plural: &str) -> String {
        let noun = if value == 1 { singular } else { plural };
        format!("{} {noun}", count(value))
    }

    /// An age in its single largest unit: `45s`, `9m`, `3h`, `6d`.
    ///
    /// One largest unit rather than a pair, because an age column is scanned
    /// down rather than read, and `6d` answers "is this stuck" where `6d 4h`
    /// only makes the reader do the arithmetic.
    pub fn age(seconds: u64) -> String {
        if seconds >= 86_400 {
            format!("{}d", seconds / 86_400)
        } else if seconds >= 3_600 {
            format!("{}h", seconds / 3_600)
        } else if seconds >= 60 {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        }
    }
}

/// The 4pt grid: nine values, and every gap in the product is one of them.
///
/// Spacing does more structural work than borders do in this design — the whole
/// point of deleting seventy percent of the strokes is that proximity has to carry
/// the grouping instead. That only works if the gaps are a *set*, because a reader
/// reads "these two are 8 apart" as "these two belong together" and reads a 7px
/// gap as noise rather than as a decision.
pub mod space {
    use gpui_kit::{Pixels, px};

    /// The gap inside a single control: icon to label, dot to word.
    pub const XXS: Pixels = px(2.);
    /// Between the parts of one thing.
    pub const XS: Pixels = px(4.);
    /// Between two things in the same group.
    pub const SM: Pixels = px(8.);
    /// Between two groups.
    pub const MD: Pixels = px(12.);
    /// A panel's own padding. Four pixels looser than a form's default, which is
    /// the whole difference between a form and a tool.
    pub const LG: Pixels = px(16.);
    /// A card's padding, when the card has room for it.
    pub const LG_PLUS: Pixels = px(20.);
    /// Between sections.
    pub const XL: Pixels = px(24.);
    /// Between the major sections of a page.
    pub const XXL: Pixels = px(32.);
    /// Between the major regions of a page.
    pub const XXXL: Pixels = px(40.);
    /// Icon to label, loose. The one non-grid gap, because an icon and its label
    /// want optical space rather than a multiple of four.
    pub const ICON: Pixels = px(6.);
}

/// Corners: five values, and the shape of the product follows from which one a
/// thing is.
///
/// The old code mixed 4, 6, 8 and 10 with no rule, which is the most reliable
/// way to make a set of components look like several people drew them. A corner
/// says what a thing *is*: a dot is round, a chip is barely round, a card and a
/// button share a value because they are the same gesture at two sizes, and the
/// palette is rounder still because it is the most separate thing on screen.
pub mod radius {
    use gpui_kit::{Pixels, px};

    /// A status dot, or a square set into another square.
    pub const XS: Pixels = px(3.);
    /// A chip, a badge, an input.
    pub const SM: Pixels = px(4.);
    /// A button, a menu row, a card.
    pub const MD: Pixels = px(6.);
    /// A popover, a floating card, a panel.
    pub const LG: Pixels = px(8.);
    /// A command palette, a dialog.
    pub const XL: Pixels = px(12.);
}

/// Fixed dimensions for desktop controls and panels.
///
/// The chrome budget is the number this module is really for. Five horizontal
/// bands used to stack up above the content — title bar, tab bar, toolbar,
/// banner, and the table's own header — 114px of it, of which almost none was
/// content. Folding the toolbar into the resource header and the status bar into
/// the sidebar brings that to 84px, and the number is worth holding because a
/// table that gets thirty pixels back is a table that shows four more rows.
pub mod size {
    use gpui_kit::{Pixels, px};
    pub const HIT_MIN: Pixels = px(20.);
    /// The default control height: a search box, a button, a compact input.
    pub const CONTROL: Pixels = px(28.);
    /// A button that is only an icon.
    ///
    /// Its own size rather than [`CONTROL`], because a toolbar has to fit its
    /// buttons in a fixed-height strip: a 28px icon button plus the 1px a focus
    /// ring reserves is 30px, and it will not fit a 28px row. The dock's close
    /// control measured exactly that.
    pub const ICON_BUTTON: Pixels = px(24.);
    /// The sidebar's filter box.
    ///
    /// A control height, and its own token because the sidebar's is transparent
    /// and borderless while every other 32px field is `surface.raised` with a
    /// 1px `border.base`. Same height, different affordance, and borrowing
    /// [`CONTROL`] for it is how a borderless box acquires a border.
    pub const FILTER_BOX: Pixels = px(32.);
    /// A sticky group heading — the sidebar's regions, a list's sections.
    ///
    /// Not [`ROW_NORMAL`]: that one is a table row, and a heading is not a row.
    /// Sharing a number with a different thing is how a heading ends up sized by
    /// a table decision.
    pub const GROUP_HEAD: Pixels = px(28.);

    // ROW is the *comfort* step of the density scale, and 32 is the default. A
    // restrained interface needs the air, and the default view is a filtered one
    // — "needs attention" is a dozen rows, not ten thousand. Scanning the full
    // set is what dense is for.
    pub const ROW: Pixels = px(32.);
    /// Dense: 24px, for the full sweep where the reader is looking for one row.
    pub const ROW_DENSE: Pixels = px(24.);
    /// Normal: 28px.
    pub const ROW_NORMAL: Pixels = px(28.);
    /// Comfortable: 32px, the default.
    pub const ROW_COMFORT: Pixels = px(32.);
    pub const TREE_ROW: Pixels = px(24.);

    /// The title bar: cluster, namespace, the connection dot, the settings gear.
    pub const TITLE_BAR: Pixels = px(40.);
    /// The view header: what you are looking at, how much of it there is, and the one
    /// action that belongs to it.
    ///
    /// **40, and the equality is load-bearing.** This band and the Overview's own toolbar are
    /// the same 40px, so every view spends the same height above its body and switching views
    /// moves nothing. At 44 it did not: a resource view spent 44 + 28 = 72px of chrome and the
    /// Overview spent 40 + 28 = 68, so opening a table from the Overview dropped the whole
    /// content plane up 4px and the table's own summary line made it read as a whole section
    /// jumping — the single most complained-about thing in a window where the content is the
    /// product. 40 is also the rhythm the Inspector's toolbars and the status-bar shell already
    /// use, so one number now governs every header band in the app.
    pub const RESOURCE_HEADER: Pixels = px(40.);
    /// The open-view strip. Only present when more than one view is open, so it
    /// costs nothing in the common case of a single list.
    pub const OPEN_VIEWS: Pixels = px(28.);
    /// A table's own header row.
    pub const TABLE_HEADER: Pixels = px(32.);
    /// The strip above a table that says what the rows add up to.
    pub const SUMMARY_STRIP: Pixels = px(32.);
    /// The bar at the bottom of a table when a range is selected.
    pub const SELECTION_BAR: Pixels = px(40.);
    /// A palette or menu row.
    pub const PALETTE_ROW: Pixels = px(32.);
    /// The dock's always-present tab strip.
    pub const DOCK_TABS: Pixels = px(28.);
    /// The dock's per-panel toolbar.
    pub const DOCK_TOOLBAR: Pixels = px(28.);

    /// The names these bands used to have.
    pub const TOOLBAR: Pixels = TITLE_BAR;
    pub const TAB_BAR: Pixels = OPEN_VIEWS;
    pub const UPDATE_STRIP: Pixels = RESOURCE_HEADER;
    pub const UPDATE_PROGRESS: Pixels = px(160.);

    /// The status bar: counts and links, and nothing else.
    pub const STATUS_BAR: Pixels = px(24.);
    /// A health or connection dot. Six pixels, so it is a mark and not a bullet.
    pub const STATUS_DOT: Pixels = px(6.);

    pub const SIDEBAR_MIN: Pixels = px(180.);
    pub const SIDEBAR_MAX: Pixels = px(360.);
    /// The sidebar's resting width, and the width it remembers per cluster.
    pub const SIDEBAR_DEFAULT: Pixels = px(236.);
    /// The collapsed sidebar: an icon rail wide enough to hit.
    pub const SIDEBAR_RAIL: Pixels = px(48.);
    /// Narrow rail for the Hotbar.
    pub const HOTBAR_RAIL: Pixels = px(40.);
    /// Hotbar slot hit area.
    pub const HOTBAR_SLOT: Pixels = px(28.);
    pub const INSPECTOR_MIN: Pixels = px(260.);
    pub const INSPECTOR_MAX: Pixels = px(480.);
    /// The inspector's resting width.
    pub const INSPECTOR_DEFAULT: Pixels = px(352.);
    /// Below this window width the inspector stops docking and floats instead.
    ///
    /// Docking it any narrower pushes the centre below the 1036px the table's
    /// default columns need, and a table that gives up two of its seven columns
    /// is worse than one that is briefly covered.
    pub const INSPECTOR_FLOAT_BELOW: f32 = 1000.;
    pub const CENTER_MIN: Pixels = px(480.);
    pub const MAIN_CONTENT_MIN: Pixels = px(280.);
    /// The dock's shortest body: its 28px tab strip, its 28px toolbar, the 28px
    /// row the chrome is measured against, and three product-default log lines.
    ///
    /// The design states 156px and, in the same section, records that 156 does
    /// not hold once the height breakpoints are applied — the body's minimum
    /// cannot be a number the height collapse ignores. This is that minimum
    /// re-derived from the chrome the dock actually has.
    pub const DOCK_MIN: Pixels = px(142.);
    pub const DOCK_MAX: Pixels = px(400.);
    /// Below this window height the dock's body collapses to nothing and only
    /// its tab strip survives, because a table is worth more than a log.
    pub const DOCK_COLLAPSE_BELOW: f32 = 760.;
    /// Icon in a control, such as a button or a tab.
    pub const ICON: Pixels = px(16.);
    /// The kind icon in the sidebar. Fourteen pixels is the size the kind set
    /// was drawn and judged at, and a family that is legible at fourteen is
    /// legible everywhere.
    pub const KIND_ICON: Pixels = px(14.);
    /// The kind icon beside a resource title, where there is room for it.
    pub const KIND_ICON_TITLE: Pixels = px(16.);
    /// The mark in a *navigation* lane: the left rail's controls, the sidebar's
    /// kind column, and the Hotbar's slots.
    ///
    /// It is [`KIND_ICON`] and not a fourth number, because the rail was mixing
    /// three optometries on one vertical spine. `Button::with_size(28)` derives
    /// its glyph from the *box* — `size * 0.75`, so twenty-one pixels — so an
    /// icon button on a `HOTBAR_SLOT` lane drew at 21 while the lane's own mark
    /// drew at [`ICON`]'s 16 and the sidebar's kind column drew at 14. A reader
    /// aiming at a 28px row could not tell from the mark how big the control
    /// was, and the rail read as three different tools stacked.
    ///
    /// Fourteen is also the right answer rather than the convenient one: it is
    /// the size the shared icon family was drawn and judged at, so a mark that
    /// moves from the sidebar's kind column onto the rail keeps its weight
    /// instead of being redrawn per lane.
    pub const NAV_MARK: Pixels = KIND_ICON;
    /// Icon that leads an empty or loading state.
    pub const ICON_LARGE: Pixels = px(24.);
    /// The status-marker family: a health glyph, a confidence mark, a severity
    /// dot, and the markers in a Describe or log row.
    pub const STATUS_MARKER: Pixels = ICON;
    /// A 2px accent rail: the selection's only unambiguous signal, and the one
    /// that survives a greyscale screenshot.
    pub const SELECTION_RAIL: Pixels = px(2.);
    /// Narrowest window the shell supports, in logical pixels.
    ///
    /// `main.rs` sets `window_min_size` and the shell decides its compact
    /// breakpoints from the same number, so every breakpoint in the design is at
    /// or above this.
    pub const WINDOW_MIN: (f32, f32) = (960., 640.);
}

/// Row height: the floor, or the configured line plus a pixel of slack.
///
/// The pixel is not decoration and not a divider reservation. A cell inside the
/// shared table is laid out one pixel shorter than the row that holds it —
/// measured, not inferred: a 36px row hands a `h_full()` cell 35px — so a row
/// sized to exactly its line crops the line by a pixel, and the crop shows as
/// text sitting a hair high in its row.
///
/// The floor is the product's default row, 32.
pub fn row_height(line_height: Pixels) -> Pixels {
    line_height.max(size::ROW).max(line_height + border::LINE)
}

/// Row height for a resource-table row.
///
/// The same rhythm as every other row, for the same reason: a table row is a
/// row, and a table is not a different kind of list. This used to add the
/// shared table's 1px row divider on top; the design deleted row dividers, and
/// the pixel they reserved is the slack [`row_height`] now carries for its own
/// sake.
pub fn table_row_height(line_height: Pixels) -> Pixels {
    row_height(line_height)
}

/// Shared status severity for tables, details, and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Success,
    Warning,
    Error,
    Info,
    Neutral,
    Muted,
}

pub const MARKER_MIN_CONTRAST: f32 = 3.0;
pub const TEXT_MIN_CONTRAST: f32 = 4.5;
pub const INCREASED_CONTRAST_TEXT_MIN: f32 = 7.0;
pub const INCREASED_CONTRAST_GRAPHIC_MIN: f32 = 4.5;
const SURFACE_MIN_CONTRAST: f32 = 1.05;

/// The floor for text on an inactive control.
///
/// WCAG 1.4.3 exempts "text that is part of an inactive user interface
/// component", so a disabled label is not body text and does not have to reach
/// `TEXT_MIN_CONTRAST`. It does have to be perceivable, and it has to be the
/// quietest of the text roles rather than a copy of `text_muted`, which is what
/// happens when every secondary role is pushed to the same floor.
pub const DISABLED_TEXT_MIN_CONTRAST: f32 = 3.0;

/// The floor for a status channel's **mark** ink — the 6px dot of §4.4's status
/// cell, the 3px segment of §4.4's summary strip, a 2px rail.
///
/// `UI-SPEC` §1.5 gives `success` one job and names it: "只用于点与小面积". Four
/// fifths of what the product does with the four channels is a dot, a bar or a
/// rail, and a mark is a graphic, so it is held to the graphic threshold — 4.5:1
/// rather than the 3:1 the ordinary marks get, because a status dot is the one
/// mark a reader scans a column *for*, and 3:1 red on a near-white plane is the
/// difference between a red dot and a smudge.
///
/// It is a floor in *both* modes, and that is the whole point of a separate
/// field rather than reusing `Floors::secondary`. Increase Contrast raises the
/// body-text floor to 7:1, and on a dark plane the only way to reach 7:1 with a
/// red is to lighten it until the red is gone: the solve walks HSL's lightness and
/// holds hue and saturation, so the dark `danger` measured `#F2555A` at rest and
/// `#F89FA2` with the setting on. The setting made the mark more readable and
/// less recognisable, and only the second of those is a regression.
///
/// §4.4's status cell then puts a `13/500` *word* in the same row, and that word
/// is a second role — `Floors::channel_word` — rather than this one at a higher
/// floor.
pub const STATUS_MARK_MIN_CONTRAST: f32 = INCREASED_CONTRAST_GRAPHIC_MIN;

/// How much quieter a secondary text role has to be than the role above it, as a
/// *fraction* of the ceiling's own contrast.
///
/// [`quieter_color_on_all`] walks lightness in 1/256 steps, so without a margin a
/// role can end up one step below its neighbour: two values, one colour. The
/// margin is a ratio rather than a fixed number of contrast points because a fixed
/// number means something different at every rung, and the two quietest rungs are
/// the two that suffer: `QUIET_MIN_GAP` of `0.4` is a tenth of the 4.5 band a
/// disabled control used to live in and a *seventh* of the 3:1 band it lives in
/// now, where the whole band between the floor and the ceiling is three solver
/// steps wide. A subtraction asked for more room than the rung had, the walk found
/// nothing, and the fallback returned a value with no margin at all — `fg.disabled`
/// and `fg.tertiary` came out 3.85:1 and 3.91:1 in Light, which is two rungs of one
/// ladder one step apart on screen.
///
/// 0.88 gives a gap of 0.54 at 4.5:1 — more than the 0.4 it replaces, so the
/// promise is a little stronger where it was written — and it is the loosest
/// value the light ladder can use at all: the band has to be
/// `tertiary x ratio - 3.02` wide on the *worst* of the six surfaces, and on
/// `surface.inset` that is 0.23 at 0.88 and empty below 0.87.
const QUIET_MAX_RATIO: f32 = 0.88;

/// How far a modal scrim pushes the content behind it toward the far end of
/// the app's value range. Enough to read as "not interactive any more", quiet
/// enough that the modal stays the brightest thing on screen.
const BACKDROP_ALPHA: f32 = 0.45;

pub fn composite_surface(background: Hsla, overlay: Hsla) -> Hsla {
    composite_background(background, overlay)
}

fn composite_background(background: Hsla, overlay: Hsla) -> Hsla {
    background.alpha(1.0).blend(overlay).alpha(1.0)
}

fn contrast_ratio(foreground: Hsla, background: Hsla) -> f32 {
    let background = background.alpha(1.0);
    let foreground = composite_surface(background, foreground);
    crate::design::calculate_contrast_ratio(foreground, background)
}

fn color_at_lightness(foreground: Hsla, lightness: f32) -> Hsla {
    Hsla {
        l: lightness.clamp(0.0, 1.0),
        ..foreground
    }
    .alpha(1.0)
}

fn meets_contrast(foreground: Hsla, backgrounds: &[Hsla], minimum: f32) -> bool {
    backgrounds
        .iter()
        .all(|background| contrast_ratio(foreground, *background) >= minimum)
}

fn worst_contrast(foreground: Hsla, backgrounds: &[Hsla]) -> f32 {
    backgrounds
        .iter()
        .map(|background| contrast_ratio(foreground, *background))
        .fold(f32::INFINITY, f32::min)
}

/// Mean lightness of a surface set, in HSL lightness.
///
/// Only used to work out which way a role has to move to get quieter, so the
/// average is enough: a theme's surfaces sit on one side of the text.
fn surface_lightness(backgrounds: &[Hsla]) -> f32 {
    backgrounds
        .iter()
        .map(|background| background.l)
        .sum::<f32>()
        / backgrounds.len() as f32
}

/// A role that has to be quieter than the role it sits below, on every surface.
///
/// Every secondary text role shares `TEXT_MIN_CONTRAST`, and
/// [`adjusted_color_on_all`] walks lightness in steps of 1/256, so pushing
/// several of them to that same floor lands them all on the same value: whoever
/// arrives first wins. A disabled control is exempt from the body-text floor
/// under WCAG 1.4.3, so it gets a floor of its own, and the walk starts at the
/// lightness of the role above it and moves *toward* the surfaces. Starting
/// there is what guarantees the two can never meet, and stopping at the first
/// value that clears the floor is what makes this one the quieter of the two.
///
/// The gap is required as well as the floor, because two roles one 1/256
/// lightness step apart are two values and one colour, which is the failure this
/// exists to fix. It is applied to *every* surface rather than to the worst
/// reading, and as a ratio of the ceiling rather than a fixed number of contrast
/// points — see [`QUIET_MAX_RATIO`] for why a fixed number cannot be right at
/// every rung, and what it cost when it was one.
///
/// When no value clears the floor with the margin, the shared floor solver takes
/// over. That is what happens under Increase Contrast, where every text role is
/// pushed to 7:1 on the same surfaces and the band of values that qualifies is
/// narrower than the gap between two roles. The default appearance is where the
/// ladder has to survive.
fn quieter_color_on_all(
    foreground: Hsla,
    ceiling: Hsla,
    backgrounds: &[Hsla],
    minimum: f32,
) -> Hsla {
    if backgrounds.is_empty() {
        return foreground.alpha(1.0);
    }
    let target = minimum + 0.02;
    let ceiling = ceiling.alpha(1.0);
    let clears_the_margin_everywhere = |candidate: Hsla| {
        backgrounds.iter().all(|background| {
            contrast_ratio(candidate, *background)
                <= contrast_ratio(ceiling, *background) * QUIET_MAX_RATIO
        })
    };
    let toward_darker = ceiling.l <= surface_lightness(backgrounds);
    for step in 1..=256 {
        let offset = step as f32 / 256.0;
        let lightness = if toward_darker {
            ceiling.l + offset
        } else {
            ceiling.l - offset
        }
        .clamp(0.0, 1.0);
        if lightness == ceiling.l {
            break;
        }
        let candidate = color_at_lightness(foreground, lightness);
        if meets_contrast(candidate, backgrounds, target) && clears_the_margin_everywhere(candidate)
        {
            return candidate;
        }
    }
    adjusted_color_on_all(foreground, backgrounds, minimum)
}

fn adjusted_color_on_all(foreground: Hsla, backgrounds: &[Hsla], minimum: f32) -> Hsla {
    if backgrounds.is_empty() {
        return foreground.alpha(1.0);
    }
    let foreground = foreground.alpha(1.0);
    let target = minimum + 0.02;
    if meets_contrast(foreground, backgrounds, target) {
        return foreground;
    }
    for step in 0..=256 {
        let offset = step as f32 / 256.0;
        for direction in [-1.0, 1.0] {
            if step == 0 && direction < 0.0 {
                continue;
            }
            let lightness = (foreground.l + direction * offset).clamp(0.0, 1.0);
            if step > 0 && lightness == foreground.l {
                continue;
            }
            let candidate = color_at_lightness(foreground, lightness);
            if meets_contrast(candidate, backgrounds, target) {
                return candidate;
            }
        }
    }
    let darker = color_at_lightness(foreground, 0.0);
    let lighter = color_at_lightness(foreground, 1.0);
    if worst_contrast(darker, backgrounds) >= worst_contrast(lighter, backgrounds) {
        darker
    } else {
        lighter
    }
}

fn color_on_all(preferred: Hsla, fallback: Hsla, backgrounds: &[Hsla], minimum: f32) -> Hsla {
    if backgrounds.is_empty() {
        return preferred.alpha(1.0);
    }
    let preferred = preferred.alpha(1.0);
    let fallback = fallback.alpha(1.0);
    if meets_contrast(preferred, backgrounds, minimum) {
        return preferred;
    }
    if meets_contrast(fallback, backgrounds, minimum) {
        return fallback;
    }
    let preferred = adjusted_color_on_all(preferred, backgrounds, minimum);
    if meets_contrast(preferred, backgrounds, minimum) {
        return preferred;
    }
    let fallback = adjusted_color_on_all(fallback, backgrounds, minimum);
    if worst_contrast(fallback, backgrounds) > worst_contrast(preferred, backgrounds) {
        fallback
    } else {
        preferred
    }
}

fn refined_graphic_on(
    preferred: Hsla,
    fallback: Hsla,
    backgrounds: &[Hsla],
    minimum: f32,
    increased: bool,
) -> Hsla {
    if increased {
        graphic_on_all(preferred, backgrounds, minimum)
    } else {
        color_on_all(preferred, fallback, backgrounds, minimum)
    }
}

pub fn text_on(background: Hsla, preferred: Hsla, fallback: Hsla) -> Hsla {
    text_on_with_minimum(background, preferred, fallback, TEXT_MIN_CONTRAST)
}

pub(crate) fn text_on_for_mode(
    background: Hsla,
    preferred: Hsla,
    fallback: Hsla,
    increased: bool,
) -> Hsla {
    let minimum = if increased {
        INCREASED_CONTRAST_TEXT_MIN
    } else {
        TEXT_MIN_CONTRAST
    };
    refined_text_on(background, preferred, fallback, minimum, increased)
}

fn text_on_with_minimum(background: Hsla, preferred: Hsla, fallback: Hsla, minimum: f32) -> Hsla {
    let background = background.alpha(1.0);
    let preferred = composite_surface(background, preferred);
    let fallback = composite_surface(background, fallback);
    color_on_all(preferred, fallback, &[background], minimum)
}

fn refined_text_on(
    background: Hsla,
    preferred: Hsla,
    fallback: Hsla,
    minimum: f32,
    increased: bool,
) -> Hsla {
    if increased {
        let background = background.alpha(1.0);
        let preferred = composite_surface(background, preferred);
        adjusted_color_on_all(preferred, &[background], minimum)
    } else {
        text_on_with_minimum(background, preferred, fallback, minimum)
    }
}

fn graphic_on_all(preferred: Hsla, backgrounds: &[Hsla], minimum: f32) -> Hsla {
    adjusted_color_on_all(preferred, backgrounds, minimum)
}

pub fn graphic_on(background: Hsla, preferred: Hsla) -> Hsla {
    graphic_on_with_minimum(background, preferred, MARKER_MIN_CONTRAST)
}

/// Graphic token solved against an explicit threshold.
///
/// The result is opaque: a token that only clears the threshold at partial alpha
/// cannot be checked, and an alpha a renderer multiplies down again silently
/// drops below it.
pub fn graphic_on_with_minimum(background: Hsla, preferred: Hsla, minimum: f32) -> Hsla {
    let background = background.alpha(1.0);
    let preferred = composite_surface(background, preferred);
    graphic_on_all(preferred, &[background], minimum)
}

pub(crate) fn graphic_on_for_mode(background: Hsla, preferred: Hsla, increased: bool) -> Hsla {
    graphic_on_with_minimum(
        background,
        preferred,
        if increased {
            INCREASED_CONTRAST_GRAPHIC_MIN
        } else {
            MARKER_MIN_CONTRAST
        },
    )
}

/// Chart roles.
///
/// The chart plot sits on the canvas, so its marks are solved against that
/// surface instead of assuming the panel behind them.
pub mod chart {
    use gpui_kit::{App, Hsla};

    use super::role;

    /// Crosshair that marks the hovered or keyboard-scrubbed sample.
    ///
    /// It is a graphic, so it follows the graphic threshold. A flat
    /// `text_muted` wash multiplied down for subtlety falls below that
    /// threshold once Increase Contrast has pushed the text tokens to `7:1`, so
    /// the role is solved instead of dimmed. The crosshair stays a hairline
    /// lighter in weight than the data line, not lower in contrast.
    pub fn crosshair(cx: &App) -> Hsla {
        super::graphic_on_for_mode(
            role::surface_app(cx),
            super::colors(cx).text_muted,
            crate::settings::increase_contrast_enabled(cx),
        )
    }

    /// Categorical series color for one series index, solved against the canvas.
    ///
    /// The accent pool is the right hue source: it is already solved per theme
    /// and per appearance, and it stays harmonious when the user picks a
    /// different accent. But an accent is tuned to sit inside UI chrome, not to
    /// be read as a one-pixel line on the plot canvas — the lighter members of
    /// the default pool fall to roughly 1.3:1 there, which is invisible. Solving
    /// the hue against the canvas is the same move `crosshair` makes, and it
    /// guarantees the mark clears `MARKER_MIN_CONTRAST` on its own surface
    /// rather than against an assumed one.
    ///
    /// Color is deliberately not the only channel. `charts::element` already
    /// draws a distinct dash pattern per series index, which is the redundancy
    /// `charts.md › Color` asks for; this function only fixes the luminance the
    /// dash pattern cannot fix.
    pub fn series(index: usize, cx: &App) -> Hsla {
        super::graphic_on_for_mode(
            role::surface_app(cx),
            super::accent_color(cx, (index % super::SERIES_SLOTS) as u32),
            crate::settings::increase_contrast_enabled(cx),
        )
    }
}

/// Series color slots before the ramp repeats.
///
/// Five matches the dash patterns in `charts::element`, so a repeated hue always
/// arrives with a repeated stroke and never looks like a new series.
pub const SERIES_SLOTS: usize = 5;

pub fn marker_for_background(foreground: Hsla, background: Hsla) -> Hsla {
    graphic_on(background, foreground)
}

/// Keeps a raised surface distinguishable from the surfaces it can sit on.
///
/// A modal lands on the content pane and on the YAML editor far more often than
/// on a side panel, so all three are checked and the strongest one wins.
/// Checking only `panel` and `surface` let `elevated_surface` rest at exactly
/// the same value as the light editor and the dialog dissolved into the document
/// underneath it.
fn separate_surface(raised: Hsla, neighbors: &[Hsla]) -> Hsla {
    let raised = raised.alpha(1.0);
    if neighbors
        .iter()
        .all(|neighbor| contrast_ratio(raised, *neighbor) >= SURFACE_MIN_CONTRAST)
    {
        return raised;
    }
    adjusted_color_on_all(raised, neighbors, SURFACE_MIN_CONTRAST)
}

/// Text tokens only differ from their base color in lightness, so the two
/// lightness extremes decide whether a surface can host a token at all.
fn surface_hosts_text(surface: Hsla, tokens: &[Hsla], minimum: f32) -> bool {
    let backgrounds = [surface];
    tokens.iter().all(|token| {
        meets_contrast(color_at_lightness(*token, 0.0), &backgrounds, minimum)
            || meets_contrast(color_at_lightness(*token, 1.0), &backgrounds, minimum)
    })
}

/// Increased contrast needs every text surface inside one luminance band,
/// because one text token has to clear the threshold on all of them. Themes
/// paint selection and search washes with mid-tone accents, and a mid-tone
/// surface stays below `minimum` for any text color, so walk such a wash
/// toward the polarity the text already uses until the surface it paints can
/// host every token.
fn reachable_wash(bases: &[Hsla], wash: Hsla, tokens: &[Hsla], minimum: f32, darker: bool) -> Hsla {
    let hosts_text = |wash: Hsla| {
        bases
            .iter()
            .all(|base| surface_hosts_text(composite_surface(*base, wash), tokens, minimum))
    };
    if hosts_text(wash) {
        return wash;
    }
    let direction = if darker { -1.0 } else { 1.0 };
    for step in 0..=256 {
        let candidate = Hsla {
            l: (wash.l + direction * step as f32 / 256.0).clamp(0.0, 1.0),
            ..wash
        };
        if candidate == wash {
            continue;
        }
        if hosts_text(candidate) {
            return candidate;
        }
    }
    Hsla {
        l: if darker { 0.0 } else { 1.0 },
        ..wash
    }
}

fn core_surfaces(colors: &ThemeColors) -> Vec<Hsla> {
    vec![
        colors.background,
        colors.panel_background,
        colors.surface_background,
        colors.elevated_surface_background,
        colors.editor_background,
        colors.editor_gutter_background,
        colors.element_selected,
    ]
}

/// The base every row wash composites onto.
///
/// `DESIGN.md` §3.4 files `surface` under tables and inputs. The table used to
/// paint its rows on `canvas`, which left the one level a reader stares at for
/// eight hours consumed by a 34px tab strip. Every row state composites onto
/// this same base, so a striped row, a hovered row, the keyboard cursor and a
/// selected row can never end up on different surfaces.
fn row_base(colors: &ThemeColors) -> Hsla {
    colors.surface_background
}

/// The alphas the two accent row states are actually painted with.
///
/// A row state is a surface the body text sits on, so its strength is capped by
/// what the text on it can reach. Most dark themes ship a near-white accent, and
/// a light accent over a dark table walks through the mid-tones on its way to
/// any strength a reader would call a selection, and no text colour of either
/// polarity clears `7:1` in the middle of that walk. Under Increase Contrast the
/// requested alpha therefore lands in the gap, and the wash has to stop at the
/// edge of the band the text lives in instead of at the number the design asked
/// for.
///
/// The keyboard cursor keeps its share of the selection's strength, so a band too
/// narrow for the pair narrows both rather than collapsing them onto one colour.
fn row_accent_alphas(colors: &ThemeColors, selected_alpha: f32, minimum: f32) -> (f32, f32) {
    let base = row_base(colors);
    let tokens = [
        colors.text,
        colors.text_muted,
        colors.text_placeholder,
        colors.text_disabled,
        colors.text_accent,
    ];
    let hosts = |alpha: f32| {
        surface_hosts_text(
            composite_surface(base, colors.text_accent.opacity(alpha)),
            &tokens,
            minimum,
        )
    };
    if hosts(selected_alpha) {
        return (ROW_FOCUS_ALPHA, selected_alpha);
    }
    // The wash has to stay readable the whole way out from the table, so the cap is
    // where the walk leaves the band rather than the far side of it, and the composite
    // only moves one way as the alpha grows. A coarse pass finds the step that leaves
    // the band and a short bisection pins it down, because this search runs while a
    // table paints and a coarse answer is only ever a slightly weaker fill.
    let mut inside = 0.0;
    let mut outside = selected_alpha;
    for step in 1..=32 {
        let candidate = selected_alpha * step as f32 / 32.0;
        if hosts(candidate) {
            inside = candidate;
        } else {
            outside = candidate;
            break;
        }
    }
    for _ in 0..4 {
        let candidate = (inside + outside) / 2.0;
        if hosts(candidate) {
            inside = candidate;
        } else {
            outside = candidate;
        }
    }
    (ROW_FOCUS_ALPHA * inside / selected_alpha, inside)
}

/// The zebra wash, the generic hover wash and the accent washes a row can carry.
///
/// The stripe is solved rather than composited, so the refinement pass measures
/// the surface the renderer paints instead of the tint it asked for. The hover is
/// still the raw `element_hover`: the solve only walks a wash away from the table,
/// so reading the unsolved one is the conservative direction for a text token.
fn row_washes(colors: &ThemeColors, selected_alpha: f32, minimum: f32) -> [Hsla; 4] {
    let base = row_base(colors);
    let (focus_alpha, selected_alpha) = row_accent_alphas(colors, selected_alpha, minimum);
    [
        row_wash(base, colors.text.opacity(ROW_STRIPE_ALPHA)),
        colors.element_hover,
        composite_surface(base, colors.text_accent.opacity(focus_alpha)),
        composite_surface(base, colors.text_accent.opacity(selected_alpha)),
    ]
}

fn text_surfaces(colors: &ThemeColors, selected_alpha: f32, minimum: f32) -> Vec<Hsla> {
    let [stripe, hover, focus, selected] = row_washes(colors, selected_alpha, minimum);
    let editor = colors.editor_background;
    let mut surfaces = core_surfaces(colors);
    surfaces.extend([
        stripe,
        hover,
        selected,
        focus,
        composite_surface(editor, colors.element_selection_background),
        composite_surface(editor, search_match::resolve_background(colors)),
        composite_surface(editor, search_match::resolve_active_background(colors)),
    ]);
    surfaces
}

/// The surfaces the accent is solved against.
///
/// The row selection and the keyboard cursor are both accent washes, so solving
/// the accent for text contrast on them hands the solver its own output: the
/// accent walks one way, the washes walk with it, and past a certain alpha the
/// two chase each other and the solver saturates. The accent is the selection's
/// identity and its rail, not the text that sits on the selection, so it keeps
/// every surface that is not built out of the accent. The rail is a graphic and
/// is held to the graphic floor on the two washes instead.
fn accent_surfaces(colors: &ThemeColors, selected_alpha: f32, minimum: f32) -> Vec<Hsla> {
    let [stripe, hover, _, _] = row_washes(colors, selected_alpha, minimum);
    let editor = colors.editor_background;
    let mut surfaces = core_surfaces(colors);
    surfaces.extend([
        stripe,
        hover,
        composite_surface(editor, colors.element_selection_background),
        composite_surface(editor, search_match::resolve_background(colors)),
        composite_surface(editor, search_match::resolve_active_background(colors)),
    ]);
    surfaces
}

/// Surfaces only the increased-contrast pass has to keep readable.
///
/// The cursor line and the inline diagnostic wash are not theme surfaces: the
/// renderer blends them over the editor background, so the composite only
/// becomes a text surface once the text tokens have moved to `7:1`. Counting
/// them here is what keeps the active line and the diagnostic readable next to
/// the selection and the search roles instead of borrowing one of them. They
/// stay out of the default pass so the `4.5:1` refinement of every supported
/// theme is unchanged.
fn increased_text_surfaces(
    colors: &ThemeColors,
    status: &StatusColors,
    selected_alpha: f32,
) -> Vec<Hsla> {
    let mut surfaces = text_surfaces(colors, selected_alpha, INCREASED_CONTRAST_TEXT_MIN);
    surfaces.extend([
        composite_surface(
            colors.editor_background,
            editor_wash::active_line_overlay_for(colors),
        ),
        composite_surface(
            colors.editor_background,
            editor_wash::diagnostic_overlay_for(colors, status),
        ),
    ]);
    surfaces
}

/// Increased-contrast counterpart of [`accent_surfaces`].
fn increased_accent_surfaces(
    colors: &ThemeColors,
    status: &StatusColors,
    selected_alpha: f32,
) -> Vec<Hsla> {
    let mut surfaces = accent_surfaces(colors, selected_alpha, INCREASED_CONTRAST_TEXT_MIN);
    surfaces.extend([
        composite_surface(
            colors.editor_background,
            editor_wash::active_line_overlay_for(colors),
        ),
        composite_surface(
            colors.editor_background,
            editor_wash::diagnostic_overlay_for(colors, status),
        ),
    ]);
    surfaces
}

fn refine_status_triplet(
    foreground: &mut Hsla,
    background: &mut Hsla,
    border: &mut Hsla,
    base: Hsla,
    core: &[Hsla],
    text_minimum: f32,
    graphic_minimum: f32,
) {
    *background = composite_surface(base, *background);
    *foreground = adjusted_color_on_all(*foreground, core, text_minimum);
    *border = adjusted_color_on_all(*border, core, graphic_minimum);
}

/// Washes are placed with a little headroom above the text threshold, so the
/// text tokens keep room to stay near their base color instead of sitting on
/// their lightness extreme.
const WASH_CONTRAST_HEADROOM: f32 = 0.25;

/// Move the selection and search washes into the luminance band the text
/// tokens already use, so a single text token can clear `text_minimum` on
/// every surface. Theme washes are mid-tone by default, which no text color
/// can contrast to the increased-contrast threshold.
fn refine_semantic_washes(colors: &mut ThemeColors, text_minimum: f32) {
    let tokens = [
        colors.text,
        colors.text_muted,
        colors.text_placeholder,
        colors.text_disabled,
        colors.text_accent,
    ];
    let editor = colors.editor_background.alpha(1.0);
    let inputs = colors.surface_background.alpha(1.0);
    let darker = colors.text.l > 0.5;
    let minimum = text_minimum + WASH_CONTRAST_HEADROOM;
    colors.element_selection_background = reachable_wash(
        &[editor, inputs],
        colors.element_selection_background,
        &tokens,
        minimum,
        darker,
    );
    colors.search_match_background = reachable_wash(
        &[editor],
        colors.search_match_background,
        &tokens,
        minimum,
        darker,
    );
    colors.search_active_match_background = reachable_wash(
        &[editor],
        colors.search_active_match_background,
        &tokens,
        minimum,
        darker,
    );
}

fn refine_text_colors(
    colors: &mut ThemeColors,
    status: &StatusColors,
    text_minimum: f32,
    disabled_minimum: f32,
    selected_alpha: f32,
    increased: bool,
) {
    for _ in 0..16 {
        let surfaces = if increased {
            increased_text_surfaces(colors, status, selected_alpha)
        } else {
            text_surfaces(colors, selected_alpha, text_minimum)
        };
        let accent_on = if increased {
            increased_accent_surfaces(colors, status, selected_alpha)
        } else {
            accent_surfaces(colors, selected_alpha, text_minimum)
        };
        let text = adjusted_color_on_all(colors.text, &surfaces, text_minimum);
        let muted = adjusted_color_on_all(colors.text_muted, &surfaces, text_minimum);
        let placeholder = adjusted_color_on_all(colors.text_placeholder, &surfaces, text_minimum);
        let accent = adjusted_color_on_all(colors.text_accent, &accent_on, text_minimum);
        // A disabled control is exempt from the body-text floor, so it is solved
        // against a floor of its own and a ceiling taken from the role above it
        // rather than being pushed onto the same value as `text_muted`.
        let disabled = quieter_color_on_all(
            colors.text_disabled,
            placeholder,
            &surfaces,
            disabled_minimum,
        );
        let changed = text != colors.text
            || muted != colors.text_muted
            || placeholder != colors.text_placeholder
            || accent != colors.text_accent
            || disabled != colors.text_disabled;
        colors.text = text;
        colors.text_muted = muted;
        colors.text_placeholder = placeholder;
        colors.text_accent = accent;
        colors.text_disabled = disabled;
        if !changed {
            break;
        }
    }
}

fn refine_status_backgrounds(status: &mut StatusColors, text: Hsla, text_minimum: f32) {
    for background in [
        &mut status.success_background,
        &mut status.warning_background,
        &mut status.error_background,
        &mut status.info_background,
    ] {
        *background = adjusted_color_on_all(*background, &[text], text_minimum);
    }
}

fn refine_status_foregrounds(
    status: &mut StatusColors,
    surfaces: &[Hsla],
    text_minimum: f32,
    graphic_minimum: f32,
) {
    status.success = adjusted_color_on_all(status.success, surfaces, text_minimum);
    status.warning = adjusted_color_on_all(status.warning, surfaces, text_minimum);
    status.error = adjusted_color_on_all(status.error, surfaces, text_minimum);
    status.info = adjusted_color_on_all(status.info, surfaces, text_minimum);
    status.success_border = adjusted_color_on_all(status.success_border, surfaces, graphic_minimum);
    status.warning_border = adjusted_color_on_all(status.warning_border, surfaces, graphic_minimum);
    status.error_border = adjusted_color_on_all(status.error_border, surfaces, graphic_minimum);
    status.info_border = adjusted_color_on_all(status.info_border, surfaces, graphic_minimum);
}
pub fn refine_theme_with_contrast(
    colors: &mut ThemeColors,
    status: &mut StatusColors,
    accents: &mut [Hsla],
    increased: bool,
) {
    // The refiners below hand the same two structs around by mutable reference,
    // so the incoming bindings are reborrowed once here instead of at every
    // call site.
    let colors = &mut *colors;
    let status = &mut *status;
    let (text_minimum, graphic_minimum) = if increased {
        (INCREASED_CONTRAST_TEXT_MIN, INCREASED_CONTRAST_GRAPHIC_MIN)
    } else {
        (TEXT_MIN_CONTRAST, MARKER_MIN_CONTRAST)
    };
    let selected_alpha = if increased {
        INCREASED_ROW_SELECTED_ALPHA
    } else {
        ROW_SELECTED_ALPHA
    };
    // Increase Contrast raises the floor for everything, including the roles
    // that are exempt from it. Leaving disabled text at its own floor is what
    // keeps it quieter than the role above it; the increased pass still lifts
    // it, just not as far.
    let disabled_minimum = if increased {
        INCREASED_CONTRAST_TEXT_MIN
    } else {
        DISABLED_TEXT_MIN_CONTRAST
    };
    let background = colors.background.alpha(1.0);
    let editor_background = composite_surface(background, colors.editor_background);
    let panel_background = composite_surface(background, colors.panel_background);
    let surface_background = composite_surface(background, colors.surface_background);
    let raised_background = composite_surface(background, colors.elevated_surface_background);
    let terminal_background = composite_surface(background, colors.terminal_background);

    colors.background = background;
    colors.editor_background = editor_background;
    colors.panel_background = panel_background;
    colors.surface_background = surface_background;
    // The editor is a third neighbour: a modal lands on the YAML editor more
    // often than on a panel, and the editor is the one surface `surface` does
    // not cover. Checking only panel and surface let the light theme put both
    // `raised` and the editor on the same white and dissolve a dialog into the
    // document underneath it.
    colors.elevated_surface_background = separate_surface(
        raised_background,
        &[panel_background, surface_background, editor_background],
    );
    colors.terminal_background = terminal_background;
    colors.title_bar_background = composite_surface(background, colors.title_bar_background);
    colors.title_bar_inactive_background =
        composite_surface(background, colors.title_bar_inactive_background);
    colors.tab_bar_background = composite_surface(background, colors.tab_bar_background);
    colors.element_hover = composite_surface(surface_background, colors.element_hover);
    colors.element_active = composite_surface(surface_background, colors.element_active);
    colors.element_selected = composite_surface(surface_background, colors.element_selected);
    colors.editor_gutter_background =
        composite_surface(editor_background, colors.editor_gutter_background);
    colors.editor_subheader_background =
        composite_surface(background, colors.editor_subheader_background);
    colors.editor_highlighted_line_background =
        composite_surface(editor_background, colors.editor_highlighted_line_background);
    colors.panel_overlay_background =
        composite_surface(panel_background, colors.panel_overlay_background);
    search_match::refine_colors(colors);
    if increased {
        refine_semantic_washes(colors, text_minimum);
    }

    let core = core_surfaces(colors);
    refine_status_triplet(
        &mut status.success,
        &mut status.success_background,
        &mut status.success_border,
        background,
        &core,
        text_minimum,
        graphic_minimum,
    );
    refine_status_triplet(
        &mut status.warning,
        &mut status.warning_background,
        &mut status.warning_border,
        background,
        &core,
        text_minimum,
        graphic_minimum,
    );
    refine_status_triplet(
        &mut status.error,
        &mut status.error_background,
        &mut status.error_border,
        background,
        &core,
        text_minimum,
        graphic_minimum,
    );
    refine_status_triplet(
        &mut status.info,
        &mut status.info_background,
        &mut status.info_border,
        background,
        &core,
        text_minimum,
        graphic_minimum,
    );

    refine_text_colors(
        colors,
        status,
        text_minimum,
        disabled_minimum,
        selected_alpha,
        increased,
    );
    let text = colors.text;
    refine_status_backgrounds(status, text, text_minimum);
    if increased {
        let surfaces = increased_text_surfaces(colors, status, selected_alpha);
        refine_status_foregrounds(status, &surfaces, text_minimum, graphic_minimum);
    }

    colors.editor_foreground = colors.text;

    let terminal_background = colors.terminal_background;
    colors.terminal_foreground = refined_text_on(
        terminal_background,
        colors.terminal_foreground,
        colors.text,
        text_minimum,
        increased,
    );
    colors.terminal_bright_foreground = refined_text_on(
        terminal_background,
        colors.terminal_bright_foreground,
        colors.terminal_foreground,
        text_minimum,
        increased,
    );
    colors.terminal_dim_foreground = refined_text_on(
        terminal_background,
        colors.terminal_dim_foreground,
        colors.terminal_foreground,
        text_minimum,
        increased,
    );

    let graphic_surfaces = if increased {
        increased_text_surfaces(colors, status, selected_alpha)
    } else {
        core_surfaces(colors)
    };
    if increased {
        let graphic_fallback = colors.text;
        for color in [
            &mut colors.border,
            &mut colors.border_variant,
            &mut colors.border_disabled,
            &mut colors.panel_focused_border,
            &mut colors.pane_focused_border,
            &mut colors.pane_group_border,
            &mut colors.scrollbar_thumb_border,
            &mut colors.scrollbar_track_border,
            &mut colors.minimap_thumb_border,
            &mut colors.icon_disabled,
            &mut colors.icon_placeholder,
            &mut colors.icon_accent,
            &mut colors.debugger_accent,
        ] {
            *color = refined_graphic_on(
                *color,
                graphic_fallback,
                &graphic_surfaces,
                graphic_minimum,
                increased,
            );
        }
    }
    colors.border_focused = refined_graphic_on(
        colors.border_focused,
        colors.text_accent,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );
    colors.border_selected = refined_graphic_on(
        colors.border_selected,
        colors.text_accent,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );
    colors.drop_target_border = refined_graphic_on(
        colors.drop_target_border,
        colors.text_accent,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );
    colors.pane_focused_border = refined_graphic_on(
        colors.pane_focused_border,
        colors.text_accent,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );

    let panel = colors.panel_background;
    let background = colors.background;
    for accent in accents.iter_mut() {
        *accent = graphic_on_all(*accent, &[panel, background], graphic_minimum);
    }
}

impl Severity {
    /// The ink a severity speaks in.
    ///
    /// **`Success` is grey, not green.** This is the product's central colour
    /// decision and every other channel follows it: when everything is fine the
    /// interface is *quiet*, and a colour appears only for something wrong. A
    /// table of 10,000 rows in which every healthy row is green is a table in
    /// which the reader has nothing to look at.
    ///
    /// It used to return `status.success`, and five components inherited the
    /// decision without knowing it was one: a healthy cluster's node count
    /// printed green, a running port forward wore a green check, and the same
    /// pod was green in the sidebar and grey in the table. The mapping now lives
    /// once, in [`role::status_for`].
    pub fn color(self, cx: &App) -> Hsla {
        crate::design::role::status_for(self, cx)
    }

    pub fn marker(self, cx: &App) -> Hsla {
        self.marker_on(cx, role::surface_app(cx))
    }

    pub fn marker_on(self, cx: &App, background: Hsla) -> Hsla {
        marker_for_background(self.color(cx), background)
    }

    /// The ink a severity *spells itself out* in.
    ///
    /// §4.4's status cell draws a mark and then a word, and the two are separate
    /// roles on the channel — the mark at [`STATUS_MARK_MIN_CONTRAST`] and the
    /// word at the body-text floor, so Increase Contrast lifts one and not the
    /// other. A caller that reaches for [`Severity::color`] when it is about to
    /// draw text is putting a mark role on a word, which is the mistake that
    /// turned Dark `danger` pink under the setting.
    pub fn word(self, cx: &App) -> Hsla {
        crate::design::role::status_word_for(self, cx)
    }

    /// The fill a banner of this severity paints behind its text.
    ///
    /// A notice carries a severity, and a surface that means one thing has to
    /// mean it everywhere. Two components had their own copy of this: the table's
    /// notice banner, whose catch-all handed `Success` the *info* fill so "fine"
    /// and "informational" were the same colour, and the Overview's health wash.
    /// `DESIGN.md §4`'s rule that a component may not keep a private map is
    /// enforced by `no_component_keeps_a_private_status_vocabulary`, and a banner
    /// fill is a status vocabulary in the same family as the glyph and the word.
    pub fn wash(self, cx: &App) -> Hsla {
        let palette = crate::design::status_colors(cx);
        match self {
            Severity::Success => palette.success_background,
            Severity::Warning => palette.warning_background,
            Severity::Error => palette.error_background,
            // `Info`, `Neutral` and `Muted` are quiet roles rather than problems,
            // and the info fill is the only one of the four that is not a call to
            // action. Spelling all three out keeps a new severity a compile error
            // instead of a silent fall-through to a fill it never asked for.
            Severity::Info | Severity::Neutral | Severity::Muted => palette.info_background,
        }
    }
}

/// The Pending grade, shared by every surface that shows a pod's age.
///
/// `UI-SPEC` §0 (铁律三) requires a Pending to be graded by how long it has been
/// pending, because 9,900 pending rows and 300 stuck rows look identical when
/// they are the same colour. A table can grade it locally, and it did — and then
/// the sidebar, the Overview's issue chips and the status bar each made their own
/// call, so the same pod was `danger` in the table and `warning` in the sidebar.
///
/// One function, and every surface reads it. `None` ages grey, and that is the
/// point: a cluster that did not send a creation timestamp has told us nothing
/// about how long anything has been waiting, and colouring it on the strength of
/// a field that is not there is the same lie as reading `Unknown` as healthy.
pub fn pod_severity_with_age(status: &str, age: Option<std::time::Duration>) -> Severity {
    let base = pod_severity(status);
    // Only the queued states are graded. A `Failed` pod is failed at any age, and
    // a `Running` pod that has been up for a week is the healthiest thing on the
    // screen — grading those by age would invent a problem.
    if !matches!(status, "Pending" | "ContainerCreating") {
        return base;
    }
    let Some(age) = age else {
        return base;
    };
    let seconds = age.as_secs_f32();
    if seconds > AGED_PENDING_DANGER.as_secs_f32() {
        Severity::Error
    } else if seconds > AGED_PENDING_WARNING.as_secs_f32() {
        Severity::Warning
    } else {
        base
    }
}

/// A Pending older than this is a problem, not a queue.
///
/// Thirty seconds is not a round number anyone chose for a reason; it is the
/// point at which a reader watching a rollout stops believing it is still
/// working. Short enough to stay out of the way, long enough that an ordinary
/// scheduling delay never wears a colour.
pub const AGED_PENDING_WARNING: std::time::Duration = std::time::Duration::from_secs(30);

/// A Pending older than this is stuck.
///
/// Five minutes is past every normal scheduling path in Kubernetes, including a
/// node that is draining. Past it, the pod is not waiting for something — nothing
/// is coming.
pub const AGED_PENDING_DANGER: std::time::Duration = std::time::Duration::from_secs(300);

/// Map a Pod or workload status to a severity. Callers keep the status text.
///
/// `"Unknown"` is spelled out rather than left to the catch-all so the reason it
/// is *not* a warning survives: the kubelet lost the pod, so nobody has a verdict,
/// and `DESIGN.md` §4 requires "读不到 ≠ 健康". It shares the catch-all's severity
/// by design and differs from `Pending` — genuinely queued, and we asked.
pub fn pod_severity(status: &str) -> Severity {
    match status {
        "Running" | "Succeeded" | "Active" | "Ready" | "Bound" => Severity::Success,
        "Pending" | "ContainerCreating" | "Terminating" => Severity::Warning,
        "Unknown" => Severity::Neutral,
        "Failed" | "Error" | "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull" => {
            Severity::Error
        }
        "" => Severity::Muted,
        _ => Severity::Neutral,
    }
}

/// The twelve kind icons, in the shape order `UI-SPEC` §2.4.3 fixes.
///
/// One form each — `duotone`, 32% fill, 1.5px stroke, 2px corners — and the same
/// form in both appearances, because the fill is `currentColor` at a fraction of
/// the *stroke* colour rather than of the background.
///
/// The names and their asset paths are a token because they are a *decision*:
/// which twelve kinds get bespoke shapes, and which shape each one gets. The
/// files themselves belong to the app, which is the crate that embeds them; this
/// table is the one place that says which file answers for which kind, so a
/// panel and the asset source cannot disagree about it.
pub const KIND_ICON_PATHS: [(&str, &str); 12] = [
    ("Pod", "icons/k8s-pod.svg"),
    ("Deployment", "icons/k8s-deployment.svg"),
    ("StatefulSet", "icons/k8s-statefulset.svg"),
    ("ReplicaSet", "icons/k8s-replicaset.svg"),
    ("DaemonSet", "icons/k8s-daemonset.svg"),
    ("Job", "icons/k8s-job.svg"),
    ("CronJob", "icons/k8s-cronjob.svg"),
    ("Node", "icons/k8s-node.svg"),
    ("Service", "icons/k8s-service.svg"),
    ("Ingress", "icons/k8s-ingress.svg"),
    ("ConfigMap", "icons/k8s-configmap.svg"),
    ("Namespace", "icons/k8s-namespace.svg"),
];

/// The asset path of a kind's icon, or the stand-in for a kind that has none.
///
/// A kind outside the twelve gets a rounded square rather than no glyph at all,
/// and — the part that matters — the *same* stand-in every time. A kind with its
/// own bespoke shape, or a missing one, is how a set of seventy-one kinds ends
/// up looking like seventy-one separate decisions. Its identity is the first
/// letter, which the caller draws.
pub fn kind_icon_path(kind: &str) -> SharedString {
    let matches = |canonical: &str| {
        // Kubernetes pluralises in a list and a caller may hand over either, and
        // `ConfigMaps` stripping to `ConfigMap` is the same kind, not a
        // different one.
        kind.strip_suffix('s')
            .unwrap_or(kind)
            .eq_ignore_ascii_case(canonical)
    };
    KIND_ICON_PATHS
        .iter()
        .find(|(name, _)| matches(name))
        .map(|(_, path)| SharedString::from(*path))
        .unwrap_or_else(|| SharedString::from("icons/k8s-kind-fallback.svg"))
}

/// The kinds that have a bespoke icon, for a test that holds the table to the
/// set the design fixed.
pub const KIND_ICON_COUNT: usize = 12;

/// Map a Kubernetes kind to a shared-catalog glyph.
///
/// This is the *chrome* vocabulary — the cluster row, an API group, an event, a
/// network policy — and not the kind set. A kind with a bespoke icon is
/// [`kind_icon_path`], and a caller that means "which kind is this" must use
/// that one: this one answers "which category", and a Deployment and a
/// ReplicaSet are the same category and must look the same here.
pub fn kind_icon(kind: &str) -> IconName {
    match kind {
        "Pod" | "Pods" => IconName::Box,
        "Deployment" | "Deployments" | "ReplicaSet" | "ReplicaSets" | "StatefulSet"
        | "StatefulSets" | "DaemonSet" | "DaemonSets" => IconName::Blocks,
        "Node" | "Nodes" => IconName::Server,
        // A namespace is not an API group, so it does not get the group's folder,
        // and a cluster is not a node, so it does not get the node's rack. The
        // sidebar's `Cluster` row and the `Overview` tab both used to land on
        // `Server` and read as the same object as `Nodes`.
        "Group" => IconName::Folder,
        "Job" | "Jobs" | "CronJob" | "CronJobs" => IconName::Clock,
        "Event" | "Events" | "Info" => IconName::Info,
        "Service" | "Services" | "Ingress" | "Ingresses" | "NetworkPolicy" | "NetworkPolicies" => {
            IconName::ArrowRightLeft
        }
        _ => IconName::File,
    }
}

/// The glyph for the table's problems filter, in its two states.
///
/// The filter's state has to survive greyscale, so the two states are two
/// different shapes and not one shape in two colors. The icon set has no filled
/// filter glyph, so the on state borrows the funnel, which reads as "narrowed",
/// and the off state keeps the tune glyph the control always shows.
pub fn problems_filter_icon(active: bool) -> IconName {
    if active {
        IconName::Funnel
    } else {
        IconName::SlidersHorizontal
    }
}

/// Row overlay alpha values shared by rendering and contrast tests.
///
/// All four composite onto `surface.background`, so a row is only ever as
/// strong as the table it sits on and the states can be compared against each
/// other.
pub const ROW_STRIPE_ALPHA: f32 = 0.05;
/// The accent wash on the selected row.
///
/// The zebra wash, the hover wash and the keyboard cursor all land within about
/// half a point of each other on the light table, so the alphas are spread
/// further apart than they used to be: at `0.14` the selection had no room left
/// for a third state between it and the floor the other two have to clear.
pub const ROW_SELECTED_ALPHA: f32 = 0.20;
/// The accent wash on the row the keyboard cursor is on, before anything is
/// selected. A different alpha from the selection on purpose: the cursor and the
/// selection are different states, and `DESIGN.md` §5 asks for both.
pub const ROW_FOCUS_ALPHA: f32 = 0.16;
/// Alpha of the accent wash on the selected row under Increase Contrast.
///
/// Turning the setting on has to make the selection *stronger*. It used to drop
/// from `0.14` to `0.05`, which is quieter than the default it was supposed to
/// reinforce, and a range member scaled it down again on top of that.
pub const INCREASED_ROW_SELECTED_ALPHA: f32 = 0.24;
/// A row state has to be visible against the table it sits on.
///
/// Below this a wash is a rendering difference nobody can see: the light hover
/// measured 1.018:1 against the canvas it was painted on, which is less than the
/// rounding of the composite it was built from.
pub const ROW_STATE_MIN_CONTRAST: f32 = 1.2;
/// Two row states have to be tellable apart.
///
/// A hover that outshouts the selection it leads to is worse than no hover at
/// all, and a keyboard cursor indistinguishable from a selection cannot be
/// located without reading the whole row.
pub const ROW_STATE_MIN_SEPARATION: f32 = 1.05;

/// Whether the app last observed a definite answer, is still working, or could
/// not reach a verdict at all.
///
/// Observation confidence is a separate channel from [`Severity`]. A resource
/// the app could not read is not healthy, and a cluster mid-resync is not
/// broken. Collapsing the two into one color is what makes a status display lie
/// exactly when it matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// The API returned an answer and it is current.
    Known,
    /// The answer is older than the refresh interval.
    Stale,
    /// The API did not return a verdict, so health is undetermined.
    Unknown,
}

impl Confidence {
    /// Reports whether this confidence state can report a health verdict.
    pub fn is_definite(self) -> bool {
        matches!(self, Self::Known)
    }
}

/// Reads the observation confidence a piece of row or panel text implies.
///
/// Callers that already track freshness in their own state should pass that
/// instead of parsing text. This exists for the common case where only the
/// rendered string is available.
pub fn confidence_from_text(text: &str) -> Confidence {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("unknown") || lowered.contains("unreachable") {
        Confidence::Unknown
    } else if lowered.contains("stale")
        || lowered.contains("refreshing")
        || lowered.contains("reconnecting")
    {
        Confidence::Stale
    } else {
        Confidence::Known
    }
}

/// The observation-confidence channel.
///
/// Confidence borrows no other role. It used to read `text_muted` for `Known`,
/// `text_placeholder` for `Stale` and `icon_disabled` for `Unknown`, which meant
/// three roles borrowed from three other channels: a stale answer looked like a
/// hint, an unreadable one looked like a disabled control, and `icon_disabled` is
/// only ever refined under Increase Contrast, so the default path checked
/// nothing at all. `Unknown` measured 4.19:1 on the light panel and 4.01:1 on the
/// light canvas, the only body-text failure in the theme.
///
/// The two hues are the ones `UI-REDESIGN` §4.1 specified, and they live in the
/// theme file as `confidence.unknown` and `confidence.stale` — which is where a
/// channel a skin is meant to own belongs. They used to be two `const [u32; 2]`
/// tables in this file while the theme carried a *different* pair of values under
/// those two names, so the doc comment below was describing keys nothing read.
/// They are desaturated on purpose: a confidence mark must never compete with a
/// status hue for attention, and it is solved against the same surfaces as the
/// status foregrounds so both channels are held to the same threshold.
pub mod confidence {
    use super::IconName;
    use gpui_kit::{App, Hsla};

    use super::{
        Confidence, INCREASED_CONTRAST_TEXT_MIN, TEXT_MIN_CONTRAST, adjusted_color_on_all, colors,
        core_surfaces,
    };

    /// The color of a confidence marker or caption.
    ///
    /// Solved rather than read straight out of the theme, because the seeds are
    /// written for one appearance: the light `Unknown` misses the body-text floor
    /// on the light panel and the light canvas. Solving keeps the channel above
    /// `TEXT_MIN_CONTRAST` — or `7:1` under Increase Contrast — on every surface a
    /// marker can land on, which is the same promise the status foregrounds get.
    pub fn foreground(state: Confidence, cx: &App) -> Hsla {
        let minimum = if crate::settings::increase_contrast_enabled(cx) {
            INCREASED_CONTRAST_TEXT_MIN
        } else {
            TEXT_MIN_CONTRAST
        };
        let colors = colors(cx);
        let preferred = match state {
            // A known answer draws no marker at all, so the only ink it spends is
            // the caption beside it, and that caption is secondary text. It is
            // not a second health channel: `confidence::icon` and `health_icon`
            // are disjoint shape families, which is what keeps the two apart on a
            // row that carries both.
            Confidence::Known => colors.text_muted,
            Confidence::Stale => colors.confidence_stale,
            Confidence::Unknown => colors.confidence_unknown,
        };
        adjusted_color_on_all(preferred, &core_surfaces(colors), minimum)
    }

    /// The shape that carries confidence, so the channel survives color
    /// blindness and greyscale.
    ///
    /// Health is drawn with filled or outlined glyphs, so every confidence
    /// marker stays hollow or plain. That is what keeps the two channels
    /// separable when both appear on the same row: shape alone says which
    /// axis a mark belongs to.
    pub fn icon(state: Confidence) -> IconName {
        match state {
            // Known needs no mark. Drawing one would put a second glyph on
            // every healthy row and imply a second thing to read.
            Confidence::Known => IconName::Circle,
            Confidence::Stale => IconName::Clock,
            Confidence::Unknown => IconName::CircleQuestionMark,
        }
    }
}

/// The health channel's shape vocabulary.
///
/// Each severity owns a distinct outline so status survives color blindness
/// and greyscale, per `color.md > Inclusive color`. The two channels are
/// built from different shape families on purpose: health is filled or
/// heavy, confidence is hollow, so a row carrying both stays readable.
pub fn health_icon(severity: Severity) -> IconName {
    match severity {
        Severity::Success => IconName::Check,
        Severity::Warning => IconName::TriangleAlert,
        Severity::Error => IconName::CircleX,
        Severity::Info => IconName::Circle,
        Severity::Neutral | Severity::Muted => IconName::Dash,
    }
}

/// The word a shape stands for, for accessibility and for the text that
/// accompanies a shape-only marker.
pub fn health_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Success => "Healthy",
        Severity::Warning => "Needs attention",
        Severity::Error => "Failed",
        Severity::Info => "Syncing",
        Severity::Neutral | Severity::Muted => "No verdict",
    }
}

/// The word a confidence shape stands for.
pub fn confidence_label(state: Confidence) -> &'static str {
    match state {
        Confidence::Known => "Checked",
        Confidence::Stale => "Data is stale",
        Confidence::Unknown => "Status unknown, the cluster did not answer",
    }
}

/// Walks a row wash away from the table until the state is visible on it.
///
/// `element_hover` is tuned for controls: on the light table it reaches 1.105:1,
/// which is invisible, and in dark it reaches 1.43:1, which is louder than the
/// selection it can lead to. The zebra wash is a 5% `text` tint, which reached
/// 1.101:1 in light and 1.136:1 in dark — a state a reader cannot see at all.
/// Solving the wash against the surface it is actually painted on puts every row
/// state on one floor without touching the shared control role every other widget
/// reads.
///
/// The row rule is a `border.variant`, not a wash, so it is not solved here: a
/// darker stripe eats a rule painted on it, and `k8s-studio.json` carries a
/// `border.variant` that clears the floor on the solved stripe in both
/// appearances. `the_row_rule_stays_above_the_stripe_it_sits_on` is what holds the
/// two together.
fn row_wash(base: Hsla, wash: Hsla) -> Hsla {
    graphic_on_with_minimum(base, composite_surface(base, wash), ROW_STATE_MIN_CONTRAST)
}

/// A row-state wash solved against a base other than the table's.
///
/// The modal pickers are the one list in the app that paints on
/// `role::surface_raised`, and `row_selected_bg` is solved against the table's base,
/// so using it there put the selection in the same bind the sidebar rail was in:
/// legible on paper, far too quiet on the surface it actually lands on. The floor
/// is the same one every other row state clears, so the difference is the base and
/// nothing else.
pub fn row_selected_bg_on(cx: &App, base: Hsla) -> Hsla {
    let (_, alpha) = row_accent_alphas_for(cx);
    row_wash(base, colors(cx).text_accent.opacity(alpha))
}

/// The text floor the row washes have to leave room for.
fn row_text_minimum(cx: &App) -> f32 {
    if crate::settings::increase_contrast_enabled(cx) {
        INCREASED_CONTRAST_TEXT_MIN
    } else {
        TEXT_MIN_CONTRAST
    }
}

/// The cursor and selection alphas the active appearance and contrast mode allow.
fn row_accent_alphas_for(cx: &App) -> (f32, f32) {
    let requested = if crate::settings::increase_contrast_enabled(cx) {
        INCREASED_ROW_SELECTED_ALPHA
    } else {
        ROW_SELECTED_ALPHA
    };
    row_accent_alphas(colors(cx), requested, row_text_minimum(cx))
}

/// The four row states, weakest first, solved against the table as one ladder.
///
/// Four states, one floor, solved independently: that is the same collapse the
/// three border roles had, and it happened. `row_wash` only ever *raises* a wash
/// to [`ROW_STATE_MIN_CONTRAST`], so any two raw washes that both sat under the
/// floor came out at the floor — and the dark table's 5% zebra and its hover wash
/// both did, measuring 1.006:1 against each other. A striped row and a row under
/// the pointer were the same colour.
///
/// So they are solved bottom-up instead, each one at least a step clear of the one
/// below it, which is what [`ROW_STATE_MIN_SEPARATION`] is *for*: it was a constant
/// nothing enforced. The requested wash is still the starting point, so a state
/// the theme already made strong enough stays as strong as it asked to be.
fn row_washes_for(cx: &App) -> [Hsla; 4] {
    let base = role::surface_raised(cx);
    let (focus_alpha, selected_alpha) = row_accent_alphas_for(cx);
    let requested = [
        colors(cx).text.opacity(ROW_STRIPE_ALPHA),
        colors(cx).element_hover,
        colors(cx).text_accent.opacity(focus_alpha),
        colors(cx).text_accent.opacity(selected_alpha),
    ];
    let mut solved = [base; 4];
    for (index, wash) in requested.into_iter().enumerate() {
        let below = if index == 0 {
            ROW_STATE_MIN_CONTRAST
        } else {
            (ROW_STATE_MIN_CONTRAST)
                .max(calculate_contrast_ratio(solved[index - 1], base) * ROW_STATE_MIN_SEPARATION)
        };
        solved[index] = graphic_on_with_minimum(base, composite_surface(base, wash), below);
    }
    solved
}

/// Selected row background blended with the base row.
/// The color does not depend on the row index.
pub fn row_selected_bg(cx: &App) -> Hsla {
    row_washes_for(cx)[3]
}

/// The row the keyboard cursor is on, before anything is selected.
///
/// It used to be the same token as the hover, so a row the keyboard was on and a
/// row the pointer was over were the same color and the two states could not be
/// told apart. A hover is transient and a cursor is a position, so the cursor
/// carries the accent and the hover does not.
pub fn row_focus_bg(cx: &App) -> Hsla {
    row_washes_for(cx)[2]
}

pub fn row_hover_bg(cx: &App) -> Hsla {
    row_washes_for(cx)[1]
}

/// Alternating row background, blended with the base row.
///
/// The zebra is a row state like any other, so it clears the same floor as the
/// hover, the cursor and the selection instead of being a 5% tint nobody can see —
/// and it is the *weakest* of the four, because it is the only one that is not
/// saying anything about where the reader is.
pub fn row_stripe_bg(cx: &App) -> Hsla {
    row_washes_for(cx)[0]
}

pub fn increased_contrast_surface(cx: &App, surface: Hsla) -> Hsla {
    increased_contrast_colors(colors(cx), surface)
}

/// Colors-level form of [`increased_contrast_surface`].
///
/// The refinement pass runs before a theme is active, so it resolves the washes
/// from the colors it is refining instead of asking the app.
pub(crate) fn increased_contrast_colors(colors: &ThemeColors, surface: Hsla) -> Hsla {
    if surface == colors.element_selection_background && surface == colors.search_match_background {
        search_match::resolve_background(colors)
    } else if surface == colors.element_selection_background
        && surface == colors.search_active_match_background
    {
        search_match::resolve_active_background(colors)
    } else {
        surface
    }
}

/// Shape cue for a severity. Callers also show text.
/// The severity icon used where only one status channel is present.
///
/// Callers that also show an observation confidence should use
/// [`health_icon`] so the health axis keeps its own shape family.
pub fn severity_icon(severity: Severity) -> IconName {
    health_icon(severity)
}

/// The theme contract.
///
/// "Keep the skin system" is only a promise if a skin *can* be wrong, and a skin
/// is wrong in ways no type checker sees: a role that resolves to transparent
/// black, a divider so quiet it stops dividing, an accent too close to the text
/// it has to be found against. These four checks are what make the promise
/// checkable, and they are the reason the derivation layer exists — without a
/// contract, the only safe response to a missing token is the fallback that made
/// elements disappear.
///
/// Every assertion runs for both appearances, because "dark is done, light is
/// best effort" is the failure mode this product is specifically not allowed.
#[cfg(test)]
mod theme_contract {
    use super::{
        ACCENT_WASH_ALPHA, Appearance, Roles, STATUS_BORDER_ALPHA, STATUS_WASH_ALPHA, StatusColors,
        ThemeColors, border, calculate_contrast_ratio, contrast_ratio, hairline_alpha,
    };
    use gpui_kit::px;

    /// Both appearances, because a contract that only one of them satisfies is
    /// half a contract.
    const APPEARANCES: [Appearance; 2] = [Appearance::Light, Appearance::Dark];

    fn roles(appearance: Appearance) -> &'static Roles {
        super::product_roles(appearance)
    }

    /// The six surfaces a role can be painted on, named.
    ///
    /// All six, in both directions: three of them is the divider's answer to "where
    /// does a panel edge go", and using it as the answer to "where does ink go"
    /// is how `fg.disabled` ended up at 2.87:1 on the terminal canvas while the
    /// test that grades it was reading the other three.
    fn every_surface(roles: &Roles) -> [(&'static str, super::Hsla); 6] {
        [
            ("surface.app", roles.surface_app),
            ("surface.chrome", roles.surface_chrome),
            ("surface.content", roles.surface_content),
            ("surface.raised", roles.surface_raised),
            ("surface.inset", roles.surface_inset),
            ("surface.overlay", roles.surface_overlay),
        ]
    }

    /// Every role, paired with the name a reader would use to complain about it.
    fn every_role(roles: &Roles) -> Vec<(&'static str, super::Hsla)> {
        vec![
            ("surface.app", roles.surface_app),
            ("surface.chrome", roles.surface_chrome),
            ("surface.content", roles.surface_content),
            ("surface.raised", roles.surface_raised),
            ("surface.inset", roles.surface_inset),
            ("surface.overlay", roles.surface_overlay),
            ("fg.primary", roles.fg_primary),
            ("fg.secondary", roles.fg_secondary),
            ("fg.tertiary", roles.fg_tertiary),
            ("fg.disabled", roles.fg_disabled),
            ("border.subtle", roles.border_subtle),
            ("border.base", roles.border_base),
            ("border.strong", roles.border_strong),
            ("accent", roles.accent),
            ("accent.wash", roles.accent_wash),
            ("accent.fg", roles.accent_fg),
            ("status.success", roles.success),
            ("status.success.wash", roles.success_wash),
            ("status.success.border", roles.success_border),
            ("status.warning", roles.warning),
            ("status.warning.wash", roles.warning_wash),
            ("status.warning.border", roles.warning_border),
            ("status.danger", roles.danger),
            ("status.danger.wash", roles.danger_wash),
            ("status.danger.border", roles.danger_border),
            ("status.info", roles.info),
            ("status.info.wash", roles.info_wash),
            ("status.info.border", roles.info_border),
        ]
    }

    /// 1. No role resolves to nothing.
    ///
    /// This is the check the old parser could not pass and the reason the
    /// derivation layer was written. `color_of` turned a missing key into
    /// `Hsla::default()` — transparent black — which paints an element that is
    /// simply not there, with no error anywhere. A role that is a fully
    /// transparent black is the same failure wearing a different hat.
    #[test]
    fn no_role_resolves_to_transparent_black() {
        for appearance in APPEARANCES {
            for (name, color) in every_role(roles(appearance)) {
                assert!(
                    color.a > 0.0,
                    "{appearance:?} {name} is fully transparent: the role fell through instead \
                     of being derived"
                );
                assert!(
                    !(color.a == 0.0 && color.l == 0.0 && color.s == 0.0),
                    "{appearance:?} {name} is `Hsla::default()`, the exact value the derivation \
                     layer exists to replace"
                );
            }
        }
    }

    /// 2. The text ladder clears the floor each rung is held to.
    ///
    /// Primary is body-and-title ink and has to survive an hour of reading, so it
    /// is held to 7:1. Secondary carries a healthy status word, which is the
    /// thing a reader is scanning for, so it is held to the body-text floor.
    /// Tertiary is a count and a group head — present, never the subject.
    ///
    /// All three are checked on every surface text lands on, all six of them, not
    /// just the content plane: a role that is legible on the table and invisible on
    /// the terminal canvas has not solved anything.
    #[test]
    fn the_text_ladder_clears_its_floor_on_every_surface() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            for (surface_name, surface) in every_surface(roles) {
                for (role_name, ink, floor) in [
                    (
                        "fg.primary",
                        roles.fg_primary,
                        super::INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    ("fg.secondary", roles.fg_secondary, super::TEXT_MIN_CONTRAST),
                    (
                        "fg.tertiary",
                        roles.fg_tertiary,
                        border::INTERACTIVE_MIN_CONTRAST,
                    ),
                ] {
                    let measured = contrast_ratio(ink, surface);
                    assert!(
                        measured >= floor,
                        "{appearance:?} {role_name} is {measured:.2}:1 on {surface_name}, under \
                         its {floor}:1 floor"
                    );
                }
            }
        }
    }

    /// 3. The divider is visible, because in the dark appearance it is the
    ///    *only* thing separating two panels.
    ///
    /// The measured finding that produced this design: adjacent dark surfaces
    /// differ by 1.018–1.056:1, all of it below anything a reader can see. The
    /// 1px line is not decoration being kept out of a cleanup — it is the
    /// information boundary. A design where the boundary cannot be seen has no
    /// structure.
    ///
    /// All six surfaces, and the reason is a measurement rather than a principle:
    /// this checked three and the solver solved three, so the two agreed and the
    /// omission was invisible from both ends. `surface.inset` — the terminal
    /// canvas, which `panels::dock` draws a `border.subtle` on directly — came
    /// out at 1.195:1 in Dark. Same shape as the `fg.disabled` miss: two surfaces
    /// nobody looked at, and a floor that quietly stopped being a floor.
    #[test]
    fn the_divider_clears_the_rule_floor() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            for (surface_name, surface) in every_surface(roles) {
                let measured = contrast_ratio(roles.border_subtle, surface);
                assert!(
                    measured >= border::MIN_RULE_CONTRAST,
                    "{appearance:?} border.subtle is {measured:.2}:1 on {surface_name}, under the \
                     {floor}:1 a rule has to clear — the panels on either side of this line are \
                     otherwise indistinguishable",
                    floor = border::MIN_RULE_CONTRAST
                );
            }
            // The three strokes have to be tellable from one another, or a
            // component reaching for the wrong one has no way to know.
            let ordered = [
                ("border.subtle", roles.border_subtle),
                ("border.base", roles.border_base),
                ("border.strong", roles.border_strong),
            ];
            for pair in ordered.windows(2) {
                let (low_name, low) = pair[0];
                let (high_name, high) = pair[1];
                let ratio = contrast_ratio(high, roles.surface_content)
                    / contrast_ratio(low, roles.surface_content).max(f32::MIN_POSITIVE);
                assert!(
                    ratio > 1.05,
                    "{appearance:?} {high_name} and {low_name} are within 5% of each other on \
                     surface.content, so they are two names for one stroke"
                );
            }
        }
    }

    /// 4. The accent is findable.
    ///
    /// It is the selection rail, the one primary button, and the keyboard focus
    /// ring. If it cannot be found against the content plane, the app has no way
    /// to say what is selected.
    #[test]
    fn the_accent_clears_the_interactive_floor() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let measured = contrast_ratio(roles.accent, roles.surface_content);
            assert!(
                measured >= border::INTERACTIVE_MIN_CONTRAST,
                "{appearance:?} accent is {measured:.2}:1 on surface.content, under the {floor}:1 \
                 an interactive mark has to clear",
                floor = border::INTERACTIVE_MIN_CONTRAST
            );
            // The label on a filled accent button.
            //
            // The product theme names `accent.fg: #FFFFFF` on `#4F8CFF`, which
            // measures 3.22:1 — above the 3:1 every other mark in the design is
            // held to, below the 4.5:1 a body-sized label would want. The spec
            // chose the white, so the white is what ships; this holds it to the
            // floor the design already applies to the accent rail and the focus
            // ring, which is the honest promise for an accent this bright. See
            // the delivery notes: a deeper accent, or ink solved per polarity,
            // would clear 4.5:1 and is a spec decision, not a token one.
            let label = contrast_ratio(roles.accent_fg, roles.accent);
            assert!(
                label >= border::INTERACTIVE_MIN_CONTRAST,
                "{appearance:?} accent.fg is {label:.2}:1 on accent, under the {floor}:1 a mark \
                 drawn on the accent has to clear",
                floor = border::INTERACTIVE_MIN_CONTRAST
            );
        }
    }

    /// The surface ladder is ordered by height where height is carried by
    /// lightness — and the light appearance is not.
    ///
    /// The design splits the two appearances deliberately: dark carries height
    /// with a brightness ladder, light carries it with a stroke and a shadow,
    /// because two steps of near-white on white are not a difference a reader can
    /// see. So in light, `raised` and `overlay` are *meant* to sit on the same
    /// white as `content`, and the second half of the promise is that they carry
    /// the difference some other way.
    ///
    /// Checking the light ladder for a monotone ramp would be checking against a
    /// rule the spec explicitly does not have, so the test states the rule each
    /// appearance actually has: dark is a ramp, light is a flat plane whose
    /// raised steps are told apart by their border and their shadow.
    #[test]
    fn the_surface_ladder_matches_the_mechanism_its_appearance_uses() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            match appearance {
                Appearance::Dark => {
                    let ladder = [
                        ("inset", roles.surface_inset),
                        ("app", roles.surface_app),
                        ("chrome", roles.surface_chrome),
                        ("content", roles.surface_content),
                        ("raised", roles.surface_raised),
                        ("overlay", roles.surface_overlay),
                    ];
                    for pair in ladder.windows(2) {
                        let (low_name, low) = pair[0];
                        let (high_name, high) = pair[1];
                        assert!(
                            high.l > low.l,
                            "{appearance:?} surface.{high_name} ({}) is not above \
                             surface.{low_name} ({}), so the dark ladder is not a ladder",
                            high.l,
                            low.l
                        );
                    }
                }
                Appearance::Light => {
                    // Content is the plane; the raised steps may share it, but
                    // they must not fall *below* it, or a card reads as a hole.
                    for (name, surface) in [
                        ("raised", roles.surface_raised),
                        ("overlay", roles.surface_overlay),
                    ] {
                        assert!(
                            surface.l >= roles.surface_content.l,
                            "Light surface.{name} is darker than surface.content, so a raised \
                             surface reads as pressed in"
                        );
                    }
                    // And they have to be told apart somehow: a border that
                    // exists, and an overlay that is at least as high as content.
                    assert!(
                        contrast_ratio(roles.border_base, roles.surface_raised)
                            >= border::MIN_RULE_CONTRAST,
                        "Light carries height with a stroke, and the stroke on surface.raised \
                         measures {:.2}:1",
                        contrast_ratio(roles.border_base, roles.surface_raised)
                    );
                }
            }
        }
    }

    ///
    /// Two of them resolving to the same value means a Failed and a Pending read
    /// identically, which is the one thing the status column exists to prevent.
    #[test]
    fn the_status_channels_are_tellable_from_each_other() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let channels = [
                ("success", roles.success),
                ("warning", roles.warning),
                ("danger", roles.danger),
                ("info", roles.info),
            ];
            for (a_index, (a_name, a)) in channels.iter().enumerate() {
                for (b_name, b) in channels.iter().skip(a_index + 1) {
                    assert_ne!(
                        a, b,
                        "{appearance:?} status.{a_name} and status.{b_name} are the same colour, \
                         so the two states are indistinguishable"
                    );
                }
            }
            // A wash is a tint: it has to stay quieter than the ink it sits
            // under, or the text on it has nothing left to be.
            for (name, wash, ink) in [
                ("success", roles.success_wash, roles.success),
                ("warning", roles.warning_wash, roles.warning),
                ("danger", roles.danger_wash, roles.danger),
            ] {
                let over_content = contrast_ratio(wash, roles.surface_content);
                let ink_over_content = contrast_ratio(ink, roles.surface_content);
                assert!(
                    over_content < ink_over_content,
                    "{appearance:?} status.{name}.wash ({over_content:.2}:1) is louder than the \
                     ink meant to sit on it ({ink_over_content:.2}:1)"
                );
            }
        }
    }

    /// A status word on a wash of its own channel clears the body-text floor.
    ///
    /// This is the combination the app actually ships, four times over: the
    /// table's inline error bar (`danger@8%` under 12/400 `danger` text), the
    /// dock's follow-paused note, the Overview's issue chips, and the pending
    /// delete badge. Every one of them puts a status-coloured label on a wash of
    /// the same channel, which `UI-SPEC` §4.15 spells out as `danger@8%` plus
    /// `12/400 danger`.
    ///
    /// It was unmeasured, and the way it goes wrong is quiet: a wash is 8–12% of
    /// the channel over a near-white plane, so the composite is only a little
    /// off the surface under it. A channel that clears 4.5:1 on white and 4.27:1
    /// on its own wash passes every isolated measurement there is, and 4.27:1 is
    /// a 12px label below AA. The check above only ever compared the wash to the
    /// *content plane* — a statement that the wash is quiet — and never the ink to
    /// the wash, which is the statement that matters.
    ///
    /// All four channels, both appearances, and every host a wash is laid down on.
    ///
    /// The ink read here is the channel's **word**, not its mark: a 12px label is
    /// body text and is held to the body floor, while a 6px dot is a graphic and
    /// is held to `channel_mark` on the plain surfaces only (see
    /// [`Roles::refine_channels`]). A mark is never printed on a wash of its own
    /// channel, so grading it there would price a surface the app does not ship
    /// and would push Dark `danger` off the `#F2555A` `UI-SPEC` §1.5 fixes.
    #[test]
    fn a_status_word_on_its_own_wash_clears_the_body_text_floor() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            for (name, ink, wash) in [
                ("success", roles.success_word, roles.success_wash),
                ("warning", roles.warning_word, roles.warning_wash),
                ("danger", roles.danger_word, roles.danger_wash),
                ("info", roles.info_word, roles.info_wash),
            ] {
                for (host, surface) in [
                    ("surface.content", roles.surface_content),
                    ("surface.chrome", roles.surface_chrome),
                    ("surface.raised", roles.surface_raised),
                    ("surface.overlay", roles.surface_overlay),
                ] {
                    let measured = contrast_ratio(ink, super::composite_surface(surface, wash));
                    assert!(
                        measured >= super::Floors::of(false).secondary,
                        "{appearance:?} status.{name} on its own wash over {host} is \
                         {measured:.2}:1, under the {}:1 a status word has to clear — this is \
                         the ink-on-wash combination the table error bar, the dock note and the \
                         Overview issue chips all paint",
                        super::Floors::of(false).secondary
                    );
                }
            }
        }
    }

    /// No status channel is the accent, or close enough to be one.
    ///
    /// The accent is the scarce resource: `UI-REDESIGN` §4.3 lets a screen spend
    /// it on two things, the current selection and the one primary action. A
    /// status channel that *is* the accent spends it a third time, and quietly,
    /// because it looks right in isolation.
    ///
    /// Light's `info` was the accent's own value — `#2F6FED`, character for
    /// character — so `status.info.wash` and `accent.wash` were one hue at two
    /// alphas, and an info chip and a selected row were the same blue on the
    /// same surface. Dark's was three degrees of hue away, which is not a
    /// second channel but the first one again. `UI-SPEC` §1.5 names a Light
    /// value for every other channel and says nothing about `info`, so this was
    /// a gap rather than a decision, and nothing in the contract looked at it:
    /// [`the_status_channels_are_tellable_from_each_other`] compares the four
    /// channels to *each other*, and each of them was telling the truth.
    ///
    /// Both halves, because the ink alone is not the bug: a chip is a wash, so
    /// the wash has to be a different colour too. And a separation threshold
    /// rather than an inequality, because `assert_ne!` is satisfied by a hue
    /// three degrees off and three degrees is the whole failure.
    #[test]
    fn no_status_channel_is_the_accent() {
        /// The smallest hue gap, in degrees, that still reads as a different
        /// colour; below it two channels are one channel at two lightnesses.
        ///
        /// The shipped pairs are 27 degrees apart at the tightest (Light `info`
        /// against the accent), so twenty is the floor with the margin to spare.
        /// The lightness arm is there because a large lightness step separates as
        /// well as a hue step does, and a channel is allowed to be a darker
        /// member of a family the accent also lives in.
        const MIN_HUE_GAP: f32 = 20.;
        const MIN_LIGHTNESS_GAP: f32 = 0.12;

        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let accent = roles.accent;
            for (name, ink, wash) in [
                ("success", roles.success, roles.success_wash),
                ("warning", roles.warning, roles.warning_wash),
                ("danger", roles.danger, roles.danger_wash),
                ("info", roles.info, roles.info_wash),
            ] {
                assert_ne!(
                    ink, accent,
                    "{appearance:?} status.{name} is the accent, so a state and the current \
                     selection are the same colour"
                );
                // `assert_ne!` on the wash is the half that matters on screen: a
                // chip is its background, not its label.
                assert_ne!(
                    super::composite_surface(roles.surface_content, wash),
                    super::composite_surface(roles.surface_content, roles.accent_wash),
                    "{appearance:?} status.{name}.wash is the accent wash on the content plane, \
                     so a {name} chip and a selected row are the same blue"
                );
                let mut gap = (ink.h - accent.h).abs() * 360.;
                if gap > 180. {
                    gap = 360. - gap;
                }
                let lightness = (ink.l - accent.l).abs();
                assert!(
                    gap >= MIN_HUE_GAP || lightness >= MIN_LIGHTNESS_GAP,
                    "{appearance:?} status.{name} sits {gap:.0} degrees and {lightness:.2} \
                     lightness from the accent, under the {MIN_HUE_GAP} or {MIN_LIGHTNESS_GAP} \
                     that makes two channels two colours"
                );
            }
        }
    }

    /// A skin that knows nothing about the new roles still renders the design.
    ///
    /// This is the derivation layer's whole reason to exist, and the one thing
    /// that cannot be checked by looking at the product theme: the product names
    /// every one of these keys, so the only way to find out what a theme that
    /// names *none* of them resolves to is to hand the parser an empty map. If a
    /// missing key produced transparent black the redesign would have quietly
    /// deleted most of the app for anyone who picked a different skin.
    #[test]
    fn a_theme_with_no_product_roles_still_resolves_every_role() {
        let empty = serde_json::Map::new();
        let colors = ThemeColors::parse(&empty);
        let status = StatusColors::parse(&empty);
        let roles = Roles::parse(&empty, &colors, &status, Appearance::Dark);
        for (name, color) in every_role(&roles) {
            assert!(
                color.a > 0.0,
                "a bare theme left {name} transparent, so the derivation layer did not fire"
            );
        }
        // Derived, not merely present: the ladder has to still be a ladder and
        // the ink still has to have a floor. A layer that filled every hole with
        // the same colour would pass the test above and fail the product.
        let ladder = [
            roles.surface_inset,
            roles.surface_app,
            roles.surface_chrome,
            roles.surface_content,
            roles.surface_raised,
            roles.surface_overlay,
        ];
        for pair in ladder.windows(2) {
            assert!(
                pair[1].l > pair[0].l,
                "the derived surface ladder is flat: {} -> {}",
                pair[0].l,
                pair[1].l
            );
        }
        assert!(
            contrast_ratio(roles.fg_primary, roles.surface_content) >= super::TEXT_MIN_CONTRAST,
            "the derived primary ink does not clear the body-text floor on the derived content \
             surface"
        );
        assert!(
            contrast_ratio(roles.border_subtle, roles.surface_content) >= border::MIN_RULE_CONTRAST,
            "the derived divider does not clear the rule floor"
        );
    }

    /// A theme that spells a role as fully transparent is treated as absent.
    ///
    /// `#00000000` is a value a theme author writes meaning "nothing", and it is
    /// the exact thing that must not reach a renderer: the element is drawn, and
    /// nothing about it says why it cannot be seen.
    #[test]
    fn a_role_written_as_transparent_is_derived_instead() {
        let mut style = serde_json::Map::new();
        style.insert("fg.primary".to_owned(), serde_json::json!("#00000000"));
        style.insert("accent".to_owned(), serde_json::json!("#4F8CFF1F"));
        let colors = ThemeColors::parse(&style);
        let status = StatusColors::parse(&style);
        let roles = Roles::parse(&style, &colors, &status, Appearance::Dark);
        assert!(
            roles.fg_primary.a > 0.0,
            "fg.primary was written transparent and was taken at face value"
        );
        assert_eq!(
            roles.accent.a, 1.0,
            "a semi-transparent accent is not an accent; a fill needs an opaque colour"
        );
    }

    /// The washes are the alphas the design names, not whatever the theme said.
    ///
    /// This is a regression guard on the *shape* of the derivation: a wash is a
    /// fraction of its own channel, and a theme that supplies its own wash value
    /// can move it — but the default cannot drift away from the token the whole
    /// interface is tuned against.
    #[test]
    fn the_derived_washes_use_the_documented_alphas() {
        let empty = serde_json::Map::new();
        let colors = ThemeColors::parse(&empty);
        let status = StatusColors::parse(&empty);
        let roles = Roles::parse(&empty, &colors, &status, Appearance::Dark);
        assert!(
            (roles.accent_wash.a - ACCENT_WASH_ALPHA).abs() < 0.001,
            "the derived accent wash is {} not the {ACCENT_WASH_ALPHA} the selection states are \
             tuned against",
            roles.accent_wash.a
        );
        for (name, wash, channel) in [
            ("success", roles.success_wash, roles.success),
            ("warning", roles.warning_wash, roles.warning),
            ("danger", roles.danger_wash, roles.danger),
        ] {
            assert!(
                (wash.a - STATUS_WASH_ALPHA).abs() < 0.001,
                "the derived {name} wash is {} not {STATUS_WASH_ALPHA}",
                wash.a
            );
            assert!((wash.h - channel.h).abs() < 0.001 && (wash.s - channel.s).abs() < 0.001);
            assert!(
                (hairline_alpha(&roles, name) - STATUS_BORDER_ALPHA).abs() < 0.001,
                "the derived {name} hairline is not {STATUS_BORDER_ALPHA}"
            );
        }
    }

    /// A role that is a floor, not a value, has to clear that floor in both
    /// appearances.
    ///
    /// `fg_disabled` was the one role read straight out of the theme with no
    /// solve behind it, and K8s Studio's value measures 1.99:1 against a white
    /// content surface — a quarter of the floor the contract holds every other
    /// role to. The cost was not abstract: a managed-field lock badge nobody could
    /// see, and a log timestamp nobody could read. A quiet role that cannot be
    /// seen is not quiet, it is absent.
    ///
    /// All six surfaces, and both halves of the promise: the floor, and the step
    /// below the role above it. Checked on three it read 3.35:1 on the content
    /// plane and passed while sitting at 2.87:1 on the terminal canvas.
    #[test]
    fn the_quietest_ink_is_still_readable() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            for (surface_name, surface) in every_surface(roles) {
                let measured = contrast_ratio(roles.fg_disabled, surface);
                assert!(
                    measured >= super::DISABLED_TEXT_MIN_CONTRAST,
                    "{appearance:?} fg.disabled is {measured:.2}:1 on {surface_name}, under the \
                     {floor}:1 that a role below fg.tertiary still has to clear",
                    floor = super::DISABLED_TEXT_MIN_CONTRAST
                );
                // And it must stay *below* the role above it, or the ladder has
                // two rungs at one value and the quietest one means nothing.
                let above = contrast_ratio(roles.fg_tertiary, surface);
                assert!(
                    measured < above,
                    "{appearance:?} fg.disabled ({measured:.2}:1) is not quieter than \
                     fg.tertiary ({above:.2}:1) on {surface_name}, so they are one role"
                );
                // Ordering is not enough. A pair one solver step apart is ordered
                // and still one colour on screen, which is what the light pair
                // became at 3.85:1 against 3.91:1 — and a strict `<` assertion
                // passed on it. The margin is the actual requirement, so the
                // margin is what is asserted.
                assert!(
                    measured <= above * super::QUIET_MAX_RATIO,
                    "{appearance:?} fg.disabled ({measured:.3}:1) is not a step quieter than \
                     fg.tertiary ({above:.3}:1) on {surface_name}: two rungs of one ladder \
                     {:.0}% apart are one colour",
                    (measured / above - 1.0) * 100.0
                );
            }
        }
    }

    /// Healthy is grey. A healthy thing must not spend a status channel.
    ///
    /// This is the product's central colour decision, and it is the one most
    /// likely to be undone by accident: someone reads `status.success` in a
    /// component, it looks right in isolation, and a table of 10,000 green rows
    /// ships. Five independent components each made that decision for themselves
    /// before this was pinned here.
    #[test]
    fn health_does_not_spend_a_status_channel() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let healthy = super::severity_role(super::Severity::Success, appearance);
            for (name, channel) in [
                ("success", roles.success),
                ("warning", roles.warning),
                ("danger", roles.danger),
                ("info", roles.info),
            ] {
                assert_ne!(
                    healthy, channel,
                    "{appearance:?} a healthy object resolves to status.{name} — healthy is \
                     spending a status channel, and every healthy row will be a signal"
                );
            }
            // And it is legible where it stands, so "quiet" is not "absent".
            assert!(
                contrast_ratio(healthy, roles.surface_content) >= super::TEXT_MIN_CONTRAST,
                "{appearance:?} the healthy ink is not readable on the content surface"
            );
        }
    }

    /// One mapping, not five.
    ///
    /// The five components that each had their own answer are the reason this
    /// exists; the assertion that keeps it existing is that a *screenshot* of a
    /// healthy cluster must be greyscale. If a component ever reintroduces a
    /// local severity-to-ink map, this is the test that says so.
    #[test]
    fn the_severity_mapping_is_the_only_one() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let expected = [
                (super::Severity::Success, roles.fg_secondary),
                (super::Severity::Warning, roles.warning),
                (super::Severity::Error, roles.danger),
                (super::Severity::Info, roles.info),
                (super::Severity::Neutral, roles.fg_primary),
                (super::Severity::Muted, roles.fg_tertiary),
            ];
            for (severity, role) in expected {
                assert_eq!(
                    super::severity_role(severity, appearance),
                    role,
                    "{appearance:?} {severity:?} no longer resolves to its role"
                );
            }
        }
    }

    /// The Pending grade is the same number everywhere it is read.
    ///
    /// The table graded it locally, and then the sidebar, the Overview's issue
    /// chips and the status bar each made their own call — so one pod was
    /// `danger` in the table and `warning` in the sidebar, on the same screen.
    #[test]
    fn the_pending_grade_has_three_rungs_and_a_missing_age_is_not_one_of_them() {
        use super::Severity as S;
        use std::time::Duration;
        let at =
            |secs: u64| super::pod_severity_with_age("Pending", Some(Duration::from_secs(secs)));
        assert_eq!(
            at(0),
            S::Warning,
            "a brand new Pending is queued, not healthy"
        );
        assert_eq!(at(29), S::Warning, "under 30s is still ordinary scheduling");
        assert_eq!(
            at(31),
            S::Warning,
            "over 30s is a queue that has stopped moving"
        );
        assert_eq!(at(31), at(60), "30s to 5m is one band, not a ramp");
        assert_eq!(at(299), S::Warning);
        assert_eq!(
            at(301),
            S::Error,
            "past 5m nothing is coming and it is stuck"
        );
        assert_eq!(at(86_400), S::Error, "a day-old Pending is not a Pending");
        // A cluster that sent no creation timestamp has told us nothing, and
        // colouring on the strength of a field that is absent is the same lie as
        // reading `Unknown` as healthy.
        assert_eq!(
            super::pod_severity_with_age("Pending", None),
            S::Warning,
            "a missing timestamp must not upgrade a Pending into a stuck one"
        );
        // Only the queued states are graded. A week-old Running pod is the
        // healthiest thing on the screen.
        assert_eq!(
            super::pod_severity_with_age("Running", Some(Duration::from_secs(604_800))),
            S::Success,
            "age must not grade a state that is not waiting on anything"
        );
        assert_eq!(
            super::pod_severity_with_age("Failed", Some(Duration::from_secs(1))),
            S::Error
        );
    }

    /// Every role, in both appearances, that the design names a value for.
    #[test]
    fn the_scale_has_no_holes() {
        // Nine text tokens, ten spaces, six radii, four durations: the sizes the
        // self-check list says are the only ones allowed. A number that appears
        // inline instead is a number that will drift from these.
        assert_eq!(super::size::ROW, px(32.));
        assert_eq!(super::size::ROW_DENSE, px(24.));
        assert_eq!(super::size::ROW_NORMAL, px(28.));
        assert_eq!(super::size::ROW_COMFORT, px(32.));
        assert_eq!(super::size::TITLE_BAR, px(40.));
        assert_eq!(super::size::RESOURCE_HEADER, px(40.));
        assert_eq!(super::size::OPEN_VIEWS, px(28.));
        assert_eq!(super::size::STATUS_BAR, px(24.));
        assert_eq!(super::size::TABLE_HEADER, px(32.));
        assert_eq!(super::size::SUMMARY_STRIP, px(32.));
        assert_eq!(super::size::SELECTION_BAR, px(40.));
        assert_eq!(super::size::SIDEBAR_DEFAULT, px(236.));
        assert_eq!(super::size::SIDEBAR_RAIL, px(48.));
        assert_eq!(super::size::INSPECTOR_DEFAULT, px(352.));
        assert_eq!(super::size::FILTER_BOX, px(32.));
        assert_eq!(super::size::KIND_ICON, px(14.));
        assert_eq!(super::size::STATUS_DOT, px(6.));
        assert_eq!(super::size::SELECTION_RAIL, px(2.));
        assert_eq!(super::size::ICON_BUTTON, px(24.));
        assert_eq!(super::size::DOCK_TABS, px(28.));
        assert_eq!(super::size::DOCK_TOOLBAR, px(28.));
        assert_eq!(super::size::DOCK_COLLAPSE_BELOW, 760.);
        assert_eq!(super::size::INSPECTOR_FLOAT_BELOW, 1000.);
        assert_eq!(super::size::WINDOW_MIN, (960., 640.));
    }

    /// The type scale, the spacing scale and the corner scale are the only ones.
    #[test]
    fn the_scale_is_a_scale() {
        use gpui_kit::px;
        // Nine sizes, and the four levels the design says are real.
        let sizes = [
            super::text::DISPLAY,
            super::text::TITLE,
            super::text::SUBTITLE,
            super::text::BODY,
            super::text::LABEL,
            super::text::CAPTION,
            super::text::MICRO,
            super::text::MONO_XS,
            super::text::MONO_SM,
        ];
        assert_eq!(sizes.len(), 9);
        assert_eq!(super::text::DISPLAY, px(28.));
        assert_eq!(super::text::TITLE, px(15.));
        assert_eq!(super::text::SUBTITLE, px(13.));
        assert_eq!(super::text::BODY, px(13.));
        assert_eq!(super::text::LABEL, px(12.));
        assert_eq!(super::text::CAPTION, px(11.));
        assert_eq!(super::text::MICRO, px(10.));
        assert_eq!(super::text::MONO_XS, px(11.));
        assert_eq!(super::text::MONO_SM, px(12.));
        // Body and subtitle share a size and are told apart by weight, which is
        // the only place the design allows two roles on one size — and it is why
        // the two weights exist.
        assert_eq!(super::text::SUBTITLE, super::text::BODY);
        assert_ne!(super::text::MEDIUM, super::text::REGULAR);

        // Ten spaces on a 4pt grid, plus the one optical gap.
        let spaces = [
            super::space::XXS,
            super::space::XS,
            super::space::SM,
            super::space::MD,
            super::space::LG,
            super::space::LG_PLUS,
            super::space::XL,
            super::space::XXL,
            super::space::XXXL,
        ];
        assert_eq!(spaces.len(), 9);
        assert_eq!(super::space::XXS, px(2.));
        assert_eq!(super::space::XS, px(4.));
        assert_eq!(super::space::SM, px(8.));
        assert_eq!(super::space::MD, px(12.));
        assert_eq!(super::space::LG, px(16.));
        assert_eq!(super::space::LG_PLUS, px(20.));
        assert_eq!(super::space::XL, px(24.));
        assert_eq!(super::space::XXL, px(32.));
        assert_eq!(super::space::XXXL, px(40.));
        assert_eq!(super::space::ICON, px(6.));

        // Six corners, ascending, and nothing between them.
        let radii = [
            super::radius::XS,
            super::radius::SM,
            super::radius::MD,
            super::radius::LG,
            super::radius::XL,
        ];
        assert_eq!(radii.len(), 5);
        for pair in radii.windows(2) {
            assert!(pair[1] > pair[0], "the corner scale is not ascending");
        }
        assert_eq!(super::radius::XS, px(3.));
        assert_eq!(super::radius::SM, px(4.));
        assert_eq!(super::radius::MD, px(6.));
        assert_eq!(super::radius::LG, px(8.));
        assert_eq!(super::radius::XL, px(12.));

        // Four durations, and a view switch is not one of them.
        assert_eq!(super::motion::INSTANT, std::time::Duration::from_millis(0));
        assert_eq!(super::motion::FAST, std::time::Duration::from_millis(90));
        assert_eq!(super::motion::NORMAL, std::time::Duration::from_millis(140));
        assert_eq!(super::motion::SLOW, std::time::Duration::from_millis(200));
    }

    /// The state layer is a role plus a state, and never a colour of its own.
    ///
    /// Hover, press and focus are the three states every interactive element has
    /// and the three that were independently re-derived in five files. One
    /// definition, and the alphas are asserted so a component cannot quietly
    /// invent a fourth shade of hover.
    #[test]
    fn component_state_is_a_role_plus_a_state() {
        use super::state;
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let hover = state::hover_on(roles.surface_content, roles.fg_primary);
            let press = state::press_on(roles.surface_content, roles.fg_primary);
            // Hover is a few percent of the local ink: present, not a state
            // change a reader notices as a change of colour.
            let hover_delta = calculate_contrast_ratio(hover, roles.surface_content);
            assert!(
                (1.0..1.35).contains(&hover_delta),
                "{appearance:?} a hover is {hover_delta:.3}:1 against its surface, which is \
                 either invisible or a colour change rather than a depth change"
            );
            // Press is the same tint stepped once, and strictly deeper.
            assert!(
                calculate_contrast_ratio(press, roles.surface_content) > hover_delta,
                "{appearance:?} a press is not deeper than a hover, so the state has no signal"
            );
            // Disabled ink is derived, so it is never the same value as the label
            // it is replacing.
            let disabled = super::composite_surface(
                roles.surface_content,
                roles.fg_secondary.opacity(state::DISABLED_ALPHA),
            );
            assert_ne!(
                disabled, roles.surface_content,
                "{appearance:?} a disabled label composites to the surface, so the control is \
                 not drawn at all"
            );
            // The focus ring is the accent, and its glow is the same hue at a
            // fraction — one colour, two roles.
            assert_eq!(
                state::glow_for(roles.accent),
                roles.accent.opacity(state::FOCUS_RING_ALPHA),
                "the focus glow is not the accent's own fraction of itself"
            );
        }
    }

    /// The theme file names the keys the code reads, and only those.
    ///
    /// The roles hide this: a key nothing reads and a key that is read and then
    /// solved look identical once they are a colour. Two keys here had drifted
    /// that way, in opposite directions. `confidence.unknown` and
    /// `confidence.stale` were named in the file *and* described in the module
    /// doc as the source of the channel, while `confidence::foreground` read two
    /// `const [u32; 2]` tables nothing else shared — so the file's values were a
    /// second, unread spelling of a channel the code owned. And `unreachable` was
    /// named in the file while `StatusColors::parse` read `hint` for it under a
    /// comment calling the key deliberately dead; the state it names has a real
    /// home already — `Confidence::Unknown` for the ink, the shape and the
    /// caption, and the error channel for a panel whose API server is gone, which
    /// is what `docs/mockup` screen 5 draws.
    ///
    /// So the two confidence keys are live and come from the file, and there is
    /// no fifth "unreachable" channel to spell. Asserted on the raw map, because
    /// the roles cannot tell the difference.
    #[test]
    fn the_theme_file_names_exactly_the_keys_the_app_reads() {
        for appearance in APPEARANCES {
            for key in ["confidence.unknown", "confidence.stale"] {
                assert!(
                    super::product_theme_names(appearance, key),
                    "{appearance:?} the file does not name {key}, so the confidence channel has \
                     to invent it in code and a skin has nothing to change"
                );
            }
            for key in super::DEAD_THEME_KEYS {
                assert!(
                    !super::product_theme_names(appearance, key),
                    "{appearance:?} the file still names {key} and nothing reads it, so it is a \
                     second spelling of a colour the app already has"
                );
            }
        }
    }

    /// Turning the setting off has to leave the theme exactly as it was.
    ///
    /// The app refines the active appearance on every theme change, so the only
    /// thing keeping the shipped appearance pinned to the values the contract
    /// grades is that a second pass with the setting off has nothing left to do.
    /// The first pass runs inside `ThemeBridge::parse`; this one is the proof that
    /// it converged, and that switching Light -> Dark -> Light cannot drift.
    #[test]
    fn refining_with_the_setting_off_changes_nothing() {
        for appearance in APPEARANCES {
            let mut roles = *roles(appearance);
            let before = roles;
            roles.refine(false);
            assert_eq!(
                roles, before,
                "{appearance:?} the default pass moved a role, so the shipped appearance is not \
                 the one the contract grades"
            );
            roles.refine(false);
            assert_eq!(
                roles, before,
                "{appearance:?} the default pass is not idempotent, so switching Light -> Dark -> \
                 Light drifts"
            );
        }
    }

    /// The product roles clear the standard floors *before* any refinement.
    ///
    /// This is the other end of the same promise. The bare-theme test checks that
    /// a skin naming nothing still resolves every role; this checks that a skin
    /// naming all of them comes out above every floor, which is what makes
    /// `refine(false)` converge rather than have work left to do on every
    /// appearance change.
    #[test]
    fn the_product_roles_clear_the_standard_floors_out_of_the_parse() {
        for appearance in APPEARANCES {
            let roles = roles(appearance);
            let floors = super::Floors::of(false);
            for (surface_name, surface) in every_surface(roles) {
                for (name, ink, floor) in [
                    ("fg.primary", roles.fg_primary, floors.primary),
                    ("fg.secondary", roles.fg_secondary, floors.secondary),
                    ("fg.tertiary", roles.fg_tertiary, floors.tertiary),
                    ("fg.disabled", roles.fg_disabled, floors.disabled),
                    ("status.success", roles.success, floors.secondary),
                    ("status.warning", roles.warning, floors.secondary),
                    ("status.danger", roles.danger, floors.secondary),
                    ("status.info", roles.info, floors.secondary),
                ] {
                    let measured = contrast_ratio(ink, surface);
                    assert!(
                        measured >= floor,
                        "{appearance:?} {name} is {measured:.2}:1 on {surface_name} out of the \
                         parse, under its {floor}:1 floor, so the default pass has work to do"
                    );
                }
            }
        }
    }
}

/// The hairline alpha one status channel derived, so the wash check can read it
/// without a fourth accessor on the role layer.
#[cfg(test)]
fn hairline_alpha(roles: &Roles, channel: &str) -> f32 {
    match channel {
        "success" => roles.success_border.a,
        "warning" => roles.warning_border.a,
        "danger" => roles.danger_border.a,
        other => unreachable!("{other} is not a status channel"),
    }
}

/// The role a severity resolves to, for one appearance, without an `App`.
///
/// The mapping is a pure function of the roles, and a test that has to spin up
/// an app to check it is a test that will not be written.
#[cfg(test)]
fn severity_role(severity: Severity, appearance: Appearance) -> Hsla {
    let roles = product_roles(appearance);
    match severity {
        Severity::Success => roles.fg_secondary,
        Severity::Warning => roles.warning,
        Severity::Error => roles.danger,
        Severity::Info => roles.info,
        Severity::Neutral => roles.fg_primary,
        Severity::Muted => roles.fg_tertiary,
    }
}

/// Whether the product theme file names `key`, for one appearance.
///
/// The roles hide this: a key nothing reads and a key that is read and then
/// solved look identical once they are a colour. And that is exactly the class of
/// bug this file has produced twice — `confidence.*` named in the file and in a
/// doc comment while the code read two `const` tables instead, and `unreachable`
/// named in the file while the code read `hint` for it and called the key
/// deliberately dead. So the question is asked of the *file*, not of the roles.
#[cfg(test)]
fn product_theme_names(appearance: Appearance, key: &str) -> bool {
    static FILE: std::sync::LazyLock<serde_json::Value> =
        std::sync::LazyLock::new(|| serde_json::from_str(PRODUCT_THEME_JSON).expect("theme JSON"));
    let wanted = match appearance {
        Appearance::Light => "K8s Studio Light",
        Appearance::Dark => "K8s Studio Dark",
    };
    FILE["themes"]
        .as_array()
        .expect("theme list")
        .iter()
        .find(|theme| theme["name"].as_str() == Some(wanted))
        .and_then(|theme| theme.get("style"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(|style| style.contains_key(key))
}

/// A review snapshot of the derived roles, exported from here rather than
/// re-derived anywhere else.
///
/// The derivation is five walks over lightness and a bisection over alpha, and a
/// second implementation of it in a review script is a second source of truth that
/// is *wrong* rather than merely stale: the first one this crate used read
/// `rgb_to_hls`'s third slot as saturation, so every derived ink came back a
/// saturated blue, and it solved three surfaces where this solves six. Both times
/// the mistake had to be caught by a Rust test, which is the definition of a tool
/// that cannot be trusted.
///
/// So the numbers leave from the only place they are computed. A review harness
/// reads this file and renders it; it never derives a colour of its own. Set
/// `K8S_REVIEW_JSON` to a path to write one.
#[cfg(test)]
fn review_snapshot() -> serde_json::Value {
    use serde_json::json;

    use border::{INTERACTIVE_MIN_CONTRAST, MIN_RULE_CONTRAST};

    let hex = |color: Hsla| {
        let rgba: gpui_kit::Rgba = color.into();
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            channel(rgba.r),
            channel(rgba.g),
            channel(rgba.b),
            channel(rgba.a)
        )
    };

    let mut appearances = serde_json::Map::new();
    for appearance in [Appearance::Light, Appearance::Dark] {
        let roles = product_roles(appearance);
        let increased = increased_roles(appearance);
        let mut entries = serde_json::Map::new();
        for (name, color) in [
            ("surface.app", roles.surface_app),
            ("surface.chrome", roles.surface_chrome),
            ("surface.content", roles.surface_content),
            ("surface.raised", roles.surface_raised),
            ("surface.inset", roles.surface_inset),
            ("surface.overlay", roles.surface_overlay),
            ("fg.primary", roles.fg_primary),
            ("fg.secondary", roles.fg_secondary),
            ("fg.tertiary", roles.fg_tertiary),
            ("fg.disabled", roles.fg_disabled),
            ("border.subtle", roles.border_subtle),
            ("border.base", roles.border_base),
            ("border.strong", roles.border_strong),
            ("accent", roles.accent),
            ("accent.wash", roles.accent_wash),
            ("accent.fg", roles.accent_fg),
            ("success", roles.success),
            ("success.wash", roles.success_wash),
            ("success.border", roles.success_border),
            ("warning", roles.warning),
            ("warning.wash", roles.warning_wash),
            ("warning.border", roles.warning_border),
            ("danger", roles.danger),
            ("danger.wash", roles.danger_wash),
            ("danger.border", roles.danger_border),
            ("info", roles.info),
            ("info.wash", roles.info_wash),
            ("info.border", roles.info_border),
            (
                "severity:Success",
                severity_role(Severity::Success, appearance),
            ),
            (
                "severity:Warning",
                severity_role(Severity::Warning, appearance),
            ),
            ("severity:Error", severity_role(Severity::Error, appearance)),
            ("severity:Info", severity_role(Severity::Info, appearance)),
            (
                "severity:Neutral",
                severity_role(Severity::Neutral, appearance),
            ),
            ("severity:Muted", severity_role(Severity::Muted, appearance)),
        ] {
            entries.insert(name.to_owned(), json!(hex(color)));
        }
        // The surfaces a role is *painted on* rather than named after, so a
        // review sheet can show the ink where it is read rather than only where
        // it was declared.
        for (name, wash) in [
            ("wash:success", roles.success_wash),
            ("wash:warning", roles.warning_wash),
            ("wash:danger", roles.danger_wash),
            ("wash:info", roles.info_wash),
            ("wash:accent", roles.accent_wash),
        ] {
            for (host, base) in [
                ("content", roles.surface_content),
                ("chrome", roles.surface_chrome),
                ("raised", roles.surface_raised),
                ("overlay", roles.surface_overlay),
            ] {
                entries.insert(
                    format!("{name}@{host}"),
                    json!(hex(composite_surface(base, wash))),
                );
            }
        }
        // Every pair the contract grades, measured here so the export and the
        // assertions cannot disagree.
        let mut measurements = Vec::new();
        let mut measure = |label: String, ink: Hsla, base: Hsla, floor: f32| {
            let ratio = contrast_ratio(ink, base);
            measurements.push(json!({
                "pair": label,
                "ratio": (ratio * 1000.0).round() / 1000.0,
                "floor": floor,
                "passes": ratio >= floor,
            }));
        };
        for (surface_name, surface) in [
            ("surface.app", roles.surface_app),
            ("surface.chrome", roles.surface_chrome),
            ("surface.content", roles.surface_content),
            ("surface.raised", roles.surface_raised),
            ("surface.inset", roles.surface_inset),
            ("surface.overlay", roles.surface_overlay),
        ] {
            measure(
                format!("fg.primary on {surface_name}"),
                roles.fg_primary,
                surface,
                INCREASED_CONTRAST_TEXT_MIN,
            );
            measure(
                format!("fg.secondary on {surface_name}"),
                roles.fg_secondary,
                surface,
                TEXT_MIN_CONTRAST,
            );
            measure(
                format!("fg.tertiary on {surface_name}"),
                roles.fg_tertiary,
                surface,
                MARKER_MIN_CONTRAST,
            );
            measure(
                format!("fg.disabled on {surface_name}"),
                roles.fg_disabled,
                surface,
                DISABLED_TEXT_MIN_CONTRAST,
            );
            measure(
                format!("border.subtle on {surface_name}"),
                roles.border_subtle,
                surface,
                MIN_RULE_CONTRAST,
            );
            for (channel, ink, wash) in [
                ("success", roles.success, roles.success_wash),
                ("warning", roles.warning, roles.warning_wash),
                ("danger", roles.danger, roles.danger_wash),
                ("info", roles.info, roles.info_wash),
            ] {
                measure(
                    format!("status.{channel} on {surface_name}"),
                    ink,
                    surface,
                    TEXT_MIN_CONTRAST,
                );
                measure(
                    format!("status.{channel} on its own wash over {surface_name}"),
                    ink,
                    composite_surface(surface, wash),
                    TEXT_MIN_CONTRAST,
                );
            }
        }
        measure(
            "accent on surface.content".to_owned(),
            roles.accent,
            roles.surface_content,
            INTERACTIVE_MIN_CONTRAST,
        );
        measure(
            "accent.fg on accent".to_owned(),
            roles.accent_fg,
            roles.accent,
            INTERACTIVE_MIN_CONTRAST,
        );
        appearances.insert(
            if appearance == Appearance::Light {
                "light"
            } else {
                "dark"
            }
            .to_owned(),
            json!({
                "roles": entries,
                "increased": increased_entries(&increased),
                "measurements": measurements,
            }),
        );
    }
    json!({ "appearances": appearances })
}

/// The product roles for one appearance with `increaseContrast` on.
///
/// [`product_roles`] hands back what the shipped window paints; this is the same
/// derivation with the other set of floors, so the two can be compared without a
/// window and without trusting a review script to re-run the pass.
#[cfg(test)]
fn increased_roles(appearance: Appearance) -> Roles {
    let mut bridge = DEFAULT_BRIDGE.clone();
    bridge.appearance = appearance;
    bridge.refine(true);
    *bridge.roles()
}

/// The role values, as a name -> hex table.
#[cfg(test)]
fn increased_entries(roles: &Roles) -> serde_json::Value {
    use serde_json::json;

    let hex = |color: Hsla| {
        let rgba: gpui_kit::Rgba = color.into();
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            channel(rgba.r),
            channel(rgba.g),
            channel(rgba.b),
            channel(rgba.a)
        )
    };
    json!({
        "fg.primary": hex(roles.fg_primary),
        "fg.secondary": hex(roles.fg_secondary),
        "fg.tertiary": hex(roles.fg_tertiary),
        "fg.disabled": hex(roles.fg_disabled),
        "border.subtle": hex(roles.border_subtle),
        "border.base": hex(roles.border_base),
        "border.strong": hex(roles.border_strong),
        "accent": hex(roles.accent),
        "success": hex(roles.success),
        "warning": hex(roles.warning),
        "danger": hex(roles.danger),
        "info": hex(roles.info),
    })
}

/// Writes [`review_snapshot`] where `K8S_REVIEW_JSON` points, and checks it is
/// complete either way.
///
/// A snapshot that silently lost a role would be worse than no snapshot, so the
/// test asserts the shape rather than only writing it: two appearances, thirty
/// roles each, and a measurement for every graded pair.
#[test]
fn the_review_snapshot_is_exported_from_here() {
    let snapshot = review_snapshot();
    let appearances = snapshot["appearances"]
        .as_object()
        .expect("both appearances");
    assert_eq!(appearances.len(), 2, "one snapshot per appearance");
    for (name, appearance) in appearances {
        let roles = appearance["roles"].as_object().expect("role table");
        assert!(
            roles.len() >= 35,
            "{name} exported {} roles, which is fewer than this module defines",
            roles.len()
        );
        for required in [
            "surface.content",
            "fg.primary",
            "warning",
            "warning.wash",
            "wash:warning@content",
            "severity:Success",
        ] {
            assert!(
                roles.contains_key(required),
                "{name} did not export {required}"
            );
        }
        let measurements = appearance["measurements"].as_array().expect("measurements");
        // Six surfaces, and on each: four inks, one rule, and four channels
        // measured twice — bare, and on the channel's own wash. Seventy-eight,
        // plus the accent and the label on it.
        assert_eq!(
            measurements.len(),
            80,
            "{name} did not measure every surface and channel"
        );
        assert!(
            measurements
                .iter()
                .all(|entry| entry["ratio"].is_number() && entry["floor"].is_number()),
            "{name} exported a measurement without a ratio or a floor"
        );
    }
    if let Ok(path) = std::env::var("K8S_REVIEW_JSON")
        && let Err(error) = std::fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap())
    {
        eprintln!("could not write the review snapshot to {path}: {error}");
    }
}

#[cfg(test)]
mod contrast_tests {
    use super::{
        INCREASED_CONTRAST_GRAPHIC_MIN, INCREASED_CONTRAST_TEXT_MIN, MARKER_MIN_CONTRAST,
        ROW_STATE_MIN_CONTRAST, ROW_STRIPE_ALPHA, Severity, TEXT_MIN_CONTRAST,
        calculate_contrast_ratio, chart, colors, composite_surface, confidence, editor_wash, focus,
        increased_contrast_surface, role, row_stripe_bg, row_wash, search_match, status_colors,
        text_on, text_selection,
    };

    #[gpui_kit::test]
    fn semantic_surfaces_resolve_to_theme_colors(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            let colors = colors(cx);
            // One field per role, the way `Roles::parse` resolves a theme that
            // names none of them. The old names let a role be asserted against
            // three different fields at once, which held only while the shipped
            // themes happened to give those fields the same value.
            assert_eq!(role::surface_app(cx), colors.background);
            assert_eq!(role::surface_chrome(cx), colors.panel_background);
            assert_eq!(role::surface_content(cx), colors.editor_background);
            assert_eq!(role::surface_raised(cx), colors.elevated_surface_background);
            assert_eq!(role::accent_wash(cx), colors.element_selected);
            assert_eq!(focus::border(cx), colors.border_focused);
            assert_eq!(
                row_stripe_bg(cx),
                row_wash(
                    colors.surface_background,
                    colors.text.opacity(ROW_STRIPE_ALPHA)
                )
            );
            let active_line_overlay = editor_wash::active_line_overlay(cx);
            let diagnostic_overlay = editor_wash::diagnostic_overlay(cx);
            assert_eq!(active_line_overlay, colors.editor_active_line_background);
            assert_eq!(
                diagnostic_overlay,
                status_colors(cx)
                    .error_background
                    .opacity(editor_wash::DIAGNOSTIC_ALPHA)
            );
            assert_eq!(
                editor_wash::active_line(cx),
                composite_surface(colors.editor_background, active_line_overlay)
            );
            assert_eq!(
                editor_wash::diagnostic(cx),
                composite_surface(colors.editor_background, diagnostic_overlay)
            );
            assert_ne!(active_line_overlay, colors.element_selection_background);
            assert_ne!(active_line_overlay, diagnostic_overlay);
            assert_eq!(
                text_selection::background(cx),
                colors.element_selection_background
            );
            let selection = composite_surface(
                colors.editor_background,
                colors.element_selection_background,
            );
            let search = composite_surface(colors.editor_background, search_match::background(cx));
            let active_search = composite_surface(
                colors.editor_background,
                search_match::active_background(cx),
            );
            assert_ne!(search, selection);
            assert_ne!(active_search, selection);
            assert_ne!(active_search, search);
            assert_eq!(
                text_selection::foreground(cx),
                text_on(selection, colors.text, colors.text_accent)
            );
            let input_selection = composite_surface(
                colors.surface_background,
                colors.element_selection_background,
            );
            assert_eq!(
                text_selection::foreground_on(cx, colors.surface_background),
                text_on(input_selection, colors.text, colors.text_accent)
            );
            assert_eq!(
                search_match::foreground(cx),
                text_on(search, colors.text, colors.text_accent)
            );
            assert_eq!(
                search_match::active_foreground(cx),
                text_on(active_search, colors.text, colors.text_accent)
            );
            if colors.search_match_background == colors.element_selection_background {
                assert_eq!(
                    increased_contrast_surface(cx, colors.search_match_background),
                    search_match::background(cx)
                );
            }
            if colors.search_active_match_background == colors.element_selection_background {
                assert_eq!(
                    increased_contrast_surface(cx, colors.search_active_match_background),
                    search_match::active_background(cx)
                );
            }
        });
    }

    /// A row state has to be visible against the table it sits on, and the states
    /// have to be tellable apart from each other.
    ///
    /// The old assertions only checked text *on* a row, which is why a 1.018:1
    /// hover, a 1.02:1 multi-select member and a 1.101:1 zebra all passed: the
    /// text on them was perfectly readable, the rows just were not there. A state
    /// nobody can see is not a state.
    ///
    /// Increase Contrast has to make the selection *stronger*, and the text on it
    /// has to keep clearing the raised threshold. The wash used to drop from 14%
    /// to 5% when the setting was on, so turning it on quietly made a selected row
    /// weaker than a range member, which scaled it down again on top of that.
    #[gpui_kit::test]
    fn every_row_state_is_visible_and_the_four_are_tellable_apart(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        for appearance in [super::Appearance::Dark, super::Appearance::Light] {
            for increased in [false, true] {
                cx.update(|cx| {
                    crate::settings::set_test_increase_contrast(cx, increased);
                    super::set_appearance(cx, appearance);
                    super::refine_active_theme(cx);
                    let table = role::surface_raised(cx);
                    let states = [
                        ("stripe", row_stripe_bg(cx)),
                        ("hover", super::row_hover_bg(cx)),
                        ("cursor", super::row_focus_bg(cx)),
                        ("selected", super::row_selected_bg(cx)),
                    ];
                    for (name, wash) in states {
                        assert!(
                            calculate_contrast_ratio(wash, table) >= ROW_STATE_MIN_CONTRAST,
                            "{appearance:?} increased={increased}: the {name} row is \
                             {:.3}:1 on the table, under the {ROW_STATE_MIN_CONTRAST}:1 a row \
                             state has to clear — it is a rendering difference nobody can see",
                            calculate_contrast_ratio(wash, table)
                        );
                    }
                    for pair in states.windows(2) {
                        let ratio = calculate_contrast_ratio(pair[1].1, table)
                            / calculate_contrast_ratio(pair[0].1, table).max(f32::MIN_POSITIVE);
                        assert!(
                            ratio > super::ROW_STATE_MIN_SEPARATION,
                            "{appearance:?} increased={increased}: the {} row is only {ratio:.3}x \
                             the {} row, under the {}:1 two states have to be tellable apart by",
                            pair[1].0,
                            pair[0].0,
                            super::ROW_STATE_MIN_SEPARATION
                        );
                    }
                    // And the text on the strongest of them is still readable, at
                    // the threshold this mode is actually holding it to.
                    let floor = if increased {
                        INCREASED_CONTRAST_TEXT_MIN
                    } else {
                        TEXT_MIN_CONTRAST
                    };
                    for ink in [colors(cx).text, colors(cx).text_muted] {
                        assert!(
                            calculate_contrast_ratio(ink, super::row_selected_bg(cx)) >= floor,
                            "{appearance:?} increased={increased}: body text on the selected row \
                             is under the {floor}:1 this mode holds text to"
                        );
                    }
                });
            }
        }
    }

    /// Increase Contrast has to reach the roles, not only the skin.
    ///
    /// This is the assertion the setting did not have. `ThemeBridge::refine`
    /// wrote `ThemeColors` and `StatusColors` and stopped, while 479 call sites
    /// in the crate read `role::*` and 72 read `colors()` — so the setting moved
    /// the colour of things no component draws and left every surface, divider,
    /// focus rail and status channel the app actually paints exactly where it was.
    /// A comment in `theme::set_mode` claimed the opposite.
    ///
    /// Both appearances, both modes, and every role: the raised floors are
    /// `INCREASED_CONTRAST_TEXT_MIN` for ink and `INCREASED_CONTRAST_GRAPHIC_MIN`
    /// for marks and rules, which is the promise `theme.json`'s own
    /// accessibility contract states.
    ///
    /// The status channels read their **word** ink. The mark beside it is a
    /// graphic and is pinned to `channel_mark`, which is the same 4.5:1 in both
    /// modes — so the four marks already clear it with the setting off and
    /// nothing is left for the setting to move, which is the intent (see
    /// [`Floors::channel_mark`]). Grading the mark at 7:1 would ask the setting
    /// to spend a hue on a role that is already legible as a dot.
    #[gpui_kit::test]
    fn increase_contrast_lifts_every_role_the_app_reads(cx: &mut gpui_kit::TestAppContext) {
        for appearance in [super::Appearance::Dark, super::Appearance::Light] {
            cx.update(|cx| {
                crate::settings::set_test_increase_contrast(cx, false);
                super::set_appearance(cx, appearance);
                super::refine_active_theme(cx);
                let before = *super::roles(cx);
                crate::settings::set_test_increase_contrast(cx, true);
                super::refine_active_theme(cx);
                let after = *super::roles(cx);

                for (name, quiet, loud, floor) in [
                    (
                        "fg.primary",
                        before.fg_primary,
                        after.fg_primary,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "fg.secondary",
                        before.fg_secondary,
                        after.fg_secondary,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "fg.tertiary",
                        before.fg_tertiary,
                        after.fg_tertiary,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "fg.disabled",
                        before.fg_disabled,
                        after.fg_disabled,
                        INCREASED_CONTRAST_GRAPHIC_MIN,
                    ),
                    (
                        "accent",
                        before.accent,
                        after.accent,
                        INCREASED_CONTRAST_GRAPHIC_MIN,
                    ),
                    (
                        "status.success",
                        before.success_word,
                        after.success_word,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "status.warning",
                        before.warning_word,
                        after.warning_word,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "status.danger",
                        before.danger_word,
                        after.danger_word,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                    (
                        "status.info",
                        before.info_word,
                        after.info_word,
                        INCREASED_CONTRAST_TEXT_MIN,
                    ),
                ] {
                    // Only a role that was *short* of the raised floor has to move.
                    // The dark `fg.primary` already reads 15.5:1 and the light one
                    // 18.9:1, and a pass that moved those to "prove" it ran would be
                    // moving a role the floor cannot move.
                    if super::contrast_ratio(quiet, after.surface_content) < floor {
                        assert_ne!(
                            quiet, loud,
                            "{appearance:?} {name} was under its {floor}:1 raised floor with \
                             the setting off and is the same colour with it on, so the setting \
                             does not reach the role the app reads"
                        );
                    }
                }
                // Every ink at the raised floor, on every surface it lands on.
                //
                // `fg.disabled` is not one of them: WCAG 1.4.3 exempts an inactive
                // control from the text floor, and the role is defined as the
                // quietest ink in the scale, so under Increase Contrast it is held
                // to the *graphic* floor and still has to sit under the rung above
                // it. The skin's own pass agrees — it pushes `text`,
                // `text_muted`, `text_placeholder` and `text_accent` to 7:1 and
                // leaves `text_disabled` on its own floor.
                for surface in after.all_surfaces() {
                    for (name, ink, floor) in [
                        ("fg.primary", after.fg_primary, INCREASED_CONTRAST_TEXT_MIN),
                        (
                            "fg.secondary",
                            after.fg_secondary,
                            INCREASED_CONTRAST_TEXT_MIN,
                        ),
                        (
                            "fg.tertiary",
                            after.fg_tertiary,
                            INCREASED_CONTRAST_TEXT_MIN,
                        ),
                        (
                            "fg.disabled",
                            after.fg_disabled,
                            INCREASED_CONTRAST_GRAPHIC_MIN,
                        ),
                    ] {
                        let measured = super::contrast_ratio(ink, surface);
                        assert!(
                            measured >= floor,
                            "{appearance:?} {name} is {measured:.2}:1 on a role surface with \
                             Increase Contrast on, under the {floor}:1 the setting promises"
                        );
                    }
                }
                // Marks and rules at the raised graphic floor.
                for (name, mark, surface) in [
                    ("accent", after.accent, after.surface_content),
                    ("status.success", after.success, after.surface_content),
                    ("status.warning", after.warning, after.surface_content),
                    ("status.danger", after.danger, after.surface_content),
                    ("status.info", after.info, after.surface_content),
                    ("border.subtle", after.border_subtle, after.surface_content),
                    ("border.base", after.border_base, after.surface_content),
                    ("border.strong", after.border_strong, after.surface_content),
                    (
                        "border.subtle on chrome",
                        after.border_subtle,
                        after.surface_chrome,
                    ),
                ] {
                    let measured = super::contrast_ratio(mark, surface);
                    assert!(
                        measured >= INCREASED_CONTRAST_GRAPHIC_MIN,
                        "{appearance:?} {name} is {measured:.2}:1 with Increase Contrast on, \
                         under the {INCREASED_CONTRAST_GRAPHIC_MIN}:1 the setting promises of a \
                         mark"
                    );
                }
                // The ladder still has rungs after the pass, or "quieter" stopped
                // meaning anything: disabled is the exempt one, and it still has
                // to sit under the role above it.
                let surface = after.surface_content;
                assert!(
                    super::contrast_ratio(after.fg_disabled, surface)
                        < super::contrast_ratio(after.fg_tertiary, surface),
                    "{appearance:?} fg.disabled and fg.tertiary are one role with Increase \
                     Contrast on"
                );
                for pair in [
                    (after.border_subtle, after.border_base),
                    (after.border_base, after.border_strong),
                ] {
                    let ratio = super::contrast_ratio(pair.1, surface)
                        / super::contrast_ratio(pair.0, surface).max(f32::MIN_POSITIVE);
                    assert!(
                        ratio > 1.05,
                        "{appearance:?} two rules are within 5% of each other with Increase \
                         Contrast on ({ratio:.3}x), so they are one stroke"
                    );
                }
            });
        }
    }

    /// The chart crosshair is a graphic, so it follows the graphic threshold in
    /// both modes instead of being dimmed below it.
    #[gpui_kit::test]
    fn chart_crosshair_meets_the_graphic_threshold(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            crate::settings::set_test_increase_contrast(cx, false);
            let canvas = role::surface_app(cx);
            let crosshair = chart::crosshair(cx);
            assert_eq!(crosshair.a, 1.0, "an alpha cannot be checked");
            assert!(
                calculate_contrast_ratio(crosshair, canvas) >= MARKER_MIN_CONTRAST,
                "crosshair is below the graphic threshold on the canvas"
            );
        });
        cx.update(|cx| {
            crate::settings::set_test_increase_contrast(cx, true);
            let canvas = role::surface_app(cx);
            let crosshair = chart::crosshair(cx);
            assert_eq!(crosshair.a, 1.0, "an alpha cannot be checked");
            assert!(
                calculate_contrast_ratio(crosshair, canvas) >= INCREASED_CONTRAST_GRAPHIC_MIN,
                "crosshair is below the increased-contrast graphic threshold"
            );
        });
    }

    /// The observation-confidence channel carries no other role's ink.
    ///
    /// It used to read `text_muted` for `Known`, `text_placeholder` for `Stale`
    /// and `icon_disabled` for `Unknown`. `icon_disabled` is only refined under
    /// Increase Contrast, so the default path never checked it, and the light
    /// `Unknown` measured 4.19:1 on the panel and 4.01:1 on the canvas: the only
    /// body-text failure in the theme.
    ///
    /// The channel is a live one: its two seeds come from the theme file's
    /// `confidence.unknown` and `confidence.stale` rather than from two
    /// `const [u32; 2]` tables, so a skin owns it. The seeds are written for one
    /// appearance and the shipped path solves them against the same core surfaces
    /// the status foregrounds use, so this checks the product of the two — the
    /// light `Unknown` seed misses the floor on the light panel and the light
    /// canvas, and solving is what closes that.
    ///
    /// A confidence mark has to clear the body-text floor on every surface it can
    /// land on, in both appearances, and it must not be the health hue beside it.
    #[gpui_kit::test]
    fn the_live_confidence_foreground_meets_the_text_threshold(cx: &mut gpui_kit::TestAppContext) {
        use super::Confidence;
        cx.update(|cx| {
            crate::settings::set_test_increase_contrast(cx, false);
            for state in [Confidence::Known, Confidence::Stale, Confidence::Unknown] {
                let ink = confidence::foreground(state, cx);
                for background in [
                    role::surface_app(cx),
                    role::surface_chrome(cx),
                    role::surface_raised(cx),
                    role::surface_raised(cx),
                ] {
                    assert!(
                        calculate_contrast_ratio(ink, background) >= TEXT_MIN_CONTRAST,
                        "{state:?} confidence is below the body-text floor on {background:?}"
                    );
                }
            }
        });
    }

    /// The health channel's own ink, so a stale answer and a healthy object never
    /// resolve to the same colour.
    #[gpui_kit::test]
    fn confidence_and_health_resolve_to_different_colors(cx: &mut gpui_kit::TestAppContext) {
        use super::Confidence;
        cx.update(|cx| {
            crate::settings::set_test_increase_contrast(cx, false);
            for (state, severity) in [
                (Confidence::Unknown, Severity::Error),
                (Confidence::Stale, Severity::Warning),
            ] {
                let ink = confidence::foreground(state, cx);
                let health = severity.marker_on(cx, role::surface_app(cx));
                assert_ne!(
                    ink, health,
                    "{state:?} and {severity:?} resolve to the same color, so a resource the \
                     app could not read reads as one that is unwell"
                );
            }
        });
    }
}
