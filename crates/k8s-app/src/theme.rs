//! The product theme file, projected onto gpui-kit's component roles.
//!
//! Two theme systems read one file, and that is deliberate.
//!
//! * [`k8s_ui::design`] parses `k8s-studio.json` into the roles the *app* draws:
//!   canvas and panel surfaces, status and severity, chart series, syntax, and
//!   the contrast refinement the `increaseContrast` setting drives.
//! * gpui-kit's [`Theme`] is shadcn-shaped and knows none of those names, so
//!   this module projects the same colours onto gpui-kit's roles and registers
//!   the two product themes in its [`ThemeRegistry`].
//!
//! The split is the ownership boundary the coding guides ask for: gpui-kit owns
//! component appearance, `design.rs` owns the app's semantic roles, and neither
//! reads the other's state. A colour added to the theme file reaches both halves
//! — gpui-kit through [`install`], the app through `design` — but only the roles
//! each one names.
//!
//! The two systems are kept in step by always changing them together in
//! [`set_mode`] and [`select`], so a window can never read an app role from one
//! appearance and a component role from the other. One role has no token to go
//! through and is projected directly: `project_active_thumb`.

use std::sync::LazyLock;

use gpui_kit::component::theme::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::{App, Hsla, Rgba, Window};
use k8s_ui::design;
use serde_json::{Map, Value, json};

/// Dark product theme, and the name it is registered under.
pub use k8s_ui::settings::PRODUCT_THEME_DARK;
/// Light product theme, and the name it is registered under.
pub use k8s_ui::settings::PRODUCT_THEME_LIGHT;

/// Stamps the two ink roles that the projection does not deliver.
///
/// **Measured, not assumed.** A probe after `apply_config` reads, in the shipped
/// dark appearance: `muted_foreground` = `l 0.639` (this product's `text.muted`,
/// `#9AA0A6`) but `foreground` = `l 0.98` and `secondary_foreground` = `l 0.98` —
/// both gpui-kit's own `#FAFAFA` default, against this product's `text`
/// (`#E8EAED`, `l 0.915`). The key spellings are not the cause: every one of the
/// 63 entries in `ROLE_SOURCES` matches a `#[serde(rename)]` in gpui-kit's
/// schema, and `muted.foreground` arrives on the very same mechanism, so the loss
/// is downstream of the colours map.
///
/// It matters because `Button::Ghost` paints from `secondary_foreground`, so every
/// `.ghost()` control in the app — 56 sites across 13 files — rendered in a colour
/// this product never chose, and one *brighter* than its own `fg_primary`. A ghost
/// icon in a toolbar that outshines the text beside it is the defect; the cause
/// living upstream of `Button` is only why it is fixed here.
///
/// Two roles, stamped from the product's own `fg.primary`, after the projection
/// and never before: everything downstream reads them, and anything that already
/// states its own ink explicitly is untouched by this.
fn pin_foreground_roles(cx: &mut App) {
    let ink = design::role::fg_primary(cx);
    // The row rule. `table.row.border` is projected and the projection is emitted,
    // but gpui-component reads it with `apply_color!(table_row_border, fallback =
    // self.border)` and the fallback wins - which is how a design that deleted row
    // dividers shipped a 1px rule under EVERY row: measured at peak (28,29,30)
    // against a (17,18,22) row, so the rule is a real edge at every boundary and
    // the row grid reads as a stack of boxes. Stated here for the same reason as
    // the two inks above: the value is right and something upstream of `Table`
    // replaces it.
    let no_rule = design::role::fg_primary(cx).opacity(0.);
    let theme = Theme::global_mut(cx);
    theme.foreground = ink;
    theme.secondary_foreground = ink;
    theme.table_row_border = no_rule;
}

