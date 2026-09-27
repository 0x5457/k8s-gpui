//! Design tokens and semantic mappings for dimensions, typography, status, and resource icons.
//!
//! View code uses theme colors and shared tokens instead of raw colors or one-off spacing.

use std::sync::Arc;

use gpui::{App, Hsla, Pixels};
use ui::{ActiveTheme as _, IconName};

pub mod text {
    use gpui::{Pixels, px};

    pub const SECTION: Pixels = px(15.);
    pub const SECTION_LINE_HEIGHT: Pixels = px(20.);
    pub const PANEL_TITLE: Pixels = px(14.);
    pub const PANEL_TITLE_LINE_HEIGHT: Pixels = px(18.);
    pub const BODY: Pixels = px(13.);
    pub const BODY_LINE_HEIGHT: Pixels = px(16.);
    pub const METADATA: Pixels = px(11.);
    pub const METADATA_LINE_HEIGHT: Pixels = px(14.);
    pub const DATA: Pixels = px(12.);
    pub const DATA_LINE_HEIGHT: Pixels = px(18.);

    /// The one number a surface exists to communicate.
    ///
    /// Reserved for a ratio or count that a reader should get before anything
    /// else, such as a cluster's ready-to-total health. Using it for a section
    /// heading would spend the strongest signal in the type scale on
    /// structure. Per `typography.md > Conveying hierarchy`, not every word
    /// needs to grow; the content people care about is the one that should.
    pub const DISPLAY: Pixels = px(24.);
    pub const DISPLAY_LINE_HEIGHT: Pixels = px(28.);
}

pub mod surface {
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    pub fn canvas(cx: &App) -> Hsla {
        cx.theme().colors().background
    }

    pub fn panel(cx: &App) -> Hsla {
        cx.theme().colors().panel_background
    }

    pub fn editor(cx: &App) -> Hsla {
        cx.theme().colors().editor_background
    }

    pub fn input(cx: &App) -> Hsla {
        cx.theme().colors().surface_background
    }

    pub fn raised(cx: &App) -> Hsla {
        cx.theme().colors().elevated_surface_background
    }

    pub fn control(cx: &App) -> Hsla {
        cx.theme().colors().element_background
    }

    pub fn notification(cx: &App) -> Hsla {
        cx.theme().colors().elevated_surface_background
    }

    pub fn selected(cx: &App) -> Hsla {
        cx.theme().colors().element_selected
    }

    /// The scrim a modal or popover lays over the content behind it.
    ///
    /// `modality.md > Best practices` warns that a modal which obscures its
    /// previous context makes people lose track of the task they suspended.
    /// A scrim states the modality without needing a heavier border, and it
    /// keeps the yellow status washes behind a dialog from out-shouting the
    /// dialog itself.
    pub fn backdrop(cx: &App) -> Hsla {
        use super::BACKDROP_ALPHA;
        // Always toward black. A scrim that lightens in dark mode lifts the
        // content toward the dialog's own value, so the dialog stops being the
        // brightest thing on screen and the app reads as a grey card on a grey
        // field. Darkening works in both appearances and keeps the modal on top.
        canvas(cx).blend(gpui::black().opacity(BACKDROP_ALPHA))
    }

    pub fn tab_bar(cx: &App) -> Hsla {
        cx.theme().colors().tab_bar_background
    }

    pub fn tab_active(cx: &App) -> Hsla {
        cx.theme().colors().tab_active_background
    }

    /// The terminal canvas.
    ///
    /// A terminal is content, not chrome, so it belongs one step *above* the
    /// panel that contains it: the Dock is the container and the grid is a card
    /// inside it, in both appearances. The role used to be read straight off
    /// `colors().terminal_background` from the terminal panel, which left it
    /// outside the ramp and free to sit on the wrong side of the Dock - in
    /// K8s Studio Dark it was a hole below its container while in K8s Studio
    /// Light it was a card above it, so one rule could not describe both.
    pub fn terminal(cx: &App) -> Hsla {
        cx.theme().colors().terminal_background
    }
}

/// Focus roles. Keyboard focus is a position cue, not a surface wash, so it is
/// named separately from the selection roles and never borrows their colors.
pub mod focus {
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    /// Border that replaces a transparent edge while the element holds focus.
    pub fn border(cx: &App) -> Hsla {
        cx.theme().colors().border_focused
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
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    use super::{composite_surface, increased_contrast_colors};

    /// Alpha of the inline diagnostic tint, matching the editor renderer.
    pub const DIAGNOSTIC_ALPHA: f32 = 0.16;

    /// Wash under the cursor line.
    pub fn active_line_overlay(cx: &App) -> Hsla {
        active_line_overlay_for(cx.theme().colors())
    }

    /// Wash under a line that carries an inline diagnostic.
    ///
    /// Diagnostic is a separate role from the cursor line: a diagnostic that
    /// shares the cursor wash cannot be told apart from "this is where I am".
    pub fn diagnostic_overlay(cx: &App) -> Hsla {
        diagnostic_overlay_for(cx.theme().colors(), cx.theme().status())
    }

    /// Surface the cursor-line text sits on.
    pub fn active_line(cx: &App) -> Hsla {
        composite_surface(
            cx.theme().colors().editor_background,
            active_line_overlay(cx),
        )
    }

    /// Surface the diagnostic text sits on.
    pub fn diagnostic(cx: &App) -> Hsla {
        composite_surface(
            cx.theme().colors().editor_background,
            diagnostic_overlay(cx),
        )
    }

    pub(crate) fn active_line_overlay_for(colors: &theme::ThemeColors) -> Hsla {
        increased_contrast_colors(colors, colors.editor_active_line_background)
    }

    pub(crate) fn diagnostic_overlay_for(
        colors: &theme::ThemeColors,
        status: &theme::StatusColors,
    ) -> Hsla {
        increased_contrast_colors(colors, status.error_background.opacity(DIAGNOSTIC_ALPHA))
    }
}

pub mod text_selection {
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    use super::composite_surface;

    pub fn background(cx: &App) -> Hsla {
        cx.theme().colors().element_selection_background
    }

    pub fn foreground_on(cx: &App, base_surface: Hsla) -> Hsla {
        let colors = cx.theme().colors();
        let selection = composite_surface(base_surface, background(cx));
        super::text_on_for_mode(
            selection,
            colors.text,
            colors.text_accent,
            crate::settings::increase_contrast_enabled(cx),
        )
    }

    pub fn foreground(cx: &App) -> Hsla {
        foreground_on(cx, cx.theme().colors().editor_background)
    }
}

pub mod search_match {
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    use super::composite_surface;

    const MATCH_FALLBACK_ALPHA: f32 = 0.21;
    const ACTIVE_FALLBACK_ALPHA: f32 = 0.19;
    const ACTIVE_FALLBACK_ALPHA_ALT: f32 = 0.17;

    pub(crate) fn resolve_background(colors: &theme::ThemeColors) -> Hsla {
        if colors.search_match_background == colors.element_selection_background {
            colors.text_accent.opacity(MATCH_FALLBACK_ALPHA)
        } else {
            colors.search_match_background
        }
    }

    pub(crate) fn resolve_active_background(colors: &theme::ThemeColors) -> Hsla {
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

    pub(crate) fn refine_colors(colors: &mut theme::ThemeColors) {
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
        resolve_background(cx.theme().colors())
    }

    pub fn active_background(cx: &App) -> Hsla {
        resolve_active_background(cx.theme().colors())
    }

    pub fn foreground_on(cx: &App, base_surface: Hsla, active: bool) -> Hsla {
        let colors = cx.theme().colors();
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
        foreground_on(cx, cx.theme().colors().editor_background, false)
    }

    pub fn active_foreground(cx: &App) -> Hsla {
        foreground_on(cx, cx.theme().colors().editor_background, true)
    }
}

pub mod search {
    pub use super::search_match::{
        active_background, active_foreground, background, foreground, foreground_on,
    };
}

/// Motion budgets.
///
/// The app has no transitions: every state change is instant, which is what a
/// keyboard-driven tool wants and what `motion.md > Best practices` asks for at
/// high frequency. The two durations below are the only ones that exist, and
/// both are repeats rather than transitions — a loading marker turning and a
/// caret blinking. A third value would be a budget for an animation nobody
/// spends, so a new transition has to add its token here rather than inline a
/// number.
pub mod motion {
    use std::time::Duration;

    /// One turn of the loading marker.
    ///
    /// Slow enough to read as "working" rather than "flickering", and only
    /// started while something is genuinely being waited on.
    pub const LOADING: Duration = Duration::from_millis(1_200);

    /// One half-cycle of a blinking text caret.
    pub const CARET: Duration = Duration::from_millis(900);

    /// Rotation period of the loading marker, in whole seconds.
    ///
    /// `with_rotate_animation` counts in seconds, so the millisecond tokens
    /// cannot be passed to it directly. The marker only spins while it reports
    /// real waiting; `AnimationElement` renders a single static frame when
    /// `App::reduce_motion` is set.
    pub const SPINNER_PERIOD_SECONDS: u64 = 1;
}

pub mod border {
    use gpui::{Pixels, px};

    pub const LINE: Pixels = px(1.);
    pub const HIT: Pixels = px(20.);
    pub const FOCUS_RAIL: Pixels = px(2.);
    pub const TABLE_FOCUS_RAIL: Pixels = px(3.);