/// gpui-kit colour role -> the product theme's key for the same role.
///
/// Only roles the product file actually names are listed, and a key missing from
/// the file is skipped rather than defaulted, so a role the product does not
/// speak about keeps gpui-kit's own value instead of inheriting an unrelated one.
/// Unlisted gpui-kit roles fall back to its light or dark defaults, which is the
/// right answer for roles that are component geometry rather than product meaning.
/// # `secondary.foreground` projects, and the screen does not change
///
/// A magnified capture of the title bar measured the six ghost icon buttons at
/// `#FAFAFA`, which is gpui-kit's OWN default — brighter than this product's
/// `fg.primary` (`#E8EAED` dark, `#101114` light). A row of controls drawn in a
/// colour the product does not have is the one kind of ink that is wrong in both
/// appearances rather than one of them, and it sits on 56 `.ghost()` call sites
/// across 13 files.
///
/// **The obvious explanation is wrong, and it was checked.** The key spelling was
/// the first suspect: gpui-kit's `ThemeSchema` declares `secondary_foreground` as a
/// bare field, and only the roles that genuinely want dots carry an explicit
/// `#[serde(rename = "a.b.c")]`. This table writes `secondary.foreground`, which
/// reads like it should have been `secondary_foreground`. Comparing every key in
/// `ROLE_SOURCES` against every `#[serde(rename)]` in `schema.rs` finds **none of
/// the 63 wrong** — `secondary.foreground` is a real rename and it is being
/// emitted under a name gpui-kit reads.
///
/// So the projection is fine and the loss is downstream of it, and the remaining
/// candidates are:
///   - the `Button` reads a `Theme` this module does not install. gpui-kit has two
///     (`component::theme::Theme`, which this projects into, and `base::Theme`,
///     which `project_active_thumb` reaches into separately for the scrollbar).
///     A ghost button reading the BASE theme's default would render exactly what
///     was measured, and it would be unaffected by anything this module does.
///   - `apply_config` fills the pair and something later re-applies the defaults.
///
/// **Not changed here, deliberately.** The one-line experiment that would settle
/// the first candidate is to read `theme().secondary_foreground` at runtime and
/// print it next to the ink a ghost button actually paints; that needs a debug
/// build with a probe, not an edit. Guessing at the projection moves every
/// `secondary.*` role in the product at once, and the cost of being wrong is
/// higher than the cost of the ink sitting one tier bright.
const ROLE_SOURCES: [(&str, &str); 64] = [
    // Canvas, text, and the boundaries between them.
    ("background", "background"),
    ("foreground", "text"),
    ("border", "border"),
    // Not `border`. gpui-kit scales this one: a dark input's fill and stroke are
    // `input.mix_oklab(transparent_black, 0.3)`, so the theme has to ask for
    // 30% more ink than the rule it wants or the field's border lands under
    // `MIN_RULE_CONTRAST` and stops dividing. The theme file names that value
    // `input.border` for what it is; see `retheme.py`.
    ("input.border", "input.border"),
    ("ring", "border.focused"),
    ("caret", "text"),
    ("popover.background", "elevated_surface.background"),
    ("popover.foreground", "text"),
    ("muted.background", "surface.background"),
    ("muted.foreground", "text.muted"),
    // The product's accent. `text.accent` is the one accent role the theme file
    // names, and it is the colour the focus rail and every accent control use.
    ("accent.background", "text.accent"),
    ("accent.foreground", "background"),
    ("primary.background", "text.accent"),
    ("primary.foreground", "background"),
    ("secondary.background", "surface.background"),
    ("secondary.foreground", "text"),
    ("link", "text.accent"),
    ("link.active", "text.accent"),
    ("link.hover", "link_text.hover"),
    ("drag.border", "border.focused"),
    ("drop_target.background", "drop_target.background"),
    // Status. gpui-kit's solid status colour is the product's status foreground.
    ("danger.background", "error"),
    ("danger.foreground", "background"),
    ("success.background", "success"),
    ("success.foreground", "background"),
    ("warning.background", "warning"),
    ("warning.foreground", "background"),
    ("info.background", "info"),
    ("info.foreground", "background"),
    // Lists and tables, the surfaces most of the app's density lives on.
    ("list.background", "panel.background"),
    ("list.hover.background", "element.hover"),
    ("list.active.background", "element.selected"),
    ("list.active.border", "border.focused"),
    ("list.even.background", "surface.background"),
    ("list.head.background", "surface.background"),
    ("table.background", "panel.background"),
    ("table.hover.background", "element.hover"),
    ("table.active.background", "element.selected"),
    ("table.even.background", "surface.background"),
    ("table.head.background", "surface.background"),
    ("table.foot.background", "surface.background"),
    // gpui-component's `TableRow` puts a 1px top border on every row after the
    // first, and `TableHeader` puts a 1px bottom border on the header band, in
    // this one token — there is no option that turns either off.
    // rules out row separators and allows a rule only on inputs, overlays and
    // panel dividers, so a table row is not a place a rule belongs. The header
    // band is separated by its own background, not by a stroke, and the
    // separation it needs is drawn where the app owns the table rather than
    // smuggled in through a token the library owns.
    ("table.row.border", "border.transparent"),
    // Tabs, the sidebar, and the chrome the app paints itself.
    ("tab.background", "tab_bar.background"),
    ("tab_bar.background", "tab_bar.background"),
    ("tab_bar.segmented.background", "tab_bar.background"),
    ("sidebar.background", "panel.background"),
    ("sidebar.foreground", "text"),
    ("sidebar.border", "border"),
    ("title_bar.background", "title_bar.background"),
    ("status_bar.background", "title_bar.background"),
    ("window.border", "border"),
    ("accordion.background", "panel.background"),
    ("group_box.background", "surface.background"),
    ("description_list.label.background", "surface.background"),
    ("skeleton.background", "surface.background"),
    // Controls the app draws on surfaces of its own.
    ("button.background", "surface.background"),
    ("button.foreground", "text"),
    ("button.hover.background", "element.hover"),
    ("button.active.background", "element.active"),
    ("progress.bar.background", "surface.background"),
    ("switch.background", "element.active"),
    // Scrollbars and selection come straight from the product's own roles.
    ("scrollbar.thumb.background", "scrollbar.thumb.background"),
    (
        "scrollbar.thumb.hover.background",
        "scrollbar.thumb.hover_background",
    ),
    ("selection.background", "element.selection_background"),
];

/// Registers the product themes with gpui-kit and makes them its light and dark
/// pair, then lands on the appearance the desktop is asking for. Call once, right
/// after [`gpui_kit::init`].
///
/// A projection failure is logged and leaves gpui-kit's own defaults in place: a
/// theme that did not load is a cosmetic problem, and refusing to start would
/// make it fatal.
///
/// The two `apply_config` calls are what fill the light and dark slots, and the
/// order they happen in decides what a window reads before anything else has
/// said otherwise. Applying light and then dark left **dark** installed, so
/// `install` was not "register the themes" but "register them and force dark",
/// and a caller that only ever called `install` had no appearance switch at all:
/// `main`'s own `install_theme` overwrites the choice from `settings.json` a few
/// lines later, so the difference never showed in the app and it was the harness
/// path that could not reach the light appearance at all.
///
/// So the pair is filled first and the *system* appearance decides which of the
/// two is left on screen — the same choice `ThemeChoice::System` makes, taken
/// here because at this point in startup there is no window and the window
/// appearance is the only appearance there is.
pub fn install(cx: &mut App) {
    if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(&component_theme_set()) {
        eprintln!("k8s-gpui: product theme did not project onto gpui-kit: {error:#}");
        return;
    }
    // `apply_config` fills the mode's pair, so the two calls together leave the
    // product themes as the light and dark themes gpui-kit switches between.
    let (light, dark) = {
        let registry = ThemeRegistry::global(cx);
        (
            registry.themes().get(PRODUCT_THEME_LIGHT).cloned(),
            registry.themes().get(PRODUCT_THEME_DARK).cloned(),
        )
    };
    let theme = Theme::global_mut(cx);
    if let Some(config) = light {
        theme.apply_config(&config);
    }
    if let Some(config) = dark {
        theme.apply_config(&config);
    }
    pin_foreground_roles(cx);
    // No window yet, so this reads the app's own appearance rather than a
    // window's. `set_mode` is the one place an appearance changes, and it is what
    // keeps the two theme systems from disagreeing about it.
    set_mode(cx, ThemeMode::from(cx.window_appearance()), None);
}