    /// The rule that separates a column from its neighbour.
    ///
    /// Structural dividers stay low contrast on purpose. A rule strong enough
    /// to read as content turns a dense table into a spreadsheet, so the rest
    /// position is kept faint and the interactive state is what strengthens
    /// it. Per `lists-and-tables.md > Desktop (macOS)`, alternating row
    /// colors already carry values across columns, which is what removes the
    /// need for a permanent rule between every pair.
    pub const COLUMN_RULE: Pixels = px(1.);

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
}

pub mod space {
    use gpui::{Pixels, px};
    pub const XS: Pixels = px(4.);
    pub const SM: Pixels = px(8.);
    pub const MD: Pixels = px(12.);
    pub const LG: Pixels = px(16.);
    pub const XL: Pixels = px(24.);
    pub const XXL: Pixels = px(32.);
}

/// Fixed dimensions for desktop controls and panels.
pub mod size {
    use gpui::{Pixels, px};
    pub const HIT_MIN: Pixels = px(20.);
    pub const CONTROL: Pixels = px(28.);
    pub const ROW: Pixels = px(28.);
    pub const TREE_ROW: Pixels = ROW;
    pub const TOOLBAR: Pixels = px(40.);
    pub const UPDATE_STRIP: Pixels = px(40.);
    pub const UPDATE_PROGRESS: Pixels = px(160.);
    pub const TAB_BAR: Pixels = px(34.);
    pub const STATUS_BAR: Pixels = px(32.);
    pub const STATUS_DOT: Pixels = px(8.);
    pub const SIDEBAR_MIN: Pixels = px(160.);
    pub const SIDEBAR_MAX: Pixels = px(360.);
    /// Narrow rail for the Hotbar.
    pub const HOTBAR_RAIL: Pixels = px(40.);
    /// Hotbar slot hit area.
    pub const HOTBAR_SLOT: Pixels = px(28.);
    pub const INSPECTOR_MIN: Pixels = px(240.);
    pub const INSPECTOR_MAX: Pixels = px(480.);
    pub const CENTER_MIN: Pixels = px(480.);
    pub const MAIN_CONTENT_MIN: Pixels = px(280.);
    /// The tab bar, the toolbar, and the status banner do not scroll, and the log body needs
    /// three more rows on top of them, so the Dock cannot be shorter than this. 34 + 40 + 28 of
    /// fixed chrome plus three 18px log rows is 156px. `Pixels` arithmetic is not `const`, so the
    /// sum is spelled out and a test holds it to the tokens.
    pub const DOCK_MIN: Pixels = px(156.);
    pub const DOCK_MAX: Pixels = px(400.);
    /// Icon in a control, such as a button or a tab.
    pub const ICON: Pixels = px(16.);
    /// Icon that leads an empty or loading state.
    pub const ICON_LARGE: Pixels = px(32.);
    /// The status-marker family: a health glyph, a confidence mark, a severity
    /// dot, and the markers in a Describe or log row.
    ///
    /// These were rendered at 12px directly beside 13px body text, which made
    /// the one thing that carries a row's verdict read as a speck. The family
    /// shares `ICON` so a marker is the same size everywhere it appears, and
    /// `DESIGN.md` §3.3 records it as a fixed size rather than leaving each call
    /// site to pick.
    pub const STATUS_MARKER: Pixels = ICON;
    /// Narrowest window the shell supports, in logical pixels.
    ///
    /// `main.rs` sets `window_min_size` and the shell decides its compact
    /// breakpoints from the same number. They were two independent literals, so
    /// nothing stopped them from drifting and the layout promises in
    /// `DESIGN.md` §6 stopped being enforceable.
    pub const WINDOW_MIN: (f32, f32) = (960., 640.);
}

/// Text sizes for the desktop UI.
pub const TEXT: Pixels = text::BODY;
pub const TEXT_SMALL: Pixels = text::METADATA;
pub const TEXT_MONO: Pixels = text::DATA;

pub fn row_height(line_height: Pixels) -> Pixels {
    line_height.max(size::ROW)
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

/// How much quieter a secondary text role has to be than the role above it, as a
/// contrast difference on the surface that binds the two.
///
/// [`quieter_color_on_all`] walks lightness in 1/256 steps, so without a gap a
/// role can end up one step below its neighbour: two values, one colour. The
/// band between the floor and the ceiling is over a full point wide, so the gap
/// is a demand for a real step, not a rounding artefact.
const QUIET_MIN_GAP: f32 = 0.4;

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
    ui::utils::calculate_contrast_ratio(foreground, background)
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

/// A role that has to be quieter than the role it sits below.
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
/// exists to fix.
///
/// When no value clears the floor with the gap, the shared floor solver takes
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
    let ceiling_worst = worst_contrast(ceiling, backgrounds);
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
        if meets_contrast(candidate, backgrounds, target)
            && worst_contrast(candidate, backgrounds) <= ceiling_worst - QUIET_MIN_GAP
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
    use gpui::{App, Hsla};
    use ui::ActiveTheme as _;

    use super::surface;

    /// Crosshair that marks the hovered or keyboard-scrubbed sample.
    ///
    /// It is a graphic, so it follows the graphic threshold. A flat
    /// `text_muted` wash multiplied down for subtlety falls below that
    /// threshold once Increase Contrast has pushed the text tokens to `7:1`, so
    /// the role is solved instead of dimmed. The crosshair stays a hairline
    /// lighter in weight than the data line, not lower in contrast.
    pub fn crosshair(cx: &App) -> Hsla {
        super::graphic_on_for_mode(
            surface::canvas(cx),
            cx.theme().colors().text_muted,
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
            surface::canvas(cx),
            cx.theme()
                .accents()
                .color_for_index((index % super::SERIES_SLOTS) as u32),
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

fn core_surfaces(colors: &theme::ThemeColors) -> Vec<Hsla> {
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
fn row_base(colors: &theme::ThemeColors) -> Hsla {
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
fn row_accent_alphas(colors: &theme::ThemeColors, selected_alpha: f32, minimum: f32) -> (f32, f32) {
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
fn row_washes(colors: &theme::ThemeColors, selected_alpha: f32, minimum: f32) -> [Hsla; 4] {
    let base = row_base(colors);
    let (focus_alpha, selected_alpha) = row_accent_alphas(colors, selected_alpha, minimum);
    [
        row_wash(base, colors.text.opacity(ROW_STRIPE_ALPHA)),
        colors.element_hover,
        composite_surface(base, colors.text_accent.opacity(focus_alpha)),
        composite_surface(base, colors.text_accent.opacity(selected_alpha)),
    ]
}

fn text_surfaces(colors: &theme::ThemeColors, selected_alpha: f32, minimum: f32) -> Vec<Hsla> {
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
fn accent_surfaces(colors: &theme::ThemeColors, selected_alpha: f32, minimum: f32) -> Vec<Hsla> {
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
    colors: &theme::ThemeColors,
    status: &theme::StatusColors,
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
    colors: &theme::ThemeColors,
    status: &theme::StatusColors,
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
fn refine_semantic_washes(colors: &mut theme::ThemeColors, text_minimum: f32) {
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
    colors: &mut theme::ThemeColors,
    status: &theme::StatusColors,
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

fn refine_status_backgrounds(status: &mut theme::StatusColors, text: Hsla, text_minimum: f32) {
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
    status: &mut theme::StatusColors,
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

pub fn refine_theme(theme: &mut theme::Theme) {
    refine_theme_with_contrast(theme, false);
}

pub fn refine_theme_with_contrast(theme: &mut theme::Theme, increased: bool) {
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
    let background = theme.styles.colors.background.alpha(1.0);
    let editor_background = composite_surface(background, theme.styles.colors.editor_background);
    let panel_background = composite_surface(background, theme.styles.colors.panel_background);
    let surface_background = composite_surface(background, theme.styles.colors.surface_background);
    let raised_background =
        composite_surface(background, theme.styles.colors.elevated_surface_background);
    let terminal_background =
        composite_surface(background, theme.styles.colors.terminal_background);

    theme.styles.colors.background = background;
    theme.styles.colors.editor_background = editor_background;
    theme.styles.colors.panel_background = panel_background;
    theme.styles.colors.surface_background = surface_background;
    // The editor is a third neighbour: a modal lands on the YAML editor more
    // often than on a panel, and the editor is the one surface `surface` does
    // not cover. Checking only panel and surface let the light theme put both
    // `raised` and the editor on the same white and dissolve a dialog into the
    // document underneath it.
    theme.styles.colors.elevated_surface_background = separate_surface(
        raised_background,
        &[panel_background, surface_background, editor_background],
    );
    theme.styles.colors.terminal_background = terminal_background;
    theme.styles.colors.title_bar_background =
        composite_surface(background, theme.styles.colors.title_bar_background);
    theme.styles.colors.title_bar_inactive_background = composite_surface(
        background,
        theme.styles.colors.title_bar_inactive_background,
    );
    theme.styles.colors.toolbar_background =
        composite_surface(background, theme.styles.colors.toolbar_background);
    theme.styles.colors.tab_bar_background =
        composite_surface(background, theme.styles.colors.tab_bar_background);
    theme.styles.colors.tab_inactive_background =
        composite_surface(background, theme.styles.colors.tab_inactive_background);
    theme.styles.colors.tab_active_background =
        composite_surface(background, theme.styles.colors.tab_active_background);
    theme.styles.colors.element_background =
        composite_surface(surface_background, theme.styles.colors.element_background);
    theme.styles.colors.element_hover =
        composite_surface(surface_background, theme.styles.colors.element_hover);
    theme.styles.colors.element_active =
        composite_surface(surface_background, theme.styles.colors.element_active);
    theme.styles.colors.element_selected =
        composite_surface(surface_background, theme.styles.colors.element_selected);
    theme.styles.colors.element_disabled =
        composite_surface(surface_background, theme.styles.colors.element_disabled);
    theme.styles.colors.editor_gutter_background = composite_surface(
        editor_background,
        theme.styles.colors.editor_gutter_background,
    );
    theme.styles.colors.editor_subheader_background =
        composite_surface(background, theme.styles.colors.editor_subheader_background);
    theme.styles.colors.editor_highlighted_line_background = composite_surface(
        editor_background,
        theme.styles.colors.editor_highlighted_line_background,
    );
    theme.styles.colors.panel_overlay_background = composite_surface(
        panel_background,
        theme.styles.colors.panel_overlay_background,
    );
    theme.styles.colors.panel_overlay_hover =
        composite_surface(panel_background, theme.styles.colors.panel_overlay_hover);
    search_match::refine_colors(&mut theme.styles.colors);
    if increased {
        refine_semantic_washes(&mut theme.styles.colors, text_minimum);
    }

    let core = core_surfaces(&theme.styles.colors);
    let status = &mut theme.styles.status;
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
        &mut theme.styles.colors,
        &theme.styles.status,
        text_minimum,
        disabled_minimum,
        selected_alpha,
        increased,
    );
    let text = theme.styles.colors.text;
    refine_status_backgrounds(&mut theme.styles.status, text, text_minimum);
    if increased {
        let surfaces =
            increased_text_surfaces(&theme.styles.colors, &theme.styles.status, selected_alpha);
        refine_status_foregrounds(
            &mut theme.styles.status,
            &surfaces,
            text_minimum,
            graphic_minimum,
        );
    }

    let colors = &mut theme.styles.colors;
    colors.editor_foreground = colors.text;
    colors.editor_line_number = colors.text_muted;
    colors.editor_active_line_number = colors.text;
    colors.editor_hover_line_number = colors.text_muted;
    colors.editor_code_lens_foreground = Some(colors.text_muted);

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
        increased_text_surfaces(colors, &theme.styles.status, selected_alpha)
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
    colors.icon = refined_graphic_on(
        colors.icon,
        colors.text,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );
    colors.icon_muted = refined_graphic_on(
        colors.icon_muted,
        colors.text_muted,
        &graphic_surfaces,
        graphic_minimum,
        increased,
    );

    let panel = colors.panel_background;
    let background = colors.background;
    let accents = theme.styles.accents.0.iter().copied().collect::<Vec<_>>();
    theme.styles.accents.0 = Arc::from(
        accents
            .into_iter()
            .map(|accent| graphic_on_all(accent, &[panel, background], graphic_minimum))
            .collect::<Vec<_>>(),
    );
}

impl Severity {
    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Severity::Success => theme.status().success,
            Severity::Warning => theme.status().warning,
            Severity::Error => theme.status().error,
            Severity::Info => theme.status().info,
            Severity::Neutral => theme.colors().text,
            Severity::Muted => theme.colors().text_muted,
        }
    }

    pub fn marker(self, cx: &App) -> Hsla {
        self.marker_on(cx, surface::canvas(cx))
    }

    pub fn marker_on(self, cx: &App, background: Hsla) -> Hsla {
        marker_for_background(self.color(cx), background)
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
        let palette = cx.theme().status();
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

/// Map a Kubernetes kind to an icon.
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
        IconName::FilterFunnel
    } else {
        IconName::Filter
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
/// The two hues below are the ones `DESIGN-PROPOSAL` §4.1 specified and the
/// themes now carry as `confidence.unknown` and `confidence.stale`. They are
/// desaturated on purpose: a confidence mark must never compete with a status
/// hue for attention, and it is solved against the same surfaces as the status
/// foregrounds so both channels are held to the same threshold.
pub mod confidence {
    use gpui::{App, Hsla, rgba};
    use theme::Appearance;
    use ui::{ActiveTheme as _, IconName};

    use super::{
        Confidence, INCREASED_CONTRAST_TEXT_MIN, TEXT_MIN_CONTRAST, adjusted_color_on_all,
        core_surfaces,
    };

    /// Seed hue, dark appearance first: "I could not reach a verdict".
    const UNKNOWN: [u32; 2] = [0x8a97a8ff, 0x6b7887ff];
    /// Seed hue, dark appearance first: "the answer is older than the refresh
    /// interval".
    const STALE: [u32; 2] = [0xb9944fff, 0x8a6400ff];

    fn seed(hues: [u32; 2], cx: &App) -> Hsla {
        let dark = cx.theme().appearance() == Appearance::Dark;
        rgba(hues[usize::from(!dark)]).into()
    }

    /// The color of a confidence marker or caption.
    ///
    /// Solved rather than read straight out of the theme, because the seed hues
    /// are written for one appearance: the light `Unknown` misses the body-text
    /// floor on the light panel and the light canvas. Solving keeps the channel
    /// above `TEXT_MIN_CONTRAST` — or `7:1` under Increase Contrast — on every
    /// surface a marker can land on, which is the same promise the status
    /// foregrounds get.
    pub fn foreground(state: Confidence, cx: &App) -> Hsla {
        let minimum = if crate::settings::increase_contrast_enabled(cx) {
            INCREASED_CONTRAST_TEXT_MIN
        } else {
            TEXT_MIN_CONTRAST
        };
        let preferred = match state {
            // A known answer draws no marker at all, so the only ink it spends is
            // the caption beside it, and that caption is secondary text. It is
            // not a second health channel: `confidence::icon` and `health_icon`
            // are disjoint shape families, which is what keeps the two apart on a
            // row that carries both.
            Confidence::Known => cx.theme().colors().text_muted,
            Confidence::Stale => seed(STALE, cx),
            Confidence::Unknown => seed(UNKNOWN, cx),
        };
        adjusted_color_on_all(preferred, &core_surfaces(cx.theme().colors()), minimum)
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
            Confidence::Stale => IconName::HistoryRerun,
            Confidence::Unknown => IconName::CircleHelp,
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
        Severity::Warning => IconName::Warning,
        Severity::Error => IconName::XCircleFilled,
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

/// [`row_wash`] against the surface the table is actually painted on.
///
/// The colours-level callers go through `row_wash` too, so the refinement pass
/// solves text against the wash the renderer produces rather than the requested
/// one. The solver only ever walks a wash *away* from its base, so a caller that
/// skipped it would be reading a surface that is never painted.
fn row_state_wash(cx: &App, wash: Hsla) -> Hsla {
    row_wash(surface::input(cx), wash)
}

/// A row-state wash solved against a base other than the table's.
///
/// The modal pickers are the one list in the app that paints on
/// `surface::raised`, and `row_selected_bg` is solved against the table's base,
/// so using it there put the selection in the same bind the sidebar rail was in:
/// legible on paper, far too quiet on the surface it actually lands on. The floor
/// is the same one every other row state clears, so the difference is the base and
/// nothing else.
pub fn row_selected_bg_on(cx: &App, base: Hsla) -> Hsla {
    let (_, alpha) = row_accent_alphas_for(cx);
    row_wash(base, cx.theme().colors().text_accent.opacity(alpha))
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
    row_accent_alphas(cx.theme().colors(), requested, row_text_minimum(cx))
}

/// Selected row background blended with the base row.
/// The color does not depend on the row index.
pub fn row_selected_bg(cx: &App) -> Hsla {
    let (_, alpha) = row_accent_alphas_for(cx);
    row_state_wash(cx, cx.theme().colors().text_accent.opacity(alpha))
}

/// The row the keyboard cursor is on, before anything is selected.
///
/// It used to be the same token as the hover, so a row the keyboard was on and a
/// row the pointer was over were the same color and the two states could not be
/// told apart. A hover is transient and a cursor is a position, so the cursor
/// carries the accent and the hover does not.
pub fn row_focus_bg(cx: &App) -> Hsla {
    let (alpha, _) = row_accent_alphas_for(cx);
    row_state_wash(cx, cx.theme().colors().text_accent.opacity(alpha))
}

pub fn row_hover_bg(cx: &App) -> Hsla {
    row_state_wash(cx, cx.theme().colors().element_hover)
}

/// Alternating row background, blended with the base row.
///
/// The zebra is a row state like any other, so it clears the same floor as the
/// hover, the cursor and the selection instead of being a 5% tint nobody can see.
pub fn row_stripe_bg(cx: &App) -> Hsla {
    row_state_wash(cx, cx.theme().colors().text.opacity(ROW_STRIPE_ALPHA))
}

pub fn increased_contrast_surface(cx: &App, surface: Hsla) -> Hsla {
    increased_contrast_colors(cx.theme().colors(), surface)
}

/// Colors-level form of [`increased_contrast_surface`].
///
/// The refinement pass runs before a theme is active, so it resolves the washes
/// from the colors it is refining instead of asking the app.
pub(crate) fn increased_contrast_colors(colors: &theme::ThemeColors, surface: Hsla) -> Hsla {
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

#[cfg(test)]
mod contrast_tests {
    use gpui::{Hsla, px, rgba};
    use ui::ActiveTheme as _;

    use super::{
        BACKDROP_ALPHA, DISABLED_TEXT_MIN_CONTRAST, INCREASED_CONTRAST_GRAPHIC_MIN,
        INCREASED_CONTRAST_TEXT_MIN, INCREASED_ROW_SELECTED_ALPHA, MARKER_MIN_CONTRAST,
        ROW_FOCUS_ALPHA, ROW_SELECTED_ALPHA, ROW_STATE_MIN_CONTRAST, ROW_STATE_MIN_SEPARATION,
        ROW_STRIPE_ALPHA, SERIES_SLOTS, SURFACE_MIN_CONTRAST, Severity, TEXT_MIN_CONTRAST,
        adjusted_color_on_all, border, chart, composite_surface, confidence, contrast_ratio,
        core_surfaces, editor_wash, focus, format, graphic_on, graphic_on_with_minimum,
        health_icon, health_label, increased_accent_surfaces, increased_contrast_surface,
        increased_text_surfaces, kind_icon, marker_for_background, pod_severity,
        problems_filter_icon, refine_theme, refine_theme_with_contrast, row_height, row_stripe_bg,
        row_wash, row_washes, search_match, size, surface, text_on, text_on_for_mode,
        text_selection, worst_contrast,
    };

    const TEXT_SELECTION_MIN_SEPARATION: f32 = 1.05;

    /// The smallest perceptual gap between two adjacent neutral layers that
    /// still reads as two layers rather than one.
    ///
    /// Struck against `SURFACE_MIN_CONTRAST` (`1.05`), which only asks the
    /// raised surface to be *distinguishable* and is far too weak to build a
    /// layout on: the shipped ramp used to pass that bar while every layer sat
    /// on the same value. Light mode is held to the same floor as dark even
    /// though its steps are narrower, so a theme edit cannot quietly flatten
    /// one appearance only.
    ///
    /// In CIE L\*, which is the unit `DESIGN.md` §3.4 quotes. The test used to
    /// measure HSL lightness x100 instead, so the number the document promised
    /// and the number the test enforced were two different quantities.
    ///
    /// One floor per appearance, because the light ramp's narrowest real step is
    /// 1.77 L\* and a single floor strong enough for dark is not reachable there.
    const MIN_LAYER_SEPARATION_DARK: f32 = 2.0;
    const MIN_LAYER_SEPARATION_LIGHT: f32 = 1.5;

    /// Chrome edges need a step, not a full layer step: the tab strip and the
    /// title bar sit inside one band on purpose, and a 1px rule already marks
    /// their edges.
    const MIN_CHROME_SEPARATION: f32 = 1.0;

    /// How far a modal scrim must push the canvas for the modality to read.
    const MIN_BACKDROP_SEPARATION: f32 = 1.1;

    /// CIE L\* of a color, the unit `DESIGN.md` §3.4 quotes.
    ///
    /// Contrast ratio is the wrong tool for layer separation: near-black
    /// surfaces all cluster around `1.0` even when they are visibly different,
    /// which is exactly why the old ramp looked flat while its contrast numbers
    /// looked fine.
    fn lightness_star(color: u32) -> f32 {
        let [r, g, b, _] = color.to_be_bytes();
        let linear = [r, g, b]
            .into_iter()
            .map(|channel| f64::from(channel) / 255.0)
            .map(|channel| {
                if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                }
            })
            .zip([0.2126, 0.7152, 0.0722])
            .map(|(channel, weight)| channel * weight)
            .sum::<f64>();
        (if linear > 0.008_856 {
            116.0 * linear.powf(1.0 / 3.0) - 16.0
        } else {
            903.3 * linear
        }) as f32
    }

    fn surface_separation(a: u32, b: u32) -> f32 {
        (lightness_star(a) - lightness_star(b)).abs()
    }

    /// Reference values for One Light and One Dark.
    /// Row and selected-row colors use the same blend formula as the renderer.
    /// Stripe = surface + text 5 percent. Selected = surface + text accent 20 percent.
    const LIGHT_PANEL: u32 = 0xebebecff;
    const LIGHT_ROW: u32 = 0xdcdcddff;
    const LIGHT_SELECTED: u32 = 0xcacacaff;
    const LIGHT_SELECTED_ROW: u32 = 0xcacedeff;
    const LIGHT_EDITOR: u32 = 0xfafafaff;
    const LIGHT_TEXT: u32 = 0x242529ff;
    const LIGHT_MUTED: u32 = 0x58585aff;
    const LIGHT_SUCCESS_BG: u32 = 0xdfeadbff;
    const LIGHT_WARNING_BG: u32 = 0xfaf2e6ff;
    const LIGHT_ERROR_BG: u32 = 0xfbdfd9ff;
    const LIGHT_INFO_BG: u32 = 0xe2e2faff;

    const DARK_PANEL: u32 = 0x2f343eff;
    const DARK_ROW: u32 = 0x3b414dff;
    const DARK_SELECTED: u32 = 0x454a56ff;
    const DARK_SELECTED_ROW: u32 = 0x435063ff;
    const DARK_EDITOR: u32 = 0x282c33ff;
    const DARK_TEXT: u32 = 0xdce0e5ff;
    const DARK_MUTED: u32 = 0xa9afbcff;
    const DARK_SUCCESS_BG: u32 = 0x454e52ff;
    const DARK_WARNING_BG: u32 = 0x4c4e53ff;
    const DARK_ERROR_BG: u32 = 0x4a4651ff;
    const DARK_INFO_BG: u32 = 0x414c5dff;

    fn luminance(hex: u32) -> f64 {
        let [r, g, b, _] = hex.to_be_bytes();
        [r, g, b]
            .into_iter()
            .map(|byte| {
                let channel = f64::from(byte) / 255.0;
                if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                }
            })
            .zip([0.2126, 0.7152, 0.0722])
            .map(|(channel, weight)| channel * weight)
            .sum()
    }

    fn contrast(fg: u32, bg: u32) -> f64 {
        let (high, low) = {
            let (fg, bg) = (luminance(fg), luminance(bg));
            if fg >= bg { (fg, bg) } else { (bg, fg) }
        };
        (high + 0.05) / (low + 0.05)
    }

    fn assert_contrast(label: &str, fg: u32, bg: u32, min: f64) {
        let ratio = contrast(fg, bg);
        assert!(ratio >= min, "{label}: {ratio:.2}:1 < {min}:1");
    }

    fn parse_color(value: &str) -> u32 {
        let value = value.strip_prefix('#').expect("hex color");
        u32::from_str_radix(&value[..6], 16).expect("hex color") << 8 | 0xff
    }

    fn theme_color(theme_json: &str, theme_name: &str, key: &str) -> u32 {
        let theme: serde_json::Value = serde_json::from_str(theme_json).expect("theme JSON");
        theme["themes"]
            .as_array()
            .expect("theme list")
            .iter()
            .find(|theme| theme.get("name").and_then(serde_json::Value::as_str) == Some(theme_name))
            // Dotted top-level keys only. See `theme_defines` for why the nested
            // copies these used to fall back to were dead weight.
            .and_then(|theme| theme["style"].get(key))
            .and_then(serde_json::Value::as_str)
            .map(parse_color)
            .unwrap_or_else(|| panic!("missing theme color {theme_name}/{key}"))
    }

    fn k8s_theme_color(appearance: &str, key: &str) -> u32 {
        let theme_name = match appearance {
            "light" => "K8s Studio Light",
            "dark" => "K8s Studio Dark",
            _ => panic!("unknown K8s Studio appearance {appearance}"),
        };
        theme_color(
            include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
            theme_name,
            key,
        )
    }

    /// Whether a K8s Studio appearance defines a role at all, in either of the
    /// two places the theme file can put one.
    fn theme_defines(appearance: &str, key: &str) -> bool {
        let theme_name = match appearance {
            "light" => "K8s Studio Light",
            "dark" => "K8s Studio Dark",
            _ => panic!("unknown K8s Studio appearance {appearance}"),
        };
        let theme: serde_json::Value =
            serde_json::from_str(include_str!("../../k8s-app/assets/themes/k8s-studio.json"))
                .expect("theme JSON");
        theme["themes"]
            .as_array()
            .expect("theme list")
            .iter()
            .find(|theme| theme.get("name").and_then(serde_json::Value::as_str) == Some(theme_name))
            // Only the dotted top-level keys load: `ThemeStyleContent` declares
            // `colors` and `status` as `#[serde(flatten)]`, and a nested object of
            // either name is an unknown key that serde drops. The theme file used
            // to carry such copies, and they had already drifted from the live
            // values, so reading them reported a role as configured when the app
            // was running on a different one.
            .map(|theme| theme["style"].get(key).is_some())
            .expect("K8s Studio appearance")
    }

    fn theme_hsla(theme_json: &str, theme_name: &str, key: &str) -> Hsla {
        rgba(theme_color(theme_json, theme_name, key)).into()
    }

    fn status_key(severity: Severity) -> &'static str {
        match severity {
            Severity::Success => "success",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Info => "info",
            Severity::Neutral | Severity::Muted => "text",
        }
    }

    fn theme_marker_hsla(theme_json: &str, theme_name: &str, severity: Severity) -> Hsla {
        match severity {
            Severity::Muted => theme_hsla(theme_json, theme_name, "text.muted"),
            _ => theme_hsla(theme_json, theme_name, status_key(severity)),
        }
    }

    fn assert_marker_contrast(label: &str, marker: Hsla, background: Hsla) {
        let ratio = ui::utils::calculate_contrast_ratio(marker, background);
        assert!(
            ratio >= MARKER_MIN_CONTRAST,
            "{label}: {ratio:.2}:1 < {MARKER_MIN_CONTRAST}:1"
        );
    }

    fn assert_hsla_contrast(label: &str, foreground: Hsla, background: Hsla, minimum: f32) {
        let ratio = ui::utils::calculate_contrast_ratio(foreground, background);
        assert!(ratio >= minimum, "{label}: {ratio:.2}:1 < {minimum:.2}:1");
    }

    fn resolved_theme(theme_json: &str, theme_name: &str) -> theme::Theme {
        let family = theme_settings::refine_theme_family(
            theme_settings::deserialize_user_theme(theme_json.as_bytes()).expect("theme JSON"),
        );
        family
            .themes
            .into_iter()
            .find(|theme| theme.name.as_ref() == theme_name)
            .unwrap_or_else(|| panic!("missing resolved theme {theme_name}"))
    }

    fn assert_refined_theme_contract(label: &str, theme: &theme::Theme) {
        let colors = theme.colors();
        let status = theme.status();
        let backgrounds = [
            colors.background,
            colors.panel_background,
            colors.surface_background,
            colors.elevated_surface_background,
            colors.editor_background,
            colors.editor_gutter_background,
            colors.element_selected,
        ];
        for (name, color) in [
            ("background", colors.background),
            ("panel", colors.panel_background),
            ("surface", colors.surface_background),
            ("raised", colors.elevated_surface_background),
            ("editor", colors.editor_background),
            ("editor gutter", colors.editor_gutter_background),
            ("terminal", colors.terminal_background),
        ] {
            assert_eq!(color.a, 1.0, "{label} {name} is not opaque");
        }
        assert!(
            contrast_ratio(colors.elevated_surface_background, colors.panel_background)
                >= SURFACE_MIN_CONTRAST,
            "{label} raised and panel have no separation"
        );
        assert!(
            contrast_ratio(colors.elevated_surface_background, colors.editor_background)
                >= SURFACE_MIN_CONTRAST,
            "{label} raised and editor have no separation"
        );
        let [stripe, hover, focus, selected] =
            row_washes(colors, ROW_SELECTED_ALPHA, TEXT_MIN_CONTRAST);
        let selection = composite_surface(
            colors.editor_background,
            colors.element_selection_background,
        );
        let search = composite_surface(
            colors.editor_background,
            search_match::resolve_background(colors),
        );
        let active_search = composite_surface(
            colors.editor_background,
            search_match::resolve_active_background(colors),
        );
        let text_backgrounds = [
            colors.background,
            colors.panel_background,
            colors.surface_background,
            colors.elevated_surface_background,
            colors.editor_background,
            colors.editor_gutter_background,
            colors.element_selected,
            stripe,
            hover,
            selected,
            focus,
            selection,
            search,
            active_search,
        ];
        for foreground in [colors.text, colors.text_muted, colors.text_placeholder] {
            for background in text_backgrounds {
                assert_hsla_contrast(
                    &format!("{label} text"),
                    foreground,
                    background,
                    TEXT_MIN_CONTRAST,
                );
            }
        }
        // A disabled control is exempt from the body-text floor, but it still has
        // to clear its own, and it has to stay the quietest of the text roles.
        for background in text_backgrounds {
            assert_hsla_contrast(
                &format!("{label} disabled text"),
                colors.text_disabled,
                background,
                DISABLED_TEXT_MIN_CONTRAST,
            );
        }
        assert!(
            worst_contrast(colors.text_disabled, &text_backgrounds)
                < worst_contrast(colors.text_muted, &text_backgrounds),
            "{label} disabled text is not quieter than text_muted on the surface that binds \
             them"
        );
        for background in [
            colors.background,
            colors.panel_background,
            colors.surface_background,
        ]
        .into_iter()
        .chain([
            colors.elevated_surface_background,
            colors.editor_background,
            colors.editor_gutter_background,
        ]) {
            assert_hsla_contrast(
                &format!("{label} accent"),
                colors.text_accent,
                background,
                TEXT_MIN_CONTRAST,
            );
        }
        // The accent is body text on the surfaces above and a rail on the two rows it
        // tints itself, and the solver deliberately keeps the two apart: solving the
        // accent against a wash built out of the accent hands it its own output and the
        // two chase each other to the end of the range. A rail is a graphic, so the
        // graphic floor is what the rail has to clear on the row it marks.
        for (row, background) in [("focus", focus), ("selected", selected)] {
            assert_hsla_contrast(
                &format!("{label} accent rail on the {row} row"),
                colors.text_accent,
                background,
                MARKER_MIN_CONTRAST,
            );
        }
        for foreground in [
            colors.border_focused,
            colors.border_selected,
            colors.drop_target_border,
            colors.pane_focused_border,
            colors.icon,
            colors.icon_muted,
        ] {
            for background in backgrounds {
                assert_hsla_contrast(
                    &format!("{label} graphic"),
                    foreground,
                    background,
                    MARKER_MIN_CONTRAST,
                );
            }
        }
        assert_eq!(colors.editor_foreground, colors.text);
        assert_eq!(colors.editor_line_number, colors.text_muted);
        for foreground in [
            colors.terminal_foreground,
            colors.terminal_bright_foreground,
            colors.terminal_dim_foreground,
        ] {
            assert_hsla_contrast(
                &format!("{label} terminal"),
                foreground,
                colors.terminal_background,
                TEXT_MIN_CONTRAST,
            );
        }
        let statuses = [
            (
                status.success,
                status.success_background,
                status.success_border,
            ),
            (
                status.warning,
                status.warning_background,
                status.warning_border,
            ),
            (status.error, status.error_background, status.error_border),
            (status.info, status.info_background, status.info_border),
        ];
        for (foreground, status_background, border) in statuses {
            assert_eq!(status_background.a, 1.0, "{label} status is not opaque");
            for background in backgrounds {
                assert_hsla_contrast(
                    &format!("{label} status"),
                    foreground,
                    background,
                    TEXT_MIN_CONTRAST,
                );
                assert_hsla_contrast(
                    &format!("{label} status border"),
                    border,
                    background,
                    MARKER_MIN_CONTRAST,
                );
            }
            assert_hsla_contrast(
                &format!("{label} status body"),
                colors.text,
                status_background,
                TEXT_MIN_CONTRAST,
            );
        }
        for accent in theme.accents().0.iter() {
            for background in [colors.panel_background, colors.background] {
                assert_hsla_contrast(
                    &format!("{label} accent list"),
                    *accent,
                    background,
                    MARKER_MIN_CONTRAST,
                );
            }
        }
    }

    fn k8s_contrast_snapshot(theme: &theme::Theme) -> Vec<f32> {
        let colors = theme.colors();
        let status = theme.status();
        let backgrounds = [
            colors.background,
            colors.panel_background,
            colors.surface_background,
            colors.elevated_surface_background,
            colors.editor_background,
            colors.editor_gutter_background,
            colors.element_selected,
        ];
        let [stripe, hover, focus, selected] =
            row_washes(colors, ROW_SELECTED_ALPHA, TEXT_MIN_CONTRAST);
        let mut snapshot = Vec::new();
        for foreground in [colors.text, colors.text_muted, colors.text_placeholder] {
            for background in backgrounds
                .into_iter()
                .chain([stripe, hover, selected, focus])
            {
                snapshot.push(contrast_ratio(foreground, background));
            }
        }
        for background in backgrounds.into_iter().chain([selected, focus]) {
            snapshot.push(contrast_ratio(colors.text_accent, background));
        }
        for foreground in [
            colors.border_focused,
            colors.border_selected,
            colors.drop_target_border,
            colors.pane_focused_border,
            colors.icon,
            colors.icon_muted,
        ] {
            for background in backgrounds {
                snapshot.push(contrast_ratio(foreground, background));
            }
        }
        for foreground in [
            colors.terminal_foreground,
            colors.terminal_bright_foreground,
            colors.terminal_dim_foreground,
        ] {
            snapshot.push(contrast_ratio(foreground, colors.terminal_background));
        }
        for (foreground, status_background, border) in [
            (
                status.success,
                status.success_background,
                status.success_border,
            ),
            (
                status.warning,
                status.warning_background,
                status.warning_border,
            ),
            (status.error, status.error_background, status.error_border),
            (status.info, status.info_background, status.info_border),
        ] {
            snapshot.push(contrast_ratio(foreground, colors.background));
            snapshot.push(contrast_ratio(border, colors.background));
            snapshot.push(contrast_ratio(colors.text, status_background));
        }
        for accent in theme.accents().0.iter() {
            snapshot.push(contrast_ratio(*accent, colors.background));
            snapshot.push(contrast_ratio(*accent, colors.panel_background));
        }
        snapshot
    }

    fn assert_text_selection_theme(label: &str, colors: &theme::ThemeColors) {
        let editor = colors.editor_background.alpha(1.0);
        let selection = composite_surface(editor, colors.element_selection_background);
        let search = composite_surface(editor, search_match::resolve_background(colors));
        let active_search =
            composite_surface(editor, search_match::resolve_active_background(colors));
        for (name, surface) in [
            ("selected", selection),
            ("search match", search),
            ("active search", active_search),
        ] {
            let foreground = text_on(surface, colors.text, colors.text_accent);
            assert_hsla_contrast(
                &format!("{label} {name} text"),
                foreground,
                surface,
                TEXT_MIN_CONTRAST,
            );
        }
        assert_hsla_contrast(
            &format!("{label} selection separation"),
            selection,
            editor,
            TEXT_SELECTION_MIN_SEPARATION,
        );
        assert_ne!(search, selection, "{label} search selection collapsed");
        assert_ne!(
            active_search, selection,
            "{label} active selection collapsed"
        );
        assert_ne!(active_search, search, "{label} active search collapsed");
    }

    #[test]
    fn status_bar_has_room_for_desktop_controls() {
        assert!(f32::from(size::STATUS_BAR) >= 28.0);
    }

    #[test]
    fn dock_min_holds_the_fixed_chrome_and_three_log_rows() {
        let chrome = size::TAB_BAR + size::TOOLBAR + size::ROW;
        assert_eq!(size::DOCK_MIN, px(156.));
        assert!(size::DOCK_MIN >= chrome + super::text::DATA_LINE_HEIGHT * 3.0);
        assert!(size::DOCK_MIN < size::DOCK_MAX);
    }

    #[test]
    fn semantic_surface_contract_uses_existing_theme_tokens() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../theme.json")).expect("theme contract");
        assert_eq!(
            contract["surfaces"],
            serde_json::json!({
                "editor": "editor.background",
                "input": "surface.background",
                "raised": "elevated_surface.background",
                "selected": "element.selected",
                "tab_bar": "tab_bar.background",
                "tab_active": "tab.active_background"
            })
        );
        assert_eq!(
            contract["editor_wash"]["active_line"],
            "editor.active_line.background"
        );
        assert_eq!(contract["editor_wash"]["diagnostic"], "error.background");
        let registered_alpha = contract["editor_wash"]["diagnostic_alpha"]
            .as_f64()
            .expect("diagnostic alpha");
        assert!(
            (registered_alpha - f64::from(editor_wash::DIAGNOSTIC_ALPHA)).abs() < 0.0001,
            "registered alpha {registered_alpha} is not the rendered alpha"
        );
        assert_eq!(
            contract["text_selection"],
            serde_json::json!({
                "background": "element.selection_background",
                "foreground": "text",
                "foreground_fallback": "text.accent"
            })
        );
        assert_eq!(
            contract["search_match"],
            serde_json::json!({
                "background": "search.match_background",
                "active_background": "search.active_match_background"
            })
        );
        assert_ne!(
            contract["text_selection"]["background"],
            contract["search_match"]["background"]
        );
        assert_ne!(
            contract["surfaces"]["tab_bar"], contract["surfaces"]["tab_active"],
            "the active tab has to read as a tab, not as the bar behind it"
        );
        assert_ne!(
            contract["editor_wash"]["active_line"],
            contract["editor_wash"]["diagnostic"]
        );
        for appearance in ["light", "dark"] {
            for token in [
                "editor.background",
                "surface.background",
                "elevated_surface.background",
                "element.selected",
                "tab_bar.background",
                "tab.active_background",
                "element.selection_background",
                "text",
                "text.accent",
                "search.match_background",
                "search.active_match_background",
                "editor.active_line.background",
                "error.background",
                "confidence.unknown",
                "confidence.stale",
            ] {
                let _ = k8s_theme_color(appearance, token);
            }
        }
    }

    /// A theme role with no reader is worse than no role.
    ///
    /// `status_bar.background` was defined in both appearances and read by
    /// nothing except this file's own refine pass: the status bar paints
    /// `panel.background`, so the v1.2 note claiming the status bar left the
    /// panel's value was true of the JSON and not of the app. `unreachable` and
    /// its background and border were the other half of the same problem: three
    /// live keys, no `Severity` for them, no code that reads them, and a border
    /// that measured between 1.07:1 and 1.84:1 across twenty surfaces, so it
    /// never once reached the marker threshold it was named for.
    #[test]
    fn the_theme_carries_no_role_without_a_reader() {
        for appearance in ["light", "dark"] {
            for key in [
                "status_bar.background",
                "unreachable",
                "unreachable.background",
                "unreachable.border",
            ] {
                assert!(
                    !theme_defines(appearance, key),
                    "{appearance} still defines {key}, which nothing reads"
                );
            }
        }
    }

    /// Every series slot must be legible as a one-pixel line on the plot canvas.
    ///
    /// The K8s Studio theme defines no `accents` list, so the pool is the crate
    /// default and is the same in both appearances — which is why one live read
    /// can be checked against both canvases. The lighter members of that pool sit
    /// near 1.3:1 on the light canvas, so `chart::series` solves the hue against
    /// the surface it is actually drawn on rather than assuming the accent
    /// carries.
    #[gpui::test]
    fn series_colors_clear_the_graphic_threshold_on_both_canvases(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let theme_json = include_str!("../../k8s-app/assets/themes/k8s-studio.json");
        cx.update(|cx| {
            let hues: Vec<Hsla> = (0..SERIES_SLOTS as u32)
                .map(|index| cx.theme().accents().color_for_index(index))
                .collect();
            for (appearance, theme_name) in
                [("light", "K8s Studio Light"), ("dark", "K8s Studio Dark")]
            {
                let canvas = theme_hsla(theme_json, theme_name, "background");
                for (index, hue) in hues.iter().enumerate() {
                    let solved = graphic_on_with_minimum(canvas, *hue, MARKER_MIN_CONTRAST);
                    assert_hsla_contrast(
                        &format!("{appearance} series {index}"),
                        solved,
                        canvas,
                        MARKER_MIN_CONTRAST,
                    );
                }
            }
        });
        // The solver is the whole point, so prove the raw pool is not already
        // sufficient. Measured from the same live accent values, not asserted
        // from memory: this is the regression the test exists to catch.
        cx.update(|cx| {
            let light = theme_hsla(theme_json, "K8s Studio Light", "background");
            let worst = (0..SERIES_SLOTS as u32)
                .map(|index| cx.theme().accents().color_for_index(index))
                .map(|hue| contrast_ratio(hue, light))
                .fold(f32::INFINITY, f32::min);
            assert!(
                worst < MARKER_MIN_CONTRAST,
                "the accent pool now clears the threshold on its own ({worst:.2}:1), so the solver is no longer needed"
            );
        });
    }

    #[gpui::test]
    fn semantic_surfaces_resolve_to_theme_colors(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            let colors = cx.theme().colors();
            assert_eq!(surface::canvas(cx), colors.background);
            assert_eq!(surface::panel(cx), colors.panel_background);
            assert_eq!(surface::editor(cx), colors.editor_background);
            assert_eq!(surface::input(cx), colors.surface_background);
            assert_eq!(surface::raised(cx), colors.elevated_surface_background);
            assert_eq!(surface::control(cx), colors.element_background);
            assert_eq!(
                surface::notification(cx),
                colors.elevated_surface_background
            );
            assert_eq!(surface::selected(cx), colors.element_selected);
            assert_eq!(surface::tab_bar(cx), colors.tab_bar_background);
            assert_eq!(surface::tab_active(cx), colors.tab_active_background);
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
                cx.theme()
                    .status()
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

    #[test]
    fn text_selection_falls_back_to_accent_on_a_solid_background() {
        let background: Hsla = rgba(0x6b6b6bff).into();
        let text: Hsla = rgba(0x777777ff).into();
        let accent: Hsla = rgba(0xffffffff).into();
        let foreground = text_on(background, text, accent);
        assert_eq!(foreground, accent);
        assert_hsla_contrast(
            "solid selection text",
            foreground,
            background,
            TEXT_MIN_CONTRAST,
        );
    }

    #[test]
    fn text_selection_stays_readable_on_a_composite_background() {
        let editor: Hsla = rgba(0xffffffff).into();
        let overlay: Hsla = rgba(0x1557c73d).into();
        let background = composite_surface(editor, overlay);
        let text: Hsla = rgba(0x16202aff).into();
        let accent: Hsla = rgba(0x1557c7ff).into();
        let foreground = text_on(background, text, accent);
        assert_eq!(foreground, text);
        assert_hsla_contrast(
            "composite selection text",
            foreground,
            background,
            TEXT_MIN_CONTRAST,
        );
        assert_hsla_contrast(
            "composite selection separation",
            background,
            editor,
            TEXT_SELECTION_MIN_SEPARATION,
        );
    }

    #[test]
    fn text_selection_roles_work_after_refinement() {
        for (theme_json, theme_names) in [
            (
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                &["K8s Studio Light", "K8s Studio Dark"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/ayu/ayu.json"),
                &["Ayu Dark", "Ayu Light", "Ayu Mirage"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                &[
                    "Gruvbox Dark",
                    "Gruvbox Dark Hard",
                    "Gruvbox Dark Soft",
                    "Gruvbox Light",
                    "Gruvbox Light Hard",
                    "Gruvbox Light Soft",
                ][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                &["One Light", "One Dark"][..],
            ),
        ] {
            for theme_name in theme_names {
                let mut theme = resolved_theme(theme_json, theme_name);
                refine_theme(&mut theme);
                assert_text_selection_theme(theme_name, theme.colors());
            }
        }
    }

    #[test]
    fn text_and_graphic_helpers_guarantee_their_thresholds() {
        let background: Hsla = rgba(0x6b6b6bff).into();
        let preferred: Hsla = rgba(0x707070ff).into();
        let fallback: Hsla = rgba(0x737373ff).into();
        let text = text_on(background, preferred, fallback);
        assert_hsla_contrast("text helper", text, background, TEXT_MIN_CONTRAST);
        let graphic = graphic_on(background, preferred);
        assert_marker_contrast("graphic helper", graphic, background);
    }

    #[test]
    fn refine_theme_covers_eleven_zed_themes() {
        let families = [
            (
                include_str!("../../../reference/zed/assets/themes/ayu/ayu.json"),
                &["Ayu Dark", "Ayu Light", "Ayu Mirage"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                &[
                    "Gruvbox Dark",
                    "Gruvbox Dark Hard",
                    "Gruvbox Dark Soft",
                    "Gruvbox Light",
                    "Gruvbox Light Hard",
                    "Gruvbox Light Soft",
                ][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                &["One Light", "One Dark"][..],
            ),
        ];
        let mut count = 0;
        for (theme_json, theme_names) in families {
            for theme_name in theme_names {
                let mut theme = resolved_theme(theme_json, theme_name);
                let background = theme.colors().background;
                let syntax = theme.syntax().clone();
                let untouched_status = [
                    (
                        theme.status().conflict,
                        theme.status().conflict_background,
                        theme.status().conflict_border,
                    ),
                    (
                        theme.status().created,
                        theme.status().created_background,
                        theme.status().created_border,
                    ),
                    (
                        theme.status().deleted,
                        theme.status().deleted_background,
                        theme.status().deleted_border,
                    ),
                    (
                        theme.status().hidden,
                        theme.status().hidden_background,
                        theme.status().hidden_border,
                    ),
                    (
                        theme.status().hint,
                        theme.status().hint_background,
                        theme.status().hint_border,
                    ),
                    (
                        theme.status().ignored,
                        theme.status().ignored_background,
                        theme.status().ignored_border,
                    ),
                    (
                        theme.status().modified,
                        theme.status().modified_background,
                        theme.status().modified_border,
                    ),
                    (
                        theme.status().predictive,
                        theme.status().predictive_background,
                        theme.status().predictive_border,
                    ),
                    (
                        theme.status().renamed,
                        theme.status().renamed_background,
                        theme.status().renamed_border,
                    ),
                    (
                        theme.status().unreachable,
                        theme.status().unreachable_background,
                        theme.status().unreachable_border,
                    ),
                ];
                refine_theme(&mut theme);
                assert_eq!(theme.colors().background, background);
                assert!(std::sync::Arc::ptr_eq(theme.syntax(), &syntax));
                let status = theme.status();
                assert_eq!(
                    [
                        (
                            status.conflict,
                            status.conflict_background,
                            status.conflict_border,
                        ),
                        (
                            status.created,
                            status.created_background,
                            status.created_border,
                        ),
                        (
                            status.deleted,
                            status.deleted_background,
                            status.deleted_border,
                        ),
                        (
                            status.hidden,
                            status.hidden_background,
                            status.hidden_border,
                        ),
                        (status.hint, status.hint_background, status.hint_border,),
                        (
                            status.ignored,
                            status.ignored_background,
                            status.ignored_border,
                        ),
                        (
                            status.modified,
                            status.modified_background,
                            status.modified_border,
                        ),
                        (
                            status.predictive,
                            status.predictive_background,
                            status.predictive_border,
                        ),
                        (
                            status.renamed,
                            status.renamed_background,
                            status.renamed_border,
                        ),
                        (
                            status.unreachable,
                            status.unreachable_background,
                            status.unreachable_border,
                        ),
                    ],
                    untouched_status
                );
                assert_refined_theme_contract(theme_name, &theme);
                let once = theme.clone();
                refine_theme(&mut theme);
                assert_eq!(theme.styles.colors, once.styles.colors);
                assert_eq!(theme.styles.status, once.styles.status);
                assert_eq!(theme.styles.accents, once.styles.accents);
                count += 1;
            }
        }
        assert_eq!(count, 11);
    }

    #[test]
    fn refine_theme_does_not_reduce_k8s_contrast() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            let mut theme = resolved_theme(
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                theme_name,
            );
            let before = k8s_contrast_snapshot(&theme);
            refine_theme(&mut theme);
            let after = k8s_contrast_snapshot(&theme);
            assert_eq!(before.len(), after.len());
            for (index, (before, after)) in before.iter().zip(after.iter()).enumerate() {
                assert!(
                    *after + 0.0001 >= *before,
                    "{theme_name} contrast {index}: {after:.3} < {before:.3}"
                );
            }
        }
    }

    #[test]
    fn increased_contrast_covers_every_supported_theme() {
        let families = [
            (
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                &["K8s Studio Light", "K8s Studio Dark"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/ayu/ayu.json"),
                &["Ayu Dark", "Ayu Light", "Ayu Mirage"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                &[
                    "Gruvbox Dark",
                    "Gruvbox Dark Hard",
                    "Gruvbox Dark Soft",
                    "Gruvbox Light",
                    "Gruvbox Light Hard",
                    "Gruvbox Light Soft",
                ][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                &["One Light", "One Dark"][..],
            ),
        ];
        for (theme_json, theme_names) in families {
            for theme_name in theme_names {
                let mut theme = resolved_theme(theme_json, theme_name);
                let original_status = theme.status().success;
                let original_info = theme.status().info;
                refine_theme_with_contrast(&mut theme, true);
                let colors = theme.colors();
                let status = theme.status();
                let surfaces =
                    increased_text_surfaces(colors, status, INCREASED_ROW_SELECTED_ALPHA);
                // The accent is the selection's identity rather than text that
                // sits on the selection, so it is not solved against the accent
                // wash and is not held to the text floor on it. Everything else
                // is.
                let accent_surfaces =
                    increased_accent_surfaces(colors, status, INCREASED_ROW_SELECTED_ALPHA);
                for (foreground_index, foreground) in [
                    colors.text,
                    colors.text_muted,
                    colors.text_placeholder,
                    colors.text_disabled,
                ]
                .into_iter()
                .enumerate()
                {
                    for (surface_index, &background) in surfaces.iter().enumerate() {
                        assert_hsla_contrast(
                            &format!(
                                "{theme_name} increased text {foreground_index} on surface {surface_index}: {foreground:?} / {background:?}"
                            ),
                            foreground,
                            background,
                            INCREASED_CONTRAST_TEXT_MIN,
                        );
                    }
                }
                for (surface_index, &background) in accent_surfaces.iter().enumerate() {
                    assert_hsla_contrast(
                        &format!(
                            "{theme_name} increased accent on surface {surface_index}: {background:?}"
                        ),
                        colors.text_accent,
                        background,
                        INCREASED_CONTRAST_TEXT_MIN,
                    );
                }
                // The rail the accent paints on the rows it tints is a graphic, and the
                // washes it lands on are built out of the accent itself, so the rail is
                // held to the raised graphic floor there rather than to the text one.
                let [_, _, focus, selected] = row_washes(
                    colors,
                    INCREASED_ROW_SELECTED_ALPHA,
                    INCREASED_CONTRAST_TEXT_MIN,
                );
                for (row, background) in [("focus", focus), ("selected", selected)] {
                    assert_hsla_contrast(
                        &format!("{theme_name} increased accent rail on the {row} row"),
                        colors.text_accent,
                        background,
                        INCREASED_CONTRAST_GRAPHIC_MIN,
                    );
                }
                for foreground in [
                    colors.border,
                    colors.border_variant,
                    colors.border_disabled,
                    colors.border_focused,
                    colors.border_selected,
                    colors.drop_target_border,
                    colors.panel_focused_border,
                    colors.pane_focused_border,
                    colors.pane_group_border,
                    colors.scrollbar_thumb_border,
                    colors.scrollbar_track_border,
                    colors.minimap_thumb_border,
                    colors.icon,
                    colors.icon_muted,
                    colors.icon_disabled,
                    colors.icon_placeholder,
                    colors.icon_accent,
                    colors.debugger_accent,
                ] {
                    for &background in &surfaces {
                        assert_hsla_contrast(
                            &format!("{theme_name} increased graphic"),
                            foreground,
                            background,
                            INCREASED_CONTRAST_GRAPHIC_MIN,
                        );
                    }
                }
                for (foreground, border) in [
                    (status.success, status.success_border),
                    (status.warning, status.warning_border),
                    (status.error, status.error_border),
                    (status.info, status.info_border),
                ] {
                    for &background in &surfaces {
                        assert_hsla_contrast(
                            &format!("{theme_name} increased status"),
                            foreground,
                            background,
                            INCREASED_CONTRAST_TEXT_MIN,
                        );
                        assert_hsla_contrast(
                            &format!("{theme_name} increased status border"),
                            border,
                            background,
                            INCREASED_CONTRAST_GRAPHIC_MIN,
                        );
                    }
                }
                let editor = colors.editor_background;
                // The real washes, not a stand-in: a diagnostic that shares the
                // cursor wash cannot be told apart from "this is where I am".
                let active_line = composite_surface(editor, colors.editor_active_line_background);
                let diagnostic = composite_surface(
                    editor,
                    status
                        .error_background
                        .opacity(editor_wash::DIAGNOSTIC_ALPHA),
                );
                let search_background =
                    composite_surface(editor, search_match::resolve_background(colors));
                let active_search_background =
                    composite_surface(editor, search_match::resolve_active_background(colors));
                let selection_background =
                    composite_surface(editor, colors.element_selection_background);
                assert_ne!(search_background, active_line);
                assert_ne!(active_search_background, active_line);
                assert_ne!(active_search_background, search_background);
                assert_ne!(
                    diagnostic, active_line,
                    "diagnostic collapsed onto the cursor line"
                );
                assert_ne!(diagnostic, selection_background);
                assert_ne!(diagnostic, search_background);
                assert_ne!(diagnostic, active_search_background);
                for syntax_name in [
                    "property",
                    "string",
                    "number",
                    "boolean",
                    "constant",
                    "comment",
                    "punctuation",
                    "variant",
                    "type",
                ] {
                    let syntax = theme
                        .syntax()
                        .style_for_name(syntax_name)
                        .and_then(|style| style.color)
                        .unwrap_or(colors.editor_foreground);
                    for (surface_index, background) in [
                        editor,
                        active_line,
                        diagnostic,
                        search_background,
                        active_search_background,
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let foreground =
                            text_on_for_mode(background, syntax, colors.editor_foreground, true);
                        assert_hsla_contrast(
                            &format!(
                                "{theme_name} increased syntax {syntax_name} on surface {surface_index}: {background:?}"
                            ),
                            foreground,
                            background,
                            INCREASED_CONTRAST_TEXT_MIN,
                        );
                    }
                }
                for background in [
                    editor,
                    active_line,
                    diagnostic,
                    search_background,
                    active_search_background,
                ] {
                    let selected = composite_surface(background, colors.element_selected);
                    let foreground =
                        text_on_for_mode(selected, colors.text, colors.text_accent, true);
                    assert_hsla_contrast(
                        &format!("{theme_name} increased selection"),
                        foreground,
                        selected,
                        INCREASED_CONTRAST_TEXT_MIN,
                    );
                }
                assert!((status.success.h - original_status.h).abs() < 0.000001);
                assert!((status.success.s - original_status.s).abs() < 0.000001);
                assert!((status.info.h - original_info.h).abs() < 0.000001);
                assert!((status.info.s - original_info.s).abs() < 0.000001);
                let once = theme.clone();
                refine_theme_with_contrast(&mut theme, true);
                assert_eq!(theme.styles.colors, once.styles.colors);
                assert_eq!(theme.styles.status, once.styles.status);
                assert_eq!(theme.styles.accents, once.styles.accents);
            }
        }
    }

    /// Every layer must be distinguishable from the one behind it, in both
    /// appearances.
    ///
    /// The theme used to ship `panel == tab_bar == status_bar` and
    /// `surface == elevated_surface`, so the whole app had one value per
    /// appearance and every boundary was carried by a 1px border. A modal
    /// dissolved into the table it covered. `color.md > Best practices` asks
    /// custom colors to give "a significantly higher amount of visual
    /// differentiation" between variants, which is what these floors encode.
    #[test]
    fn k8s_studio_layers_are_visibly_distinct() {
        // The four neutral layers, back to front. A modal is a fifth layer for
        // this purpose: it lands on the YAML editor more often than on a panel,
        // and the editor is the one surface the ramp in `DESIGN.md` §3.4 does not
        // name, so the pair used to be free to collapse to the same white.
        const LAYERS: [&str; 5] = [
            "background",
            "panel.background",
            "surface.background",
            "elevated_surface.background",
            "editor.background",
        ];
        // Chrome that shares the panel's value must still be separable, or the
        // sidebar, the tab bar, and the status bar become one undifferentiated
        // band. These are allowed a smaller step than a full layer because a
        // 1px rule already marks their edges.
        const PANEL_CHROME: [&str; 2] = ["tab_bar.background", "title_bar.background"];
        // Chrome that sits directly against another band. The tab strip used to
        // take `surface.background` bit for bit, so on screen it was brighter
        // than the toolbar above it and the table below it while the contract
        // files it under `panel`; and in the other appearance the title bar and
        // the tab bar were the same value, so a structural step existed in one
        // appearance and not the other.
        const CHROME_NEIGHBOURS: [(&str, &str); 3] = [
            ("tab_bar.background", "surface.background"),
            ("tab_bar.background", "background"),
            ("title_bar.background", "tab_bar.background"),
        ];

        for appearance in ["light", "dark"] {
            let floor = if appearance == "light" {
                MIN_LAYER_SEPARATION_LIGHT
            } else {
                MIN_LAYER_SEPARATION_DARK
            };
            for pair in LAYERS.windows(2) {
                let (behind, front) = (pair[0], pair[1]);
                let ratio = surface_separation(
                    k8s_theme_color(appearance, behind),
                    k8s_theme_color(appearance, front),
                );
                assert!(
                    ratio >= floor,
                    "{appearance}: {front} sits on {behind} at {ratio:.3} L*, which reads as the \
                     same layer. Raise the ramp step in k8s-studio.json.",
                );
            }
            for key in PANEL_CHROME {
                let ratio = surface_separation(
                    k8s_theme_color(appearance, "panel.background"),
                    k8s_theme_color(appearance, key),
                );
                assert!(
                    ratio >= MIN_CHROME_SEPARATION,
                    "{appearance}: {key} matches panel.background at {ratio:.3} L*, so the \
                     sidebar, tab bar, and title bar read as one band.",
                );
            }
            for (key, neighbour) in CHROME_NEIGHBOURS {
                let ratio = surface_separation(
                    k8s_theme_color(appearance, key),
                    k8s_theme_color(appearance, neighbour),
                );
                assert!(
                    ratio >= MIN_CHROME_SEPARATION,
                    "{appearance}: {key} matches {neighbour} at {ratio:.3} L*, so the two bands \
                     read as one.",
                );
            }
            // The terminal is content *inside* the Dock's panel, so it sits one
            // step in front of that panel in both appearances. The sign is the
            // whole assertion: the pair used to be a hole below its container in
            // dark and a card above it in light, and `surface_separation` takes an
            // absolute value, so "they differ" could not see that. A terminal
            // below the panel that holds it means the Dock reads as a frame around
            // a pit.
            // `lightness_star` is signed; `surface_separation` is its absolute
            // value, which is exactly what cannot see an inverted pair.
            let panel = lightness_star(k8s_theme_color(appearance, "panel.background"));
            let terminal = lightness_star(k8s_theme_color(appearance, "terminal.background"));
            let step = terminal - panel;
            assert!(
                step > 0.,
                "{appearance}: terminal.background is {terminal:.3} L* and panel.background is \
                 {panel:.3} L*, so the terminal is not in front of the panel containing it. \
                 Raise the ramp step in k8s-studio.json.",
            );
            assert!(
                step >= floor,
                "{appearance}: terminal.background sits {step:.3} L* above panel.background, \
                 under the {floor:.1} L* a layer step needs to read as a layer. Raise the ramp \
                 step in k8s-studio.json.",
            );
        }
    }

    /// The table's rows sit on `surface.background`, and nothing else does.
    ///
    /// `DESIGN.md` §3.4 files `surface` under tables and inputs, but the table
    /// painted its rows on `canvas`, which left the one level a reader stares at
    /// for eight hours consumed by a 34px tab strip. The contract, the theme
    /// value and the tests were all correct, so nothing caught it. This pins the
    /// base every row wash composites onto: the table's base *is*
    /// `surface.background`, so a table agent that paints a row on anything else
    /// fails here.
    #[test]
    fn the_table_paints_its_rows_on_the_surface_level() {
        for (appearance, theme_name) in [("light", "K8s Studio Light"), ("dark", "K8s Studio Dark")]
        {
            let mut theme = resolved_theme(
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                theme_name,
            );
            refine_theme(&mut theme);
            let colors = theme.colors();
            let [stripe, _, _, selected] =
                row_washes(colors, ROW_SELECTED_ALPHA, TEXT_MIN_CONTRAST);
            let canvas = colors.background;
            let table = colors.surface_background;
            assert_ne!(
                table, canvas,
                "{appearance}: the table needs its own level for this test to mean anything"
            );
            for (name, wash, overlay) in [
                ("stripe", stripe, colors.text.opacity(ROW_STRIPE_ALPHA)),
                (
                    "selected",
                    selected,
                    colors.text_accent.opacity(ROW_SELECTED_ALPHA),
                ),
            ] {
                // Both appearances have to be checked against the base rather than
                // against whichever level the wash ends up nearest: the light table is
                // the *lighter* level, so a wash built out of the text walks toward the
                // canvas and "closer to" cannot tell the two bases apart. The overlay is
                // what the row is made of, so the row is the table's version of it. The
                // wash is solved rather than composited, so the comparison has to be too
                // — otherwise it would only agree for the two washes the solver leaves
                // alone and would stop being able to tell a table base from a canvas one.
                let on_canvas = row_wash(canvas, overlay);
                assert_ne!(
                    on_canvas, table,
                    "{appearance}: the {name} wash would be the same on either level, so this \
                     test could not tell them apart"
                );
                assert_eq!(
                    wash,
                    row_wash(table, overlay),
                    "{appearance}: the {name} wash is not built off the table's surface"
                );
            }
        }
    }

    /// The scrim a modal draws has to actually darken the content behind it in
    /// both appearances.
    ///
    /// Lightening the scrim in dark mode was tried and reads as a grey card on
    /// a grey field: the app is already near-black, so a light scrim lifts the
    /// background toward the dialog's own value and the dialog stops being the
    /// brightest thing on screen. `modality.md > Best practices` only needs the
    /// content to read as "not interactive any more", which a dark scrim does
    /// in both appearances.
    #[test]
    fn backdrop_darkens_the_content_in_both_appearances() {
        for appearance in ["light", "dark"] {
            let canvas = Hsla::from(rgba(k8s_theme_color(appearance, "background")));
            let raised = Hsla::from(rgba(k8s_theme_color(
                appearance,
                "elevated_surface.background",
            )));
            let scrimmed = canvas.blend(Hsla::black().opacity(BACKDROP_ALPHA));
            assert!(
                scrimmed.l < canvas.l,
                "{appearance}: the scrim must darken the canvas, got {scrimmed:?} from {canvas:?}"
            );
            assert!(
                scrimmed.l < raised.l,
                "{appearance}: the scrim must sit below the dialog it frames, got {scrimmed:?} \
                 against raised {raised:?}"
            );
            let shifted = (canvas.l - scrimmed.l) * 100.0;
            assert!(
                shifted >= MIN_BACKDROP_SEPARATION,
                "{appearance}: the scrim only shifts the canvas by {shifted:.3}; it will not \
                 read as a modal.",
            );
        }
    }

    /// Health and confidence must never share a glyph, or the two channels
    /// become one when a row shows both.
    #[test]
    fn health_and_confidence_shapes_are_disjoint() {
        use super::{Confidence, confidence, health_icon};
        let health = [
            health_icon(Severity::Success),
            health_icon(Severity::Warning),
            health_icon(Severity::Error),
            health_icon(Severity::Info),
            health_icon(Severity::Neutral),
        ];
        for state in [Confidence::Stale, Confidence::Unknown] {
            let mark = confidence::icon(state);
            assert!(
                !health.contains(&mark),
                "confidence {state:?} reuses a health glyph, so the two channels collapse",
            );
        }
        // And the severities must be mutually distinct, or color alone is
        // carrying status, which `color.md > Inclusive color` rules out.
        for (i, icon) in health.iter().enumerate() {
            for other in &health[i + 1..] {
                assert_ne!(icon, other, "two severities share a glyph");
            }
        }
    }

    #[test]
    fn k8s_studio_text_tokens_meet_wcag_aa() {
        for (appearance, surfaces) in [
            (
                "light",
                [
                    "background",
                    "panel.background",
                    "surface.background",
                    "elevated_surface.background",
                ],
            ),
            (
                "dark",
                [
                    "background",
                    "panel.background",
                    "surface.background",
                    "elevated_surface.background",
                ],
            ),
        ] {
            let text = k8s_theme_color(appearance, "text");
            let muted = k8s_theme_color(appearance, "text.muted");
            for surface in surfaces {
                let background = k8s_theme_color(appearance, surface);
                assert_contrast(
                    &format!("{appearance} text on {surface}"),
                    text,
                    background,
                    4.5,
                );
                assert_contrast(
                    &format!("{appearance} muted text on {surface}"),
                    muted,
                    background,
                    4.5,
                );
            }
        }
    }

    #[test]
    fn k8s_studio_status_backgrounds_keep_body_text_readable() {
        for appearance in ["light", "dark"] {
            let text = k8s_theme_color(appearance, "text");
            for status in ["success", "warning", "error", "info"] {
                let background = k8s_theme_color(appearance, &format!("{status}.background"));
                assert_contrast(
                    &format!("{appearance} {status} background"),
                    text,
                    background,
                    4.5,
                );
            }
        }
    }

    /// The table paints its rows on `surface.background`, and every row state
    /// composites onto that one base.
    fn table_surface(appearance: &str) -> Hsla {
        theme_hsla(
            include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
            if appearance == "light" {
                "K8s Studio Light"
            } else {
                "K8s Studio Dark"
            },
            "surface.background",
        )
    }

    fn k8s_hsla(appearance: &str, key: &str) -> Hsla {
        theme_hsla(
            include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
            if appearance == "light" {
                "K8s Studio Light"
            } else {
                "K8s Studio Dark"
            },
            key,
        )
    }

    /// The four row washes, built the way the renderer builds them.
    fn k8s_row_washes(appearance: &str) -> [Hsla; 4] {
        let base = table_surface(appearance);
        let text = k8s_hsla(appearance, "text");
        let accent = k8s_hsla(appearance, "text.accent");
        [
            row_wash(base, text.opacity(ROW_STRIPE_ALPHA)),
            row_wash(base, k8s_hsla(appearance, "element.hover")),
            composite_surface(base, accent.opacity(ROW_FOCUS_ALPHA)),
            composite_surface(base, accent.opacity(ROW_SELECTED_ALPHA)),
        ]
    }

    #[test]
    fn k8s_studio_row_composites_meet_wcag_aa() {
        for appearance in ["light", "dark"] {
            let theme_name = if appearance == "light" {
                "K8s Studio Light"
            } else {
                "K8s Studio Dark"
            };
            let text = k8s_hsla(appearance, "text");
            let muted = k8s_hsla(appearance, "text.muted");
            let [stripe, hover, focus, selected] = k8s_row_washes(appearance);
            for (row, wash, cell_text) in [
                ("stripe", stripe, muted),
                ("hover", hover, muted),
                ("selected", selected, text),
                ("focus", focus, text),
            ] {
                assert_hsla_contrast(
                    &format!("{appearance} body on {row} row"),
                    text,
                    wash,
                    TEXT_MIN_CONTRAST,
                );
                assert_hsla_contrast(
                    &format!("{appearance} cell on {row} row"),
                    cell_text,
                    wash,
                    TEXT_MIN_CONTRAST,
                );
                for severity in [
                    Severity::Success,
                    Severity::Warning,
                    Severity::Error,
                    Severity::Info,
                ] {
                    let marker = marker_for_background(
                        theme_marker_hsla(
                            include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                            theme_name,
                            severity,
                        ),
                        wash,
                    );
                    assert_marker_contrast(
                        &format!("{appearance} {severity:?} marker on {row} row"),
                        marker,
                        wash,
                    );
                }
            }
            assert_hsla_contrast(
                &format!("{appearance} muted fallback on selected row"),
                muted,
                selected,
                TEXT_MIN_CONTRAST,
            );
        }
    }

    /// A row state has to be visible against the table it sits on, and the states
    /// have to be tellable apart from each other.
    ///
    /// The old assertions only checked text *on* a row, which is why a 1.018:1
    /// hover, a 1.02:1 multi-select member and a 1.101:1 zebra all passed: the text
    /// on them was perfectly readable, the rows just were not there. A state
    /// nobody can see is not a state.
    #[test]
    fn k8s_studio_row_states_are_visible_and_tellable_apart() {
        for appearance in ["light", "dark"] {
            let base = table_surface(appearance);
            let [stripe, hover, focus, selected] = k8s_row_washes(appearance);
            // The stripe is a state that has to be visible, but it is not one that
            // has to be separable from the others: a hover *replaces* the zebra on
            // the row it lands on, so the two are never on screen together and
            // asking them apart would only forbid the two from both being quiet.
            for (name, wash) in [
                ("stripe", stripe),
                ("hover", hover),
                ("focus", focus),
                ("selected", selected),
            ] {
                assert_hsla_contrast(
                    &format!("{appearance} {name} row against the table"),
                    wash,
                    base,
                    ROW_STATE_MIN_CONTRAST,
                );
            }
            let states = [("hover", hover), ("focus", focus), ("selected", selected)];
            for (index, (left, left_wash)) in states.iter().enumerate() {
                for (right, right_wash) in states.iter().skip(index + 1) {
                    assert_ne!(
                        left_wash, right_wash,
                        "{appearance}: the {left} and {right} rows are the same color"
                    );
                    assert_hsla_contrast(
                        &format!("{appearance} {right} row against the {left} row"),
                        *right_wash,
                        *left_wash,
                        ROW_STATE_MIN_SEPARATION,
                    );
                }
            }
            // A hover is transient and the selection is a command target, so a
            // hover that outshouts the selection is worse than no hover at all.
            assert!(
                contrast_ratio(hover, base) < contrast_ratio(selected, base),
                "{appearance}: a hover at {:.3}:1 outshouts the selection at {:.3}:1",
                contrast_ratio(hover, base),
                contrast_ratio(selected, base),
            );
        }
    }

    /// The row rule has to be the channel that carries row rhythm, in both
    /// appearances.
    ///
    /// The zebra wash and the 1px rule are two ways to do one job, and the
    /// appearances disagreed about which one was doing it: the dark rule was
    /// louder than the stripe and the light rule was quieter, so the same
    /// component read two different ways depending on the theme.
    ///
    /// Lifting the stripe to the row-state floor moved the rule with it, because a
    /// rule painted on a darker stripe has less room: at the previous
    /// `border.variant` the dark rule fell to 1.153:1 on the solved stripe, under
    /// the floor the row rule is held to everywhere else. `border.variant` is a
    /// theme role, so the two move together rather than one being left under its
    /// floor and re-measured later.
    #[test]
    fn the_row_rule_stays_above_the_stripe_it_sits_on() {
        for appearance in ["light", "dark"] {
            let base = table_surface(appearance);
            let [stripe, _, _, _] = k8s_row_washes(appearance);
            let rule = k8s_hsla(appearance, "border.variant");
            let rule_on_stripe = contrast_ratio(rule, stripe);
            let stripe_own = contrast_ratio(stripe, base);
            assert!(
                rule_on_stripe > stripe_own,
                "{appearance}: the row rule sits at {rule_on_stripe:.3}:1 on the stripe while the \
                 stripe itself is {stripe_own:.3}:1, so the rule stops carrying the rhythm"
            );
            assert_hsla_contrast(
                &format!("{appearance} row rule against the table"),
                rule,
                base,
                border::MIN_RULE_CONTRAST,
            );
            assert_hsla_contrast(
                &format!("{appearance} row rule against the stripe"),
                rule,
                stripe,
                border::MIN_RULE_CONTRAST,
            );
        }
    }

    /// Increase Contrast has to make the selection *stronger*, and the text on it
    /// has to keep clearing the raised threshold.
    ///
    /// The wash used to drop from 14% to 5% when the setting was on, so turning
    /// it on quietly made a selected row weaker than a range member, which scaled
    /// it down again on top of that.
    #[test]
    fn increase_contrast_never_weakens_row_selection() {
        // Both alphas are constants, so the ordering is settled at compile time and the rest of
        // the test can get on with the washes the two alphas actually produce.
        const {
            assert!(
                INCREASED_ROW_SELECTED_ALPHA >= ROW_SELECTED_ALPHA,
                "Increase Contrast must not weaken the row selection wash"
            );
        }
        for (appearance, theme_name) in [("light", "K8s Studio Light"), ("dark", "K8s Studio Dark")]
        {
            let mut theme = resolved_theme(
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                theme_name,
            );
            refine_theme(&mut theme);
            let [_, _, _, default_selected] =
                row_washes(theme.colors(), ROW_SELECTED_ALPHA, TEXT_MIN_CONTRAST);
            let default_ratio = contrast_ratio(default_selected, theme.colors().surface_background);
            refine_theme_with_contrast(&mut theme, true);
            let colors = theme.colors();
            let increased = row_washes(
                colors,
                INCREASED_ROW_SELECTED_ALPHA,
                INCREASED_CONTRAST_TEXT_MIN,
            );
            let increased_ratio = contrast_ratio(increased[3], colors.surface_background);
            assert!(
                increased_ratio >= default_ratio,
                "{appearance}: Increase Contrast weakens the selection from {default_ratio:.3}:1 \
                 to {increased_ratio:.3}:1"
            );
            for (name, wash) in [
                ("hover", increased[1]),
                ("focus", increased[2]),
                ("selected", increased[3]),
            ] {
                for foreground in [colors.text, colors.text_muted, colors.text_placeholder] {
                    assert_hsla_contrast(
                        &format!("{appearance} increased {name} row"),
                        foreground,
                        wash,
                        INCREASED_CONTRAST_TEXT_MIN,
                    );
                }
            }
        }
    }

    #[test]
    fn k8s_studio_selection_and_search_roles_stay_distinct_and_readable() {
        for appearance in ["light", "dark"] {
            let theme_name = if appearance == "light" {
                "K8s Studio Light"
            } else {
                "K8s Studio Dark"
            };
            let mut theme = resolved_theme(
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                theme_name,
            );
            refine_theme(&mut theme);
            let colors = theme.colors();
            assert_ne!(
                colors.search_match_background,
                colors.element_selection_background
            );
            assert_ne!(
                colors.search_active_match_background,
                colors.element_selection_background
            );
            assert_ne!(
                colors.search_active_match_background,
                colors.search_match_background
            );
            let editor = colors.editor_background;
            let selection = composite_surface(editor, colors.element_selection_background);
            let search = composite_surface(editor, search_match::resolve_background(colors));
            let active_search =
                composite_surface(editor, search_match::resolve_active_background(colors));
            assert_ne!(selection, search, "{appearance} selection/search");
            assert_ne!(
                selection, active_search,
                "{appearance} selection/active search"
            );
            assert_ne!(search, active_search, "{appearance} search/active search");
            for (name, surface) in [
                ("selection", selection),
                ("search", search),
                ("active search", active_search),
            ] {
                assert_hsla_contrast(
                    &format!("{appearance} text on {name}"),
                    colors.text,
                    surface,
                    TEXT_MIN_CONTRAST,
                );
                assert_hsla_contrast(
                    &format!("{appearance} muted text on {name}"),
                    colors.text_muted,
                    surface,
                    TEXT_MIN_CONTRAST,
                );
            }
        }
    }

    #[test]
    fn increased_contrast_preserves_search_semantics() {
        for appearance in ["light", "dark"] {
            let theme_name = if appearance == "light" {
                "K8s Studio Light"
            } else {
                "K8s Studio Dark"
            };
            let mut theme = resolved_theme(
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                theme_name,
            );
            refine_theme_with_contrast(&mut theme, true);
            let colors = theme.colors();
            let selection = colors.element_selection_background;
            let search = search_match::resolve_background(colors);
            let active_search = search_match::resolve_active_background(colors);
            assert_ne!(selection, search, "{appearance} increased selection/search");
            assert_ne!(
                selection, active_search,
                "{appearance} increased selection/active search"
            );
            assert_ne!(
                search, active_search,
                "{appearance} increased search/active search"
            );
            let editor = colors.editor_background;
            for surface in [
                composite_surface(editor, selection),
                composite_surface(editor, search),
                composite_surface(editor, active_search),
            ] {
                let foreground = text_on_for_mode(surface, colors.text, colors.text_accent, true);
                assert_hsla_contrast(
                    &format!("{appearance} increased semantic text"),
                    foreground,
                    surface,
                    INCREASED_CONTRAST_TEXT_MIN,
                );
            }
        }
    }

    #[test]
    fn row_height_never_clips_data_line_height() {
        assert_eq!(row_height(px(18.)), px(28.));
        assert_eq!(row_height(px(32.)), px(32.));
    }

    #[test]
    fn theme_markers_meet_contrast_on_selected_and_surfaces() {
        let themes = [
            (
                "k8s-light",
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                "K8s Studio Light",
            ),
            (
                "k8s-dark",
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                "K8s Studio Dark",
            ),
            (
                "one-light",
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                "One Light",
            ),
            (
                "one-dark",
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                "One Dark",
            ),
            (
                "gruvbox-light",
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                "Gruvbox Light",
            ),
            (
                "gruvbox-dark",
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                "Gruvbox Dark",
            ),
        ];
        for (label, theme_json, theme_name) in themes {
            for surface in ["surface.background", "element.selected"] {
                let background = theme_hsla(theme_json, theme_name, surface);
                for severity in [
                    Severity::Success,
                    Severity::Warning,
                    Severity::Error,
                    Severity::Info,
                    Severity::Neutral,
                    Severity::Muted,
                ] {
                    let marker = marker_for_background(
                        theme_marker_hsla(theme_json, theme_name, severity),
                        background,
                    );
                    assert_marker_contrast(
                        &format!("{label} {severity:?} marker on {surface}"),
                        marker,
                        background,
                    );
                }
            }
        }
    }

    #[test]
    fn one_light_text_tokens_meet_wcag_aa() {
        assert_contrast("body on panel", LIGHT_TEXT, LIGHT_PANEL, 4.5);
        assert_contrast("body on row", LIGHT_TEXT, LIGHT_ROW, 4.5);
        assert_contrast("body on selected row", LIGHT_TEXT, LIGHT_SELECTED_ROW, 4.5);
        assert_contrast("body on element_selected", LIGHT_TEXT, LIGHT_SELECTED, 4.5);
        assert_contrast(
            "muted line numbers / log severity",
            LIGHT_MUTED,
            LIGHT_PANEL,
            4.5,
        );
        assert_contrast("muted cells on row", LIGHT_MUTED, LIGHT_ROW, 4.5);
        assert_contrast(
            "filter placeholder on editor",
            LIGHT_MUTED,
            LIGHT_EDITOR,
            4.5,
        );
        assert_contrast(
            "badge text on success bg",
            LIGHT_TEXT,
            LIGHT_SUCCESS_BG,
            4.5,
        );
        assert_contrast(
            "badge text on warning bg",
            LIGHT_TEXT,
            LIGHT_WARNING_BG,
            4.5,
        );
        assert_contrast("badge text on error bg", LIGHT_TEXT, LIGHT_ERROR_BG, 4.5);
        assert_contrast("badge text on info bg", LIGHT_TEXT, LIGHT_INFO_BG, 4.5);
    }

    #[test]
    fn one_dark_text_tokens_meet_wcag_aa() {
        assert_contrast("body on panel", DARK_TEXT, DARK_PANEL, 4.5);
        assert_contrast("body on row", DARK_TEXT, DARK_ROW, 4.5);
        assert_contrast("body on selected row", DARK_TEXT, DARK_SELECTED_ROW, 4.5);
        assert_contrast("body on element_selected", DARK_TEXT, DARK_SELECTED, 4.5);
        assert_contrast(
            "muted line numbers / log severity",
            DARK_MUTED,
            DARK_PANEL,
            4.5,
        );
        assert_contrast("muted cells on row", DARK_MUTED, DARK_ROW, 4.5);
        assert_contrast("filter placeholder on editor", DARK_MUTED, DARK_EDITOR, 4.5);
        assert_contrast("badge text on success bg", DARK_TEXT, DARK_SUCCESS_BG, 4.5);
        assert_contrast("badge text on warning bg", DARK_TEXT, DARK_WARNING_BG, 4.5);
        assert_contrast("badge text on error bg", DARK_TEXT, DARK_ERROR_BG, 4.5);
        assert_contrast("badge text on info bg", DARK_TEXT, DARK_INFO_BG, 4.5);
    }

    /// The five roles the editor paints over its base surface.
    ///
    /// They only read as five different things while each one stays distinct and
    /// each one still hosts the text tokens, so both properties are checked for
    /// every appearance and both contrast modes.
    #[test]
    fn selection_search_active_line_and_diagnostic_stay_distinct() {
        for theme_name in ["K8s Studio Light", "K8s Studio Dark"] {
            for increased in [false, true] {
                let mut theme = resolved_theme(
                    include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                    theme_name,
                );
                refine_theme_with_contrast(&mut theme, increased);
                let colors = theme.colors();
                let status = theme.status();
                let editor = colors.editor_background;
                let label = format!("{theme_name} increased={increased}");
                let text_minimum = if increased {
                    INCREASED_CONTRAST_TEXT_MIN
                } else {
                    TEXT_MIN_CONTRAST
                };
                let roles = [
                    ("selection", colors.element_selection_background),
                    ("search match", search_match::resolve_background(colors)),
                    (
                        "active search",
                        search_match::resolve_active_background(colors),
                    ),
                    ("active line", colors.editor_active_line_background),
                    (
                        "diagnostic",
                        status
                            .error_background
                            .opacity(editor_wash::DIAGNOSTIC_ALPHA),
                    ),
                ];
                for left in 0..roles.len() {
                    for right in left + 1..roles.len() {
                        assert_ne!(
                            roles[left].1, roles[right].1,
                            "{label} {} collapsed onto {}",
                            roles[left].0, roles[right].0
                        );
                    }
                }
                for (name, wash) in roles {
                    let composite = composite_surface(editor, wash);
                    let foreground =
                        text_on_for_mode(composite, colors.text, colors.text_accent, increased);
                    assert_hsla_contrast(
                        &format!("{label} {name} text"),
                        foreground,
                        composite,
                        text_minimum,
                    );
                }
            }
        }
    }

    /// The tab and editor-wash roles the contract registers hold in Light, Dark,
    /// and every Zed theme.
    ///
    /// The registry is the product's map from a semantic role to a theme token. A
    /// registered role an appearance cannot fill in is a role the view code ends
    /// up painting with whatever the theme happened to leave behind, so the
    /// mapping is checked where the view code reads it: the active tab stays off
    /// the bar behind it, a tab label stays readable, and the cursor line stays
    /// off the selection, the search roles, and the diagnostic.
    #[test]
    fn registered_tab_and_editor_wash_roles_hold_in_every_supported_theme() {
        for (theme_json, theme_names) in [
            (
                include_str!("../../k8s-app/assets/themes/k8s-studio.json"),
                &["K8s Studio Light", "K8s Studio Dark"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/ayu/ayu.json"),
                &["Ayu Dark", "Ayu Light", "Ayu Mirage"][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json"),
                &[
                    "Gruvbox Dark",
                    "Gruvbox Dark Hard",
                    "Gruvbox Dark Soft",
                    "Gruvbox Light",
                    "Gruvbox Light Hard",
                    "Gruvbox Light Soft",
                ][..],
            ),
            (
                include_str!("../../../reference/zed/assets/themes/one/one.json"),
                &["One Light", "One Dark"][..],
            ),
        ] {
            for theme_name in theme_names {
                for increased in [false, true] {
                    let mut theme = resolved_theme(theme_json, theme_name);
                    refine_theme_with_contrast(&mut theme, increased);
                    let colors = theme.colors();
                    let status = theme.status();
                    let label = format!("{theme_name} increased={increased}");
                    let text_minimum = if increased {
                        INCREASED_CONTRAST_TEXT_MIN
                    } else {
                        TEXT_MIN_CONTRAST
                    };
                    let tab_bar = colors.tab_bar_background;
                    let tab_active = colors.tab_active_background;
                    assert_ne!(
                        tab_active, tab_bar,
                        "{label} active tab collapsed onto the bar"
                    );
                    for (name, tab_surface) in [("tab bar", tab_bar), ("active tab", tab_active)] {
                        assert_hsla_contrast(
                            &format!("{label} text on {name}"),
                            colors.text,
                            tab_surface,
                            text_minimum,
                        );
                    }
                    let editor = colors.editor_background;
                    let roles = [
                        (
                            "selection",
                            composite_surface(editor, colors.element_selection_background),
                        ),
                        (
                            "search match",
                            composite_surface(editor, search_match::resolve_background(colors)),
                        ),
                        (
                            "active search",
                            composite_surface(
                                editor,
                                search_match::resolve_active_background(colors),
                            ),
                        ),
                        (
                            "active line",
                            composite_surface(editor, editor_wash::active_line_overlay_for(colors)),
                        ),
                        (
                            "diagnostic",
                            composite_surface(
                                editor,
                                editor_wash::diagnostic_overlay_for(colors, status),
                            ),
                        ),
                    ];
                    for left in 0..roles.len() {
                        for right in left + 1..roles.len() {
                            assert_ne!(
                                roles[left].1, roles[right].1,
                                "{label} {} collapsed onto {}",
                                roles[left].0, roles[right].0
                            );
                        }
                    }
                }
            }
        }
    }

    /// The chart crosshair is a graphic, so it follows the graphic threshold in
    /// all three modes instead of being dimmed below it.
    #[gpui::test]
    fn chart_crosshair_meets_the_graphic_threshold(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::settings::set_test_increase_contrast(cx, false);
            let canvas = surface::canvas(cx);
            let crosshair = chart::crosshair(cx);
            assert_eq!(crosshair.a, 1.0, "an alpha cannot be checked");
            assert!(
                ui::utils::calculate_contrast_ratio(crosshair, canvas) >= MARKER_MIN_CONTRAST,
                "crosshair is below the graphic threshold on the canvas"
            );
        });
        cx.update(|cx| {
            crate::settings::set_test_increase_contrast(cx, true);
            let canvas = surface::canvas(cx);
            let crosshair = chart::crosshair(cx);
            assert_eq!(crosshair.a, 1.0, "an alpha cannot be checked");
            assert!(
                ui::utils::calculate_contrast_ratio(crosshair, canvas)
                    >= INCREASED_CONTRAST_GRAPHIC_MIN,
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
    #[test]
    fn the_confidence_channel_borrows_no_other_role() {
        for appearance in ["light", "dark"] {
            let unknown = k8s_hsla(appearance, "confidence.unknown");
            let stale = k8s_hsla(appearance, "confidence.stale");
            assert_ne!(
                unknown, stale,
                "{appearance}: the two confidence states have to be tellable apart"
            );
            for borrowed in [
                "text.muted",
                "text.placeholder",
                "text.disabled",
                "icon.disabled",
                "icon.placeholder",
            ] {
                assert_ne!(
                    unknown,
                    k8s_hsla(appearance, borrowed),
                    "{appearance}: confidence.unknown is {borrowed}"
                );
                assert_ne!(
                    stale,
                    k8s_hsla(appearance, borrowed),
                    "{appearance}: confidence.stale is {borrowed}"
                );
            }
        }
    }

    /// A confidence mark has to clear the body-text floor on every surface it can
    /// land on, in both appearances.
    ///
    /// The seed hue is written for one appearance and the shipped path solves it
    /// against the same core surfaces the status foregrounds use, so this checks
    /// the product of the two. The light `Unknown` seed misses the floor on the
    /// light panel and the light canvas; solving is what closes that.
    #[test]
    fn the_confidence_channel_meets_the_text_threshold() {
        for appearance in ["light", "dark"] {
            let surfaces = [
                k8s_hsla(appearance, "background"),
                k8s_hsla(appearance, "panel.background"),
                k8s_hsla(appearance, "surface.background"),
                k8s_hsla(appearance, "elevated_surface.background"),
                k8s_hsla(appearance, "editor.background"),
                k8s_hsla(appearance, "element.selected"),
            ];
            for role in ["confidence.unknown", "confidence.stale"] {
                let solved =
                    adjusted_color_on_all(k8s_hsla(appearance, role), &surfaces, TEXT_MIN_CONTRAST);
                for surface in surfaces {
                    assert_hsla_contrast(
                        &format!("{appearance} {role} on {surface:?}"),
                        solved,
                        surface,
                        TEXT_MIN_CONTRAST,
                    );
                }
            }
        }
    }

    /// The confidence mark the app actually paints clears the threshold, and it
    /// is not the health hue beside it.
    #[gpui::test]
    fn the_live_confidence_foreground_meets_the_text_threshold(cx: &mut gpui::TestAppContext) {
        use super::Confidence;
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::settings::set_test_increase_contrast(cx, false);
            for state in [Confidence::Known, Confidence::Stale, Confidence::Unknown] {
                let ink = confidence::foreground(state, cx);
                for background in [
                    surface::canvas(cx),
                    surface::panel(cx),
                    surface::input(cx),
                    surface::raised(cx),
                ] {
                    assert!(
                        ui::utils::calculate_contrast_ratio(ink, background) >= TEXT_MIN_CONTRAST,
                        "{state:?} confidence is below the body-text floor on {background:?}"
                    );
                }
            }
        });
    }

    /// The health channel's own ink, so a stale answer and a healthy object never
    /// resolve to the same colour.
    #[gpui::test]
    fn confidence_and_health_resolve_to_different_colors(cx: &mut gpui::TestAppContext) {
        use super::Confidence;
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::settings::set_test_increase_contrast(cx, false);
            for (state, severity) in [
                (Confidence::Unknown, Severity::Error),
                (Confidence::Stale, Severity::Warning),
            ] {
                let ink = confidence::foreground(state, cx);
                let health = severity.marker_on(cx, surface::canvas(cx));
                assert_ne!(
                    ink, health,
                    "{state:?} and {severity:?} resolve to the same color, so a resource the \
                     app could not read reads as one that is unwell"
                );
            }
        });
    }

    /// The secondary text roles are four different values, ordered.
    ///
    /// They used to share one threshold over one surface list, and
    /// `adjusted_color_on_all` walks lightness in 1/256 steps, so whoever arrived
    /// first won: the light `text.disabled` and `text.placeholder` came out
    /// bit-identical and the dark three sat inside 0.43 L\* of each other. A
    /// disabled control is exempt from the body-text floor, so it is solved
    /// against a floor of its own with a ceiling taken from the role above it,
    /// and that is what keeps it the quietest of the four.
    #[test]
    fn the_secondary_text_roles_stay_a_ladder() {
        for theme_name in [
            "K8s Studio Light",
            "K8s Studio Dark",
            "One Light",
            "One Dark",
            "Ayu Light",
            "Ayu Dark",
            "Gruvbox Light",
            "Gruvbox Dark",
        ] {
            let theme_json = if theme_name.starts_with("K8s") {
                include_str!("../../k8s-app/assets/themes/k8s-studio.json")
            } else if theme_name.starts_with("Ayu") {
                include_str!("../../../reference/zed/assets/themes/ayu/ayu.json")
            } else if theme_name.starts_with("Gruvbox") {
                include_str!("../../../reference/zed/assets/themes/gruvbox/gruvbox.json")
            } else {
                include_str!("../../../reference/zed/assets/themes/one/one.json")
            };
            let mut theme = resolved_theme(theme_json, theme_name);
            refine_theme(&mut theme);
            let colors = theme.colors();
            let surfaces = core_surfaces(colors);
            let product = theme_name.starts_with("K8s");
            assert_ne!(
                colors.text_disabled, colors.text_placeholder,
                "{theme_name}: text.disabled collapsed onto text.placeholder"
            );
            for surface in &surfaces {
                let disabled = contrast_ratio(colors.text_disabled, *surface);
                assert!(
                    disabled >= DISABLED_TEXT_MIN_CONTRAST,
                    "{theme_name}: text.disabled is at {disabled:.3} on {surface:?}, below its \
                     own floor"
                );
                if !product {
                    continue;
                }
                let muted = contrast_ratio(colors.text_muted, *surface);
                let placeholder = contrast_ratio(colors.text_placeholder, *surface);
                assert!(
                    disabled < muted,
                    "{theme_name}: text.disabled ({disabled:.3}) is not quieter than text_muted \
                     ({muted:.3}) on {surface:?}"
                );
                assert!(
                    disabled < placeholder,
                    "{theme_name}: text.disabled ({disabled:.3}) is not quieter than \
                     text.placeholder ({placeholder:.3}) on {surface:?}"
                );
            }
            assert!(
                worst_contrast(colors.text_disabled, &surfaces)
                    < worst_contrast(colors.text_muted, &surfaces),
                "{theme_name}: text.disabled is not quieter than text_muted where they bind"
            );
        }
    }

    /// A disabled control has to look like one in both appearances.
    ///
    /// `element.disabled` used to sit at 1.000:1 against `element.background` in
    /// light and 1.010:1 in dark, so a disabled button was a normal button with
    /// slightly grey text: nothing about the surface said the control was
    /// unavailable.
    #[test]
    fn a_disabled_control_has_a_step_of_its_own() {
        for appearance in ["light", "dark"] {
            let step = contrast(
                k8s_theme_color(appearance, "element.disabled"),
                k8s_theme_color(appearance, "element.background"),
            );
            assert!(
                step > 1.05,
                "{appearance}: element.disabled is {step:.3}:1 against element.background"
            );
        }
    }

    #[test]
    fn counts_carry_their_noun_and_their_separator() {
        assert_eq!(format::count(0), "0");
        assert_eq!(format::count(999), "999");
        assert_eq!(format::count(1_000), "1,000");
        assert_eq!(format::count(10_101), "10,101");
        assert_eq!(
            format::count_with_noun(0, "session", "sessions"),
            "0 sessions"
        );
        assert_eq!(
            format::count_with_noun(1, "session", "sessions"),
            "1 session"
        );
        assert_eq!(
            format::count_with_noun(1_234, "session", "sessions"),
            "1,234 sessions"
        );
    }

    /// The problems filter's two states are two shapes, not one shape in two
    /// colors, and neither of them is a health glyph.
    #[test]
    fn the_problems_filter_has_two_shapes() {
        use super::Confidence;
        let off = problems_filter_icon(false);
        let on = problems_filter_icon(true);
        assert_ne!(off, on, "the filter's two states are the same glyph");
        for glyph in [off, on] {
            for severity in [
                Severity::Success,
                Severity::Warning,
                Severity::Error,
                Severity::Info,
                Severity::Neutral,
            ] {
                assert_ne!(
                    glyph,
                    health_icon(severity),
                    "the filter reuses a health glyph, so a filter state reads as a verdict"
                );
            }
            for state in [Confidence::Stale, Confidence::Unknown] {
                assert_ne!(
                    glyph,
                    confidence::icon(state),
                    "the filter reuses a confidence glyph, so the two channels collapse"
                );
            }
        }
    }

    /// A kind's glyph has to name that kind.
    ///
    /// The sidebar's `Overview` row, a `Nodes` row and a `Cluster` row all came
    /// out of this function, and `Overview` was hardcoded to the same glyph as
    /// `Nodes`, so the sidebar's icon column encoded nothing.
    #[test]
    fn a_kind_glyph_does_not_name_another_kind() {
        assert_eq!(kind_icon("Node"), kind_icon("Nodes"));
        assert_ne!(kind_icon("Nodes"), kind_icon("Namespace"));
        assert_ne!(kind_icon("Nodes"), kind_icon("Cluster"));
        assert_ne!(kind_icon("Namespaces"), kind_icon("Group"));
        assert_ne!(kind_icon("Namespace"), kind_icon("Group"));
        // `Group` is the API-group container in the sidebar, which is the only
        // thing that is actually a folder.
        assert_eq!(kind_icon("Group"), ui::IconName::Folder);
    }

    /// A pod the cluster will not answer for is not a pod that is waiting.
    ///
    /// `"Unknown"` shared a branch with `"Pending"`, so both rows got the same
    /// warning triangle and the same amber, pixel for pixel apart from the word.
    /// The one channel that could have said "nobody has a verdict" was the
    /// hollow `?` confidence marker, which only paints at full opacity on a
    /// selected or hovered row — so on a quiet table the two states were
    /// identical. `DESIGN.md` §4: 读不到 ≠ 健康.
    ///
    /// The test that used to cover `status_severity` skipped exactly this label,
    /// and the split was made at the call site because this function belonged to
    /// someone else, which left the token and the app disagreeing. It belongs
    /// here, so it is pinned here.
    #[test]
    fn an_unanswered_pod_is_not_painted_as_a_waiting_one() {
        assert_eq!(pod_severity("Running"), Severity::Success);
        assert_eq!(pod_severity("Pending"), Severity::Warning);
        assert_eq!(pod_severity("CrashLoopBackOff"), Severity::Error);
        assert_eq!(pod_severity(""), Severity::Muted);
        assert_ne!(
            pod_severity("Unknown"),
            pod_severity("Pending"),
            "a pod the kubelet lost track of must not be painted as queued"
        );
        // The split is only worth anything if it reaches the channel a reader
        // actually sees, so it is checked past the severity itself.
        assert_eq!(
            health_icon(pod_severity("Unknown")),
            health_icon(Severity::Neutral)
        );
        assert_eq!(health_label(pod_severity("Unknown")), "No verdict");
    }

    /// The window floor lives in one place.
    #[test]
    fn the_window_floor_is_declared_once() {
        assert_eq!(size::WINDOW_MIN, (960., 640.));
        assert!(
            size::WINDOW_MIN.0 >= f32::from(size::CENTER_MIN) + f32::from(size::SIDEBAR_MIN),
            "the floor is too narrow to hold a sidebar and a centre pane"
        );
        assert!(
            size::WINDOW_MIN.1
                >= f32::from(size::TAB_BAR)
                    + f32::from(size::DOCK_MIN)
                    + f32::from(size::STATUS_BAR),
            "the floor is too short to hold the tab bar, the Dock and the status bar"
        );
    }

    /// The data type role has one definition.
    ///
    /// `design::text::DATA` and `PRODUCT_DATA_FONT_SIZE` were two definitions of
    /// the same number in two units, one of them dimensionless, and the settings
    /// copy won: the user-facing "Data font size" then enlarged the resource
    /// table without touching the ten other data surfaces or the column widths,
    /// which are computed from the token. The settings pair is the default the
    /// user setting may still override, so the two have to agree on the way in.
    #[test]
    fn the_data_type_role_is_declared_once() {
        assert_eq!(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            f32::from(super::text::DATA)
        );
        assert_eq!(
            crate::settings::PRODUCT_DATA_LINE_HEIGHT,
            super::text::DATA_LINE_HEIGHT / super::text::DATA
        );
    }

    /// Motion has two durations, and they are the two the app spends.
    ///
    /// The table used to advertise 50/150/300ms for a UI with no transitions in
    /// it at all, while the two durations it did use were undeclared literals in
    /// two other files.
    #[test]
    fn motion_declares_only_the_durations_the_app_spends() {
        assert_eq!(
            super::motion::LOADING,
            std::time::Duration::from_millis(1_200)
        );
        assert_eq!(super::motion::CARET, std::time::Duration::from_millis(900));
        assert!(super::motion::LOADING > super::motion::CARET);
    }

    /// A structural rule has a floor, and the floor is enforced.
    ///
    /// The light row rule sat at 1.02:1 on a light stripe, which is less than the
    /// rounding of the composite it was drawn on. The constant that used to bound
    /// rules from above had no readers at all, and every 1px line drawn with the
    /// control-boundary role sat far above it, so it promised a guarantee nothing
    /// maintained; it is gone, and what is left is a floor something checks.
    #[test]
    fn structural_rules_clear_the_floor_in_both_appearances() {
        for appearance in ["light", "dark"] {
            assert_hsla_contrast(
                &format!("{appearance} border.variant against the table"),
                k8s_hsla(appearance, "border.variant"),
                table_surface(appearance),
                border::MIN_RULE_CONTRAST,
            );
        }
    }

    /// `DESIGN.md` §4 makes `health_icon` and `confidence::icon` the app's only
    /// status vocabularies, and four components broke it: `Error` and `Warning`
    /// drew the same shape, `Success` and `Muted` the same colour. A rule nobody
    /// checks does not survive the next refactor, so this reads the crate's own
    /// sources instead of trusting review.
    ///
    /// Every one of those four was a `match` whose scrutinee is a severity, so
    /// that is what is looked for. The check cannot tell a private vocabulary from
    /// an ordinary `match` on a severity, so it over-approximates on purpose and
    /// every accepted site is named in `ACCEPTED_STATUS_MATCHES` with the reason
    /// it carries no meaning. A site that is not on that list fails here, with its
    /// `file:line`.
    #[test]
    fn no_component_keeps_a_private_status_vocabulary() {
        let mut offenders = Vec::new();
        for (path, source) in crate_sources() {
            if path == "design.rs" {
                continue;
            }
            for (line, scrutinee) in status_scrutinees(&source) {
                let accepted = ACCEPTED_STATUS_MATCHES
                    .iter()
                    .any(|(file, expression, _)| *file == path && *expression == scrutinee);
                if !accepted {
                    offenders.push(format!("  {path}:{line}  match {scrutinee}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "DESIGN.md §4: design::health_icon and design::confidence::icon are the app's only \
             status vocabularies, and a component may not keep a private map. Offenders:\n{}\n\
             Send the shape and the word through design, or add the site to \
             ACCEPTED_STATUS_MATCHES with the reason it carries no meaning.",
            offenders.join("\n"),
        );
    }

    /// The `match`es over a severity this crate already has, and why each one is
    /// not a status vocabulary.
    ///
    /// Keyed on the file and the scrutinee rather than a line number, because the
    /// line moves every time somebody edits the file and a stale number is a lie
    /// the guard would happily keep repeating. If a scrutinee is renamed the entry
    /// stops matching and the site has to be re-justified, which is the point.
    const ACCEPTED_STATUS_MATCHES: &[(&str, &str, &str)] = &[
        (
            "panels/logs.rs",
            "log_severity(token)",
            "reads a severity out of a log line; the arms are a parse, not a vocabulary",
        ),
        (
            "shell/panels.rs",
            "toast.severity",
            "picks an ARIA live-region role, which the shape vocabulary does not carry",
        ),
        (
            "shell/status_bar.rs",
            "severity",
            "ranks notifications by loudness to sort them; no shape, word or colour is chosen",
        ),
    ];

    /// Every `.rs` file under `src/`, as `(path relative to src/, contents)`.
    fn crate_sources() -> Vec<(String, String)> {
        fn walk(root: &std::path::Path, prefix: &str, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(root) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, &format!("{prefix}{name}/"), out);
                } else if name.ends_with(".rs")
                    && let Ok(source) = std::fs::read_to_string(&path)
                {
                    out.push((format!("{prefix}{name}"), source));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            "",
            &mut out,
        );
        out.sort_by(|left, right| left.0.cmp(&right.0));
        out
    }

    /// `source` with comments and literals blanked out, byte for byte.
    ///
    /// The scan looks for the word `match`, and most of the mentions of a severity
    /// in this crate are inside a test's failure message. Blanking rather than
    /// deleting keeps every offset, so the `file:line` the failure prints is the
    /// real one.
    fn code_only(source: &str) -> Vec<u8> {
        let bytes = source.as_bytes();
        let mut out = bytes.to_vec();
        let mut blank = |from: usize, to: usize| {
            for byte in &mut out[from..to] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        };
        let mut index = 0;
        while index < bytes.len() {
            let rest = &bytes[index..];
            let hashes = if rest[0] == b'r' {
                rest[1..].iter().take_while(|byte| **byte == b'#').count()
            } else {
                0
            };
            // `r#"…"#` is a raw string; `r#type` is a raw identifier and is not.
            let opens_raw = rest.get(hashes) == Some(&b'"') && (hashes > 0 || rest[0] == b'r');
            if rest.starts_with(b"//") {
                let end = rest
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| index + offset);
                blank(index, end);
                index = end;
            } else if rest.starts_with(b"/*") {
                let mut cursor = index + 2;
                let mut depth = 1;
                while cursor < bytes.len() && depth > 0 {
                    if bytes[cursor..].starts_with(b"/*") {
                        depth += 1;
                        cursor += 2;
                    } else if bytes[cursor..].starts_with(b"*/") {
                        depth -= 1;
                        cursor += 2;
                    } else {
                        cursor += 1;
                    }
                }
                let end = cursor.min(bytes.len());
                blank(index, end);
                index = end;
            } else if opens_raw {
                let terminator: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                let mut cursor = index + hashes + 1;
                while cursor < bytes.len() && !bytes[cursor..].starts_with(&terminator) {
                    cursor += 1;
                }
                let end = (cursor + terminator.len()).min(bytes.len());
                blank(index, end);
                index = end;
            } else if rest[0] == b'"' {
                let mut cursor = index + 1;
                while cursor < bytes.len() {
                    match bytes[cursor] {
                        b'\\' => cursor += 2,
                        b'"' => {
                            cursor += 1;
                            break;
                        }
                        _ => cursor += 1,
                    }
                }
                let end = cursor.min(bytes.len());
                blank(index, end);
                index = end;
            } else if let Some(len) = char_literal_len(rest) {
                blank(index + 1, index + len - 1);
                index += len;
            } else {
                index += 1;
            }
        }
        out
    }

    /// The length of a `'x'` literal at the front of `bytes`, if that is what it is.
    ///
    /// A lifetime is the case that matters: `'a` must not be mistaken for a
    /// character, or every `&'a str` in the crate swallows the rest of the file.
    fn char_literal_len(bytes: &[u8]) -> Option<usize> {
        if bytes.first() != Some(&b'\'') {
            return None;
        }
        let body = bytes.get(1..)?;
        let len = match body.first()? {
            b'\\' => 2,
            byte if *byte != b'\'' && *byte != b'\\' => 1,
            _ => return None,
        };
        (body.get(len) == Some(&b'\'')).then_some(len + 2)
    }

    /// Every `match` in `source` whose scrutinee names a severity or a confidence,
    /// as `(line, scrutinee with its whitespace collapsed)`.
    fn status_scrutinees(source: &str) -> Vec<(usize, String)> {
        let code = code_only(source);
        let keyword = b"match";
        let is_word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        let mut found = Vec::new();
        let mut index = 0;
        while index + keyword.len() <= code.len() {
            let Some(offset) = code[index..]
                .windows(keyword.len())
                .position(|window| window == keyword)
            else {
                break;
            };
            let at = index + offset;
            let start = at + keyword.len();
            let boundary = (at == 0 || !is_word_byte(code[at - 1]))
                && !code.get(start).is_some_and(|byte| is_word_byte(*byte));
            index = if boundary { start } else { at + 1 };
            if !boundary {
                continue;
            }
            // The scrutinee runs to the brace that opens the arms. Brackets are
            // balanced so a closure inside it does not end the scan early.
            let mut depth = 0i32;
            let mut cursor = start;
            while cursor < code.len() {
                let byte = code[cursor];
                if depth == 0 && (byte == b'{' || byte == b';') {
                    break;
                }
                if matches!(byte, b'(' | b'[' | b'{') {
                    depth += 1;
                } else if matches!(byte, b')' | b']' | b'}') {
                    depth -= 1;
                }
                cursor += 1;
            }
            let end = cursor.min(code.len());
            let scrutinee = String::from_utf8_lossy(&code[start..end])
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if ["Severity", "severity", "Confidence", "confidence"]
                .iter()
                .any(|needle| scrutinee.contains(needle))
            {
                found.push((1 + source[..at].matches('\n').count(), scrutinee));
            }
        }
        found
    }
}