/// 's third scrollbar step, which `ROLE_SOURCES` cannot carry.
///
/// The spec sets three steps — rest `>= 3:1`, hover `>= 4.5:1` and higher than
/// rest, active higher than hover — and the theme file names all three. gpui-kit
/// has only two tokens: its base projection paints `thumb_active` with
/// `scrollbar_thumb_hover`, so on screen active and hover were the same colour and
/// the third step was a number in the theme that nothing reached. `ROLE_SOURCES` is
/// a table of *tokens*, and there is no token to point at, so the third step has
/// to be projected onto the one place that draws it.
///
/// Which is here, and why it is surgical: `sync_base` rebuilds the whole scrollbar
/// from gpui-kit's theme on every `Theme::change`, and the only public way to
/// change one style is to take the eleven it just built and replace one. So this
/// reads what gpui-kit built, replaces the single background, and writes the rest
/// back untouched — the alternative, restating all eleven here, is a second source
/// of truth for every other scrollbar property in the product.
///
/// Runs after every place this module changes the theme, because each of those
/// rebuilds the base layer. `k8s-term`'s own scrollbar is painted by `k8s-term`
/// from its palette, so it keeps its own two steps; this is the app's.
fn project_active_thumb(cx: &mut App) {
    let Some(active) = style_of(active_theme_name(cx))
        .get("scrollbar.thumb.active_background")
        .and_then(color)
    else {
        return;
    };
    let base = gpui_kit::base::Theme::global_mut(cx);
    let scrollbar = base.scrollbar.clone();
    let styles = scrollbar.styles().clone();
    base.scrollbar = scrollbar.with_styles(styles.thumb_active(|thumb| thumb.bg(active)));
}

/// The product theme gpui-kit is currently showing, which is the one whose role
/// values the app's own surface has to be projected from.
fn active_theme_name(cx: &App) -> &'static str {
    if Theme::global(cx).is_dark() {
        PRODUCT_THEME_DARK
    } else {
        PRODUCT_THEME_LIGHT
    }
}

/// Switches both theme systems to one appearance.
///
/// This is the only place an appearance changes, so a component role and an app
/// role can never disagree about whether the app is light or dark.
pub fn set_mode(cx: &mut App, mode: ThemeMode, window: Option<&mut Window>) {
    design::set_appearance(cx, appearance(&mode));
    // Both halves of the refinement, in the order the design layers them: the
    // skin first, then the roles derived from it. `roles()` is where 479 call
    // sites in the crate read, so a refinement that stopped at the skin would
    // leave every surface, divider and focus rail the app actually paints exactly
    // where it was.
    design::refine_active_theme(cx);
    Theme::change(mode, window, cx);
    project_active_thumb(cx);
    // `Theme::change` swaps the pair, so the two roles it drops are stamped again
    // here. Skipping this is how the product ends up light-inked in dark and
    // dark-inked in light the first time the reader changes appearance.
    pin_foreground_roles(cx);
}

/// Applies a registered theme by name, in both systems.
///
/// Returns `false` when the registry has no such theme, which is how a settings
/// file naming a theme that is not installed falls back to the system themes.
///
/// The appearance comes from the registry's record of the config, which is the only
/// place it is known. A choice that carries an appearance of its own — `Light` or
/// `Dark` — does not come through here: it names no config, so `apply_config` would
/// replace the mode the reader picked with whatever the registry filed under the
/// nearest name.
pub fn select(cx: &mut App, name: &str) -> bool {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        return false;
    };
    let mode = config.mode;
    Theme::global_mut(cx).apply_config(&config);
    set_mode(cx, mode, None);
    true
}

/// The app's appearance for a gpui-kit mode.
fn appearance(mode: &ThemeMode) -> design::Appearance {
    if mode.is_dark() {
        design::Appearance::Dark
    } else {
        design::Appearance::Light
    }
}

// ---------------------------------------------------------------------------
// Terminal cells
// ---------------------------------------------------------------------------

/// The terminal's own colour slots for one appearance.
///
/// A terminal draws sixteen ANSI cells in normal intensity and eight dim ones, and
/// no component draws them, so they are not shadcn roles and there is nothing to
/// project them onto. The product theme still names every one of them, so they are
/// read from the file here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalAnsi {
    /// The sixteen normal-intensity cells, in the order a terminal numbers them.
    pub normal: [Hsla; 16],
    /// The eight dim cells, in the same order without the bright half.
    pub dim: [Hsla; 8],
}

/// The normal-intensity cell names, in terminal order.
const ANSI_NORMAL: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright_black",
    "bright_red",
    "bright_green",
    "bright_yellow",
    "bright_blue",
    "bright_magenta",
    "bright_cyan",
    "bright_white",
];

/// The dim cell names, in terminal order.
const ANSI_DIM: [&str; 8] = [
    "dim_black",
    "dim_red",
    "dim_green",
    "dim_yellow",
    "dim_blue",
    "dim_magenta",
    "dim_cyan",
    "dim_white",
];

static LIGHT_ANSI: LazyLock<TerminalAnsi> =
    LazyLock::new(|| terminal_ansi_for(PRODUCT_THEME_LIGHT));
static DARK_ANSI: LazyLock<TerminalAnsi> = LazyLock::new(|| terminal_ansi_for(PRODUCT_THEME_DARK));

/// The terminal's cells for the active appearance.
///
/// The cells come from a compiled file and do not change within a process, so each
/// appearance is parsed once however many times a theme change re-reads it.
pub fn terminal_ansi(cx: &App) -> &'static TerminalAnsi {
    match design::appearance(cx) {
        design::Appearance::Light => &LIGHT_ANSI,
        design::Appearance::Dark => &DARK_ANSI,
    }
}

/// Reads one appearance's terminal cells out of the product theme file.
fn terminal_ansi_for(theme_name: &str) -> TerminalAnsi {
    let style = style_of(theme_name);
    // Both product themes name every cell, so the fallback never decides a colour.
    // It exists so one missing key cannot leave a cell with no colour at all.
    let foreground = style
        .get("terminal.foreground")
        .and_then(color)
        .unwrap_or(Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: 1.,
        });
    let cell = |name: &str| style.get(&format!("terminal.ansi.{name}")).and_then(color);
    TerminalAnsi {
        normal: ANSI_NORMAL.map(|name| cell(name).unwrap_or(foreground)),
        dim: ANSI_DIM.map(|name| cell(name).unwrap_or(foreground)),
    }
}

/// The product theme's parsed file, read once.
fn file() -> &'static Value {
    static FILE: LazyLock<Option<Value>> =
        LazyLock::new(|| serde_json::from_str(k8s_ui::design::PRODUCT_THEME_JSON).ok());
    FILE.as_ref().expect("the compiled product theme parses")
}

/// One product theme's style object.
fn style_of(theme_name: &str) -> &'static Map<String, Value> {
    file()["themes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|theme| theme.get("name").and_then(Value::as_str) == Some(theme_name))
        .filter_map(|theme| theme.get("style"))
        .filter_map(Value::as_object)
        .next()
        .expect("the compiled product theme names both appearances")
}

/// A hex colour from the product theme, in the `#RRGGBBAA` form the file uses.
fn color(value: &Value) -> Option<Hsla> {
    Rgba::try_from(value.as_str()?).ok().map(Hsla::from)
}

/// The product theme file, restated as a gpui-kit theme set.
///
/// The product file is the source of truth, so it is read and projected rather
/// than restated: a colour changed there reaches gpui-kit without a second edit
/// here. Only the two product themes are projected, and each keeps the syntax and
/// status styles the app's own tokens are written against, so gpui-kit's editor
/// highlights YAML in the same palette the app's design tokens use.
fn component_theme_set() -> String {
    let themes = file()["themes"]
        .as_array()
        .expect("the compiled product theme has a theme list");
    let mut projected = Vec::new();
    for theme in themes {
        let name = theme["name"]
            .as_str()
            .expect("every product theme is named");
        if name != PRODUCT_THEME_LIGHT && name != PRODUCT_THEME_DARK {
            continue;
        }
        let style = theme["style"]
            .as_object()
            .expect("every product theme has a style");
        let mut config = Map::new();
        config.insert("name".to_owned(), json!(name));
        config.insert("mode".to_owned(), json!(mode_of(name)));
        config.insert("is_default".to_owned(), json!(true));
        config.insert("colors".to_owned(), Value::Object(colors_for(style)));
        // gpui-kit's `HighlightThemeStyle` declares `syntax: SyntaxColors` as a
        // required field (every colour inside it is optional, the key itself is
        // not), and the product theme deliberately carries no `syntax` block —
        // the YAML editor colours matches and errors, and nothing else. Without
        // this synthesized empty block the whole `load_themes_from_str` call
        // rejected the set ("missing field `syntax`"), gpui-kit silently kept
        // its own default light/dark themes, and every component it draws —
        // buttons, selects, badges, pickers — rendered from a palette the
        // product never chose. `{"syntax": {}}` is the smallest valid value.
        let mut highlight = style.clone();
        highlight
            .entry("syntax".to_owned())
            .or_insert_with(|| json!({}));
        config.insert("highlight".to_owned(), Value::Object(highlight));
        // The UI typeface, named here rather than left to gpui-kit's default,
        // which is the *platform* font. That default is the single reason the
        // same table measures three different widths on macOS, Windows and
        // Linux. `k8s_app::fonts::install` has already registered the file by
        // the time this is read.
        config.insert(
            "font".to_owned(),
            json!({ "family": crate::fonts::INTER_FAMILY, "size": f32::from(k8s_ui::design::text::BODY) }),
        );
        projected.push(Value::Object(config));
    }
    assert_eq!(
        projected.len(),
        2,
        "the product theme must name both appearances"
    );
    serde_json::to_string(&json!({
        "name": "K8s Studio",
        "author": "k8s-gpui",
        "themes": projected,
    }))
    .expect("a projected theme set serializes")
}

/// gpui-kit's mode for a product theme name.
fn mode_of(name: &str) -> &'static str {
    if name == PRODUCT_THEME_DARK {
        "dark"
    } else {
        "light"
    }
}

/// The gpui-kit colour roles one product theme supplies.
///
/// A role whose product key is absent is left out, so gpui-kit keeps its own
/// value for it instead of inheriting whatever this projection guessed.
fn colors_for(style: &Map<String, Value>) -> Map<String, Value> {
    let mut colors = Map::new();
    for (role, key) in ROLE_SOURCES {
        if let Some(value) = style.get(key)
            && value.is_string()
        {
            colors.insert(role.to_owned(), value.clone());
        }
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::{PRODUCT_THEME_DARK, PRODUCT_THEME_LIGHT, component_theme_set};
    use crate::fonts::INTER_FAMILY;

    /// The typeface gpui-kit's own components ask for has to be the one
    /// `k8s_app::fonts::install` registered.
    ///
    /// Two files name the UI typeface and nothing joins them: the bundled font
    /// decides the family name, and the projected theme set hands that name to
    /// every gpui-kit component. A name in one and not the other is not a
    /// partial failure — a text system resolves a family by exact match, so a
    /// miss falls the entire component tree back to the platform face while
    /// `install` still reports success and the theme still loads.
    #[test]
    fn the_projected_theme_asks_for_the_registered_family() {
        let set: serde_json::Value =
            serde_json::from_str(&component_theme_set()).expect("the projected set parses");
        for theme in set["themes"].as_array().expect("both appearances") {
            let name = theme["name"].as_str().expect("every theme is named");
            assert!(
                name == PRODUCT_THEME_LIGHT || name == PRODUCT_THEME_DARK,
                "the projection projected {name:?}, which is not a product appearance"
            );
            assert_eq!(
                theme["font"]["family"].as_str(),
                Some(INTER_FAMILY),
                "{name} asks gpui-kit for a family the bundled file does not register under"
            );
        }
    }
}
