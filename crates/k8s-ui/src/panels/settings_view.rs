//! The Settings surface.
//!
//! `UI-SPEC` §15 is not a restyle of this file, it is a different screen. The
//! principle it opens with is the whole design: **a setting that changes how
//! you work belongs in the interface, not on a settings page.** Sixty-eight
//! keyboard rows, an Integrations pane and an About pane are all decisions that
//! had nowhere else to live, and a form that grows without limit is a form
//! nobody reads.
//!
//! So the surface is six categories of at most eight rows each, and everything
//! that is a working method rather than a preference moved out of it:
//!
//! * The command list is a **shortcut reference**. `UI-REDESIGN` L13 item 3 is
//!   "`?` opens a shortcut reference", and its own note is that this is "far
//!   more useful than a 6211-line settings view". It is a destination with its
//!   own search box, not a page of settings.
//! * Integrations became **readonly status rows on Cluster**, because a probe
//!   result is a fact about the environment and not a preference.
//! * About became the **version row on Updates**, because a build number and
//!   the updater that replaces it are one fact.
//!
//! Four rules from §15.2 are the contract this file is built around: an
//! independent window, at most eight rows per page, a search box that reaches
//! every category, and appearance that takes effect the moment it is changed.
//! There is no Apply button anywhere, and the only two actions that confirm are
//! the two in the danger zone at the bottom.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::Focusable;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::theme::ThemeRegistry;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    Action, Anchor, AnyElement, App, ClipboardItem, Context, Div, Entity, FocusHandle, Global,
    Hsla, KeyBinding, KeyDownEvent, Keystroke, KeystrokeEvent, Role, ScrollHandle, SharedString,
    Stateful, Subscription, Task, Toggled, WeakEntity, Window, actions, div, point, px,
};

use crate::design::{self, Severity, space};
use crate::keymap::{self, KeyConflict};
use crate::panels::common::{
    build_menu, empty_state_with_action, label_body, label_panel_title, label_small, spinner,
};
use crate::panels::helm::HelmCapability;
use crate::settings::{
    self, DiskCache, ReduceMotionMode, SettingsStore, ThemeChoice, UserSettings,
};
use crate::shell::commands::{CommandRun, UPDATER_UNAVAILABLE_REASON, demo_commands_with_updater};
use crate::table_view::TextInput;
use crate::update::{UpdateActions, UpdatePhase, UpdateUiState};

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

// `UI-REDESIGN` L13 item 3 is "`?` opens the shortcut reference", and a keymap
// file can only name an action, so the destination needs one of its own. It
// carries no payload because a reference is a place rather than a request, and it
// sits in `k8s_shell` with the other commands a surface routes.
actions!(
    k8s_shell,
    [
        /// Open or close the shortcut reference.
        OpenShortcutReference
    ]
);

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Every width below is a multiple of the 4pt grid or a named component
/// contract, because a settings window is the one surface a reader measures
/// against: the label column and the control column are the only two lines
/// their eye runs down.
///
/// The label column. One width for every row, so no sentence re-wraps when a
/// sibling's control gets wider.
///
/// 288 rather than the 232 it started at, and for a measured reason: the longest
/// second line on the surface is an inert row's reason, and at 232 it wrapped to
/// three lines while the rows beside it were one. A page whose rows are four
/// different heights is not a page of eight rows, it is a page of twenty. The
/// width is the narrowest that holds every second line on one line at
/// `text::LABEL`, so the eight rows of a page are the same height on all six
/// pages — and it is what lets the description sit at the readable 12px rather
/// than at the 11px a caption size would have allowed.
const SETTINGS_LABEL_WIDTH: f32 = 288.;
/// The measure the page stops at.
///
/// Not the window's own width. A settings row is a pair — a name and a control —
/// and a pair only reads as a pair while its two halves stay near each other. At
/// the window's 960px the label column ended at 288 and the control column began
/// at 944, so the 656px between them was a hole rather than a gap, and on a
/// maximised window the same hole sat in the middle of a 1920px screen beside a
/// screen of empty field.
///
/// 640 is the arithmetic of the three numbers this surface already owns rather
/// than a round number chosen for one: two 16px insets, the 288px label column,
/// the 272px control column, and a 32px (`space::XXL`) gutter between them — which
/// is the measure's own last element, not slack. `Proportion and layer hierarchy`
/// asks a region to be sized by what it has to hold, and "do not enlarge a dialog
/// simply to create whitespace" is the same rule: past this the two columns stop
/// being a pair and become two lists with a field between them.
const SETTINGS_CONTENT_MAX_WIDTH: f32 = 640.;

/// The search field's own width: half the measure's inside.
///
/// Derived rather than typed, because a width chosen against the window it
/// happened to be drawn in is the definition of an accidental number — 340 was
/// measured on one screen and carried to every other, and against a 640px measure
/// it is neither half of anything nor the control lane, so the field under the tab
/// strip read as a box sized to no rule at all.
///
/// Half the measure's inside is the relation that says what it is: the widest a
/// filter box can be while the count it states still has room beside it, and half
/// of [`SETTINGS_LABEL_WIDTH`] to the pixel, so the field's trailing edge lands on
/// the middle of the form's own two columns.
///
/// A function and not a constant because `f32::from(Pixels)` is not a `const fn`
/// in gpui 0.6.6 and the relation is the whole point: writing the arithmetic is
/// what stops the next person from replacing it with a number they like.
fn settings_search_width() -> gpui_kit::Pixels {
    px((SETTINGS_CONTENT_MAX_WIDTH - f32::from(SETTINGS_PAGE_INSET) * 2.) / 2.)
}

/// The page's own inset: the distance from the measure's edge to the first
/// column of every band on the surface.
///
/// One number for the title bar, the search field, the category navigation, the
/// rows, the shortcut reference and the danger zone, because a heading, a
/// toolbar, a row and a footer that describe the same level must not each invent
/// a slightly different leading edge — one rendered pixel apart is a defect, not
/// an approximation.
///
/// It is the row grid's own horizontal padding rather than a wrapper's, on
/// purpose: the control column is measured from the trailing edge of the content
/// column, so the inset that puts every control on one spine has to be the
/// padding the rows carry themselves.
const SETTINGS_PAGE_INSET: gpui_kit::Pixels = space::LG;
/// The control column, one width for every control. The widest is a segmented
/// control of three answers or a keycap with two buttons beside it.
const SETTINGS_CONTROL_WIDTH: f32 = 272.;
/// The number and text field's width.
///
/// A field this size holds a four-digit number and its unit with room left, and
/// any wider leaves the control column ragged against the segmented controls
/// beside it.
const SETTINGS_FIELD_WIDTH: f32 = 128.;
/// The value lane: the width every control that shows a value *and the name of
/// that value* occupies.
///
/// Drop-down triggers and a check box with its label beside it. They are the same
/// kind of control — a choice the reader reads and changes — so they share one
/// lane and their frames share a leading edge; and they share the control
/// column's trailing edge, because `setting_control` right-aligns every control
/// in it. A check box that sized itself to its own label put its box 47px right
/// of the drop-down fields on the rows above and below it, which is how one row
/// out of eight came to start somewhere else on the page.
///
/// 200 is the field width the mockup's select is drawn at and the widest control
/// that fits the 272px control column, so the lane is the control column's own
/// arithmetic rather than a third number: 200 + the 72px the column has spare is
/// the room every other kind of control is measured against.
const SETTINGS_VALUE_LANE: f32 = 200.;
/// The narrowest window the shell allows, and the floor every breakpoint on
/// this surface sits at or above.
///
/// One number for the row grid and the window, because the row grid's own
/// arithmetic lands on it: the 288px label column, the 272px control column,
/// the 24px gutter and the 32px of page inset. Below it the two-column row
/// stops fitting and the rows stack.
const SETTINGS_MIN_WINDOW_WIDTH: f32 = 960.;

/// The navigation rail's own width.
///
/// A fixed lane rather than a proportion of the window: the rail holds the six
/// category names, and a name is what has to stay on one line. At 200 the
/// longest of them — `Data & privacy` at the category navigation's own 13/500 —
/// has room for its own row padding and a second locale's longer spelling beside
/// it, while the content column still gets the 640px measure it was drawn for out
/// of the window's own 960px minimum.
const SETTINGS_NAV_WIDTH: f32 = 200.;

/// The width at which the rail appears.
///
/// The arithmetic of the two columns and the gutter between them, which is the
/// same arithmetic `compact_settings_layout` uses for the stacked row: the
/// rail, one gutter, the measure — 856px. Below it the rail and the measure
/// cannot both be whole, so the surface falls back to the horizontal category
/// strip rather than drawing a rail that squeezes the form's own two columns.
///
/// It is a *viewport* threshold rather than a window one, because this surface
/// is also a tab inside the main window: there the pane it draws into is the
/// centre panel, which is narrower than the window behind it once the resource
/// tree has its share. The Settings window never sees it — its own minimum is
/// the shell's floor, which is above the number here — so on the window of its
/// own the rail is always up, and the strip is the tab's fallback and not a
/// second state of the same window.
fn settings_nav_rail(viewport_width: f32) -> bool {
    viewport_width >= SETTINGS_NAV_WIDTH + f32::from(space::LG) + SETTINGS_CONTENT_MAX_WIDTH
}

/// The size the Settings window opens at.
///
/// `UI-SPEC` §15.2 asks for an independent window, and this is the size that
/// window is built from: the width is the shell's floor above, which is above
/// the width at which the navigation rail and the whole 640px measure sit side by
/// side with the gutter between them — so the window opens on the layout it is
/// designed for rather than on the tab fallback, and the only slack in it is the
/// 120px past the measure rather than a second screen's worth of field. The
/// height is the title bar, the search line and the category rail over a page
/// that scrolls.
pub const SETTINGS_WINDOW_SIZE: (f32, f32) = (SETTINGS_MIN_WINDOW_WIDTH, 760.);

/// The narrowest and shortest the Settings window may be resized to.
///
/// The width is the shell's own floor and not this surface's layout arithmetic,
/// for the reason `design::size::WINDOW_MIN` gives: every breakpoint in this
/// product is at or above it. The layout's own rail threshold is *below* it, so
/// the rail is up across the window's whole resize range and the strip in
/// `render_tabs` is reachable only from the shell tab, where the pane is
/// narrower than the window. The height is the compact layout's own floor rather
/// than a borrowed number: below it the title bar, the search line and the
/// category navigation no longer fit together, and a settings window that cannot
/// show its own navigation is a window a person has to guess in.
pub const SETTINGS_WINDOW_MIN: (f32, f32) = (SETTINGS_MIN_WINDOW_WIDTH, 560.);

/// How often the shortcut reference re-reads the keymap generation.
///
/// The keycaps read the live keymap at render time and an external edit of
/// `keymap.json` rebuilds the bindings without notifying this view, so a cheap
/// counter comparison keeps the list honest without a keystroke.
const KEYMAP_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Tab order inside the settings surface.
///
/// GPUI walks the stops of one frame in slot order (`TabStopMap` sorts by tab
/// index, not by paint order), so within a block the slot a control carries is
/// the position Tab reaches it at.
///
/// The search field owns 0 and its clear button owns 1 (`TextInput::new`); the
/// category rail starts at 2; every row's slot is derived from its position in
/// [`SPECS`], which is why a row cannot collide with a row it is not next to.
mod tab_order {
    /// The first slot a category tab may take. The search field owns 0 and its
    /// clear button owns 1, both inside `TextInput`.
    pub const TABS_FIRST: isize = 2;
    /// The first slot a settings row may take, above the six category tabs.
    pub const ROWS_FIRST: isize = 12;
    /// Slots per category: eight rows of one slot each, with room to grow, so a
    /// row added to a category cannot reach the next category's block.
    pub const ROWS_PER_CATEGORY: isize = 9;
    /// The first slot a shortcut-reference row may take.
    pub const REFERENCE_FIRST: isize = 1_000;
    /// The two controls in the danger zone.
    pub const DANGER_FIRST: isize = 2_000;

    /// The one slot one settings row owns.
    ///
    /// One slot per row rather than one per control: every row draws exactly one
    /// control, and a second slot in the same block is a slot a control that does
    /// not exist would have taken.
    pub fn row(category_index: usize, spec_index: usize) -> isize {
        ROWS_FIRST + category_index as isize * ROWS_PER_CATEGORY + spec_index as isize
    }
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

const SETTINGS_PLACEHOLDER: &str = "Search settings…";
const REFERENCE_TITLE: &str = "Shortcut reference";
const REFERENCE_SUMMARY: &str =
    "Every command, its key, and the surface that key is live in. Press ? from anywhere.";

/// The danger zone's own label and caption.
///
/// The caption is the sentence that makes the zone readable: a row of red
/// buttons at the bottom of a window with nothing above it looks like a bug,
/// and this is what says the two things there are the two things that cannot be
/// undone, and what each one deletes. It names the consequence before the press
/// rather than after it, because the press that matters is the second one.
const DANGER_ZONE_LABEL: &str = "Danger zone";
const DANGER_ZONE_CAPTION: &str = "Neither can be undone. Clear cache deletes every cached snapshot and discovery result; Reset all settings restores every preference in this window. Everything else here takes effect as you change it.";
const CLEAR_CACHE_LABEL: &str = "Clear cache";
const CLEAR_CACHE_ARMED_LABEL: &str = "Delete the cache";
const CLEAR_CACHE_CONFIRM: &str = "Clear cache deletes every cached snapshot and discovery result under the cache folder. The cluster is not affected.";
const RESET_ALL_LABEL: &str = "Reset all settings";
const RESET_ALL_ARMED_LABEL: &str = "Reset everything";
const RESET_ALL_CONFIRM: &str = "Reset all settings deletes every preference in this window \u{2014} appearance, keyboard, cluster, editor, data and update settings \u{2014} and restores the built-in shortcuts. It does not touch cluster data.";
const CANCEL_LABEL: &str = "Cancel";

/// Shown when the settings file could not be parsed.
///
/// `settings::parse` falls back to the built-in defaults, and the worst outcome
/// on this screen is a file that fails to parse and a window that quietly shows
/// stock values: the reader changes a setting, it lands in a file that is still
/// broken, and nothing anywhere says so. So the parse is repeated here, where
/// the failure can be reported next to the values it produced — and the
/// sentence says what the app fell back to, because "it did not work" without
/// "and you are looking at defaults" is only half the information.
const SETTINGS_FILE_UNREADABLE: &str = "The settings file could not be read, so built-in defaults are in use. Fix the file, then reload.";
const SETTINGS_FILE_OPEN_LABEL: &str = "Open settings file";

/// Shown when a background write of the settings file failed after the control
/// already flipped.
const SAVE_FAILED_MESSAGE: &str =
    "The last change was not saved. Check write access to the settings file, then retry.";
const SAVE_NOT_SAVED_NOTE: &str = "Not saved";
const RETRY_LABEL: &str = "Retry";
const SAVING_LABEL: &str = "Saving…";

const UPDATES_UNAVAILABLE_MESSAGE: &str =
    "Application updates are unavailable in this build. Use a K8s Studio build with update support.";

/// The theme family that ships with the product. Everything else is a Zed theme.
const K8S_STUDIO_THEME_PREFIX: &str = "K8s Studio";

/// Text sizes offered for the data surfaces, in pixels.
///
/// The interface cannot scale its chrome without changing every spacing token
/// and hit area at the same time, so the honest size control is the one that
/// already works end to end: the data surfaces read this value for their font,
/// their row height and their column measurements, so the table and the editor
/// grow together. Discrete steps rather than free entry, because a person
/// choosing a text size wants to compare a few readable options.
const DATA_FONT_SIZES: &[f32] = &[11., 12., 13., 14., 16., 18.];

/// The accent names a reader can pick, in the order the control draws them.
///
/// The colour behind each name is the theme's own accent series, so this list is
/// names and the theme is the palette: nothing here is a colour value, and the
/// two stay independent the way §2.1's rule 3 requires.
const ACCENT_NAMES: &[&str] = &[
    "Electric blue",
    "Teal",
    "Green",
    "Amber",
    "Orange",
    "Red",
    "Violet",
    "Pink",
];

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

fn platform_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "Linux"
    }
}

/// The chord that closes the settings window, as the platform spells it.
///
/// The title bar says how to close the window it is in, because a window with
/// no close button of its own is a window a reader has to guess about. The key
/// is written the way the platform names it rather than as an English phrase,
/// so the sentence and the key agree.
fn close_chord() -> Keystroke {
    let chord = if cfg!(target_os = "macos") {
        "cmd-comma"
    } else {
        "ctrl-comma"
    };
    Keystroke::parse(chord).expect("the close chord is a literal")
}

/// The modifiers a shortcut may use, named the way the platform names them.
fn modifier_names() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Control, Command, Option, or Shift"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Ctrl, Alt, Shift, or Super"
    }
}

/// The recording hint, naming the modifiers the rule accepts.
fn recording_hint() -> String {
    format!(
        "Press a key with {modifier}, or press F2–F24 on its own. Press Escape to cancel.",
        modifier = modifier_names()
    )
}

fn keymap_reload_description() -> &'static str {
    "Reload the user keymap and apply the saved shortcuts."
}

fn unsupported_update_state() -> UpdateUiState {
    UpdateUiState::new(UpdatePhase::Unsupported).with_error(UPDATER_UNAVAILABLE_REASON)
}

/// The semantic colour a severity paints with.
///
/// One mapping for the whole product, and this is it: `role::status_for`
/// keeps `Success` quiet (healthy is the absence of a channel), which is why
/// an up-to-date check is grey here and in the Shell's update strip alike.
fn severity_color(severity: Severity, cx: &App) -> Hsla {
    design::role::status_for(severity, cx)
}

// ---------------------------------------------------------------------------
// The six categories
// ---------------------------------------------------------------------------

/// The six categories of `UI-SPEC` §15.1, in the order the rail draws them.
///
/// The order is the order a reader arrives in: what the window looks like, how
/// it is driven, what it talks to, what it edits, what it keeps, and how it
/// replaces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsCategory {
    Appearance,
    Keyboard,
    Cluster,
    Editor,
    DataPrivacy,
    Updates,
}

impl SettingsCategory {
    pub const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Keyboard,
        Self::Cluster,
        Self::Editor,
        Self::DataPrivacy,
        Self::Updates,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Keyboard => "Keyboard",
            Self::Cluster => "Cluster",
            Self::Editor => "Editor",
            Self::DataPrivacy => "Data & privacy",
            Self::Updates => "Updates",
        }
    }

    /// The one line under the category's name.
    ///
    /// It is not decoration: a search hits rows from all six categories at once,
    /// and a reader scanning a mixed list needs to know which page a hit belongs
    /// to before they click it.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Appearance => "Theme, accent, density, type and motion.",
            Self::Keyboard => "Preset, type-ahead, and the shortcut reference.",
            Self::Cluster => "Namespace, deep links, timeouts and tool availability.",
            Self::Editor => "Indentation, whitespace, validation and wrapping.",
            Self::DataPrivacy => "What is kept on disk, for how long, and what is never written.",
            Self::Updates => "How often to look, and which channel to trust.",
        }
    }

    fn index(self) -> usize {
        SettingsCategory::ALL
            .iter()
            .position(|category| *category == self)
            .expect("every category is in ALL")
    }
}

// ---------------------------------------------------------------------------
// The settings
// ---------------------------------------------------------------------------

/// What a setting's answer is.
///
/// `UI-SPEC` §15.3 names five control types and one prohibition, and this enum
/// is that table: a boolean is a switch, a path is a path, a number is a number
/// field, and an enum is answered by a control that shows every legal value.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Control {
    /// A boolean. A switch, because §15.3 names one and because a value a
    /// reader has to interpret is a setting they have to learn.
    Switch,
    /// A number inside a range, with a unit.
    Number {
        min: f64,
        max: f64,
        step: f64,
        unit: &'static str,
    },
    /// An identifier that is not an enum: a namespace, a path. Free text is the
    /// only honest control, because there is no list to read.
    Text,
    /// A filesystem path: shown, and revealed in the platform's file manager.
    Path,
    /// An answer chosen from a list. The shape is not chosen per row — it is
    /// derived from how many answers there are, in [`Control::for_choices`].
    Choices,
    /// A value nothing writes here: a build number, a probe result.
    Readonly,
    /// A button that goes somewhere.
    Action,
}

impl Control {
    /// The threshold at which an enum stops fitting on screen.
    const SEGMENTED_MAX: usize = 4;

    /// Whether this many answers fit on screen at once.
    ///
    /// Two to four answers become a segmented control, so the reader sees every
    /// legal value without a click. More than four cannot fit one control
    /// column, and a drop-down is then the only honest way to keep them all
    /// reachable: `UI-SPEC` §15.3's closing line is that a text box which
    /// silently rejects a name a reader typed is worse than no setting at all.
    ///
    /// The threshold is here rather than in each row so that a row cannot pick
    /// its own shape, because a row that picks its own shape is a row that
    /// eventually picks a free-text box.
    fn segmented(count: usize) -> bool {
        (2..=Self::SEGMENTED_MAX).contains(&count)
    }
}

/// One setting's identity: what it is called, what it does, and where it lives.
///
/// `keywords` is the search vocabulary a row does not print. A person looking
/// for "size" is looking for the text size row, which is called "Text size"; a
/// person looking for "vim" is looking for a row on Keyboard. The words in the
/// paragraph are for the reader and the words here are for the search box.
struct SettingSpec {
    setting: Setting,
    category: SettingsCategory,
    title: &'static str,
    /// The one line the row prints under its title.
    ///
    /// A row is a pair — a name on the left, a control on the right — and the
    /// mockup's row is exactly that, with a single line of small text beside the
    /// name. An eight-row page whose rows each carry a four-line paragraph is
    /// not a settings page, it is a manual: measured at a 232px label column, the
    /// Appearance page's paragraphs alone came to 737px and pushed the danger
    /// zone off the bottom of a 900px window. So the sentence is here, one line,
    /// and the paragraph is a hover away and in the search results, where a
    /// reader is already reading words.
    hint: &'static str,
    /// The whole sentence: the row's tooltip, the search haystack, and the second
    /// line of a search result.
    description: &'static str,
    keywords: &'static str,
    /// Why the control is disabled, when nothing reads the answer yet.
    ///
    /// A preference that persists and changes nothing is the same as a lie, and
    /// a switch that flips with no effect is the cheapest kind of lie. A row
    /// whose consumer is not in the build says so under its own title, in one
    /// line, and its control is disabled rather than pretending. The sentence
    /// names the specific thing that is missing, because "not supported yet" is
    /// the answer that lets a user file the same bug twice.
    pending: Option<&'static str>,
}

impl SettingSpec {
    const fn new(
        setting: Setting,
        category: SettingsCategory,
        title: &'static str,
        hint: &'static str,
        description: &'static str,
        keywords: &'static str,
        pending: Option<&'static str>,
    ) -> Self {
        Self {
            setting,
            category,
            title,
            hint,
            description,
            keywords,
            pending,
        }
    }
}

/// Every setting, once each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Setting {
    Theme,
    Accent,
    Density,
    UiFont,
    TextSize,
    Contrast,
    ReduceMotion,
    KeymapPreset,
    ShortcutReference,
    TypeAhead,
    VimMode,
    DefaultNamespace,
    DeepLinkSwitch,
    ConnectionTimeout,
    WatchRate,
    Helm,
    ClusterMetrics,
    TabWidth,
    SoftSpaces,
    FormatOnSave,
    ValidateWhileTyping,
    WordWrap,
    RecordLogContent,
    LogRetention,
    Cache,
    CacheLocation,
    CacheLimit,
    UpdateCheck,
    UpdateChannel,
    Version,
    CheckForUpdates,
}

impl Setting {
    fn spec(self) -> &'static SettingSpec {
        SPECS
            .iter()
            .find(|spec| spec.setting == self)
            .expect("every setting has a spec")
    }

    fn title(self) -> &'static str {
        self.spec().title
    }

    fn category(self) -> SettingsCategory {
        self.spec().category
    }

    /// The position of this row inside its own category.
    fn row(self) -> usize {
        SPECS
            .iter()
            .filter(|spec| spec.category == self.category())
            .position(|spec| spec.setting == self)
            .expect("a setting is inside its own category")
    }

    /// The element id and debug selector one row is measured under.
    fn selector(self) -> String {
        format!("settings-row-{}", slug(self.title()))
    }

    fn control_selector(self) -> String {
        format!("settings-control-{}", slug(self.title()))
    }

    /// The one tab slot this row's control takes.
    ///
    /// One slot, not two. A segmented control used to publish two — the first
    /// segment on one and every other segment on the other — which left a
    /// three-answer control answerable from the keyboard only by pressing Enter
    /// on its first segment. The control is a radio group now: one stop, arrow
    /// keys inside it, and the row's whole tab footprint is the slot below.
    fn tab(self) -> isize {
        tab_order::row(self.category().index(), self.row())
    }

    fn control(self) -> Control {
        use Control::*;
        // No wildcard arm: a setting added to the enum without a control here is
        // a compile error rather than an empty segmented control on the page.
        match self {
            Self::Contrast
            | Self::TypeAhead
            | Self::VimMode
            | Self::DeepLinkSwitch
            | Self::RecordLogContent
            | Self::SoftSpaces
            | Self::FormatOnSave
            | Self::ValidateWhileTyping
            | Self::WordWrap
            | Self::Cache => Switch,
            Self::DefaultNamespace => Text,
            Self::ConnectionTimeout => Number {
                min: 1.,
                max: 120.,
                step: 1.,
                unit: "s",
            },
            Self::TabWidth => Number {
                min: 1.,
                max: 8.,
                step: 1.,
                unit: "",
            },
            Self::LogRetention => Number {
                min: 1.,
                max: 90.,
                step: 1.,
                unit: "d",
            },
            Self::CacheLimit => Number {
                min: 0.,
                max: 4096.,
                step: 64.,
                unit: "MB",
            },
            Self::Helm | Self::ClusterMetrics | Self::Version => Readonly,
            Self::CacheLocation => Path,
            Self::ShortcutReference | Self::CheckForUpdates => Action,
            Self::Theme
            | Self::Accent
            | Self::Density
            | Self::UiFont
            | Self::TextSize
            | Self::ReduceMotion
            | Self::KeymapPreset
            | Self::WatchRate
            | Self::UpdateCheck
            | Self::UpdateChannel => Choices,
        }
    }

    /// The file key a value is stored under, or `None` for a row nothing stores.
    fn storage_key(self) -> Option<&'static str> {
        use Setting::*;
        Some(match self {
            Accent => "accent",
            Density => "density",
            UiFont => "uiFont",
            TypeAhead => "typeAhead",
            VimMode => "vimMode",
            DefaultNamespace => "defaultNamespace",
            DeepLinkSwitch => "deepLinkSwitch",
            ConnectionTimeout => "connectionTimeout",
            WatchRate => "watchRate",
            TabWidth => "tabWidth",
            SoftSpaces => "softSpaces",
            FormatOnSave => "formatOnSave",
            ValidateWhileTyping => "validateOnInput",
            WordWrap => "wordWrap",
            RecordLogContent => "recordLogContent",
            LogRetention => "logRetentionDays",
            CacheLocation => "cacheDir",
            CacheLimit => "cacheLimitMb",
            UpdateCheck => "updateCheck",
            UpdateChannel => "updateChannel",
            _ => return None,
        })
    }

    /// Whether this row's control writes an answer.
    ///
    /// `pending` disables a control that would store a value nothing reads. A
    /// path that opens the folder, a build number and a button that goes
    /// somewhere all work whatever the answer is, so a caption saying the answer
    /// is fixed must not take the control away with it.
    #[cfg(test)]
    fn sets_an_answer(self) -> bool {
        matches!(
            self.control(),
            Control::Switch | Control::Number { .. } | Control::Text | Control::Choices
        )
    }

    /// The name a boolean row prints beside its own check box.
    ///
    /// `Forms and settings` asks for a visible label next to the field it names,
    /// and `Checkbox` is the right control for an independent choice — but a box
    /// is not a name. On this surface the box sat alone on the trailing edge of a
    /// two-column row with its setting named a hundred pixels to the left, and a
    /// box with no word of its own is the one control a reader cannot read at a
    /// glance.
    ///
    /// A *name for the choice*, not the row's description: the description stays
    /// in the label column with every other row's, so the two lines say two
    /// different things and neither repeats the other. Ten rows carry one, and
    /// `control()` is the authority on which — a row whose control is not a box
    /// has no word to put beside one.
    fn box_label(self) -> Option<&'static str> {
        match self {
            Self::Contrast => Some("Increase contrast"),
            Self::TypeAhead => Some("Type-ahead"),
            Self::VimMode => Some("Modal editing"),
            Self::DeepLinkSwitch => Some("Follow deep links"),
            Self::RecordLogContent => Some("Record log content"),
            Self::SoftSpaces => Some("Insert spaces"),
            Self::FormatOnSave => Some("Reformat on write"),
            Self::ValidateWhileTyping => Some("Check as you edit"),
            Self::WordWrap => Some("Wrap long lines"),
            Self::Cache => Some("Cache to disk"),
            _ => None,
        }
    }

    /// The group inside the page that this row belongs to.
    ///
    /// A page of eight rows in one flat list is a list, not a page: the reader
    /// scans down a single column and has to read every row's second line to
    /// work out that Theme and Accent are about the colour of the window while
    /// Contrast and Reduce motion are about reading it. `Vertical rhythm shows
    /// grouping` is a number rather than an intention, so the grouping is stated
    /// as a name per row and drawn as a section head, and `space::XL` between
    /// two sections is three times the 8px inside a row — the gap that says "a new
    /// group" against the gap that says "the next row of this one".
    ///
    /// Every group is a *run* of consecutive rows, so the page needs no
    /// reordering and every row keeps the tab slot its position gives it. The
    /// Appearance split is therefore cut where the list already changes subject —
    /// after Accent, where the colour ends and the type begins — rather than
    /// after Text size, which would have been the tidier cut if the rows could
    /// move. A reader gets the same answer either way; only the group name is a
    /// compromise, and no group on any of the six pages holds a single row,
    /// because a head over one row is a label at a heading's weight.
    fn group(self) -> &'static str {
        use Setting::*;
        match self {
            // Appearance: the colour, then the type and the room, then the two
            // settings that exist for a reader rather than for the window.
            Theme | Accent => "Theme",
            Density | UiFont | TextSize => "Type and spacing",
            Contrast | ReduceMotion => "Reading and motion",
            // Keyboard: the set of keys, then the two answers that are not keys.
            KeymapPreset | ShortcutReference => "Shortcuts",
            TypeAhead | VimMode => "Typing and navigation",
            // Cluster: the connection, then the tools hanging off it.
            DefaultNamespace | DeepLinkSwitch | ConnectionTimeout | WatchRate => "Sessions",
            Helm | ClusterMetrics => "Tools",
            // Editor: how a line is indented, then what happens to it as it is
            // written.
            TabWidth | SoftSpaces => "Indentation",
            FormatOnSave | ValidateWhileTyping | WordWrap => "Writing",
            // Data & privacy: what is kept about the reader, then the cache.
            RecordLogContent | LogRetention => "Privacy",
            Cache | CacheLocation | CacheLimit => "Cache",
            // Updates: the policy, then this installation.
            UpdateCheck | UpdateChannel => "Automatic checks",
            Version | CheckForUpdates => "This build",
        }
    }

    /// The answers an enum row offers, in draw order.
    fn choices(self, cx: &App) -> Vec<String> {
        use Setting::*;
        match self {
            Theme => {
                let mut names = vec![ThemeChoice::System.label().to_owned()];
                names.extend(theme_names(cx));
                names
            }
            Accent => ACCENT_NAMES.iter().map(|name| (*name).to_owned()).collect(),
            Density => ["Comfortable", "Normal", "Dense"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            UiFont => ["Inter", "System"].into_iter().map(str::to_owned).collect(),
            TextSize => DATA_FONT_SIZES
                .iter()
                .copied()
                .map(data_font_label)
                .collect(),
            ReduceMotion => ReduceMotionChoice::ALL
                .into_iter()
                .map(|choice| choice.label().to_owned())
                .collect(),
            KeymapPreset => [keymap::KeymapPreset::Lens, keymap::KeymapPreset::Vscode]
                .into_iter()
                .map(|preset| preset.label().to_owned())
                .collect(),
            WatchRate => ["Paused", "1 s", "2 s", "5 s", "10 s"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            UpdateCheck => ["Startup", "Daily", "Weekly", "Manual"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            UpdateChannel => ["Stable", "Beta", "Canary"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// The settings the surface owns, in draw order.
///
/// The list is the single source of truth for four things that used to be four
/// hand-written copies of each other: the rows a page draws, the rows the search
/// filters, the count the toolbar states, and the tab slots the controls take.
/// Six categories of at most eight rows each;
/// [`tests::every_category_fits_in_eight_rows`] is what says so, because a ninth
/// row means the information architecture is wrong, not that the page needs
/// scrolling.
const SPECS: &[SettingSpec] = &[
    // -- Appearance -------------------------------------------------------
    SettingSpec::new(
        Setting::Theme,
        SettingsCategory::Appearance,
        "Theme",
        "A K8s Studio or Zed theme",
        "A K8s Studio or Zed theme. System follows the desktop setting.",
        "colour color skin light dark appearance",
        None,
    ),
    SettingSpec::new(
        Setting::Accent,
        SettingsCategory::Appearance,
        "Accent",
        "Selection and the one primary action",
        "The colour used for selection and for the one primary action. Every surface in this build reads the theme's own accent series, and nothing here can set that palette.",
        "highlight colour electric blue",
        Some("Surfaces read the theme's own accent."),
    ),
    SettingSpec::new(
        Setting::Density,
        SettingsCategory::Appearance,
        "Density",
        "Comfortable 32px · normal 28px · dense 24px",
        "How much air each row carries: 32px comfortable, 28px normal, 24px dense. Row height is a layout constant in every surface that has one, and no surface reads this yet.",
        "rows compact spacing tight comfortable",
        Some("Row height is fixed in every surface."),
    ),
    SettingSpec::new(
        Setting::UiFont,
        SettingsCategory::Appearance,
        "UI font",
        "Inter ships with the app",
        "Inter ships with the app. System uses the desktop's own interface font. The chrome font is loaded with the theme and no surface reads this yet.",
        "typeface inter variable typography",
        Some("The chrome font ships with the theme."),
    ),
    SettingSpec::new(
        Setting::TextSize,
        SettingsCategory::Appearance,
        "Text size",
        "Tables, logs and the YAML editor",
        "Text size for resource tables, logs and the YAML editor. Larger sizes also grow the row height, so text is never cropped.",
        "font size zoom readable data",
        None,
    ),
    SettingSpec::new(
        Setting::Contrast,
        SettingsCategory::Appearance,
        "Contrast",
        "Brighter text, icons and borders",
        "Increase text, icon and border contrast throughout the workbench.",
        "accessibility high contrast a11y legibility",
        None,
    ),
    SettingSpec::new(
        Setting::ReduceMotion,
        SettingsCategory::Appearance,
        "Reduce motion",
        "Less animation, everywhere",
        "Reduce non-essential animation. System follows the motion setting the app starts with.",
        "animation accessibility motion vestibular",
        None,
    ),
    // -- Keyboard ---------------------------------------------------------
    SettingSpec::new(
        Setting::KeymapPreset,
        SettingsCategory::Keyboard,
        "Keymap preset",
        "A whole set of shortcuts at once",
        "A whole set of shortcuts at once. Your own overrides are kept either way.",
        "bindings vscode lens shortcuts",
        None,
    ),
    SettingSpec::new(
        Setting::ShortcutReference,
        SettingsCategory::Keyboard,
        "Shortcut reference",
        "Every command and its key · press ?",
        REFERENCE_SUMMARY,
        "cheatsheet commands help question mark keybindings list",
        None,
    ),
    SettingSpec::new(
        Setting::TypeAhead,
        SettingsCategory::Keyboard,
        "Type-ahead",
        "Jump to a row by typing its name",
        "Jump to a row by typing its name in a table or in the sidebar. Tables do this unconditionally, so nothing reads this yet.",
        "jump search quick navigation",
        Some("Tables type-ahead already."),
    ),
    SettingSpec::new(
        Setting::VimMode,
        SettingsCategory::Keyboard,
        "Vim mode",
        "Modal editing and h/j/k/l",
        "Modal editing and h/j/k/l navigation throughout the app. A Vim preset would have to be written into the keymap, and this build ships Default and VS Code only.",
        "vi modal hjkl",
        Some("This build ships Default and VS Code."),
    ),
    // -- Cluster ----------------------------------------------------------
    SettingSpec::new(
        Setting::DefaultNamespace,
        SettingsCategory::Cluster,
        "Default namespace",
        "What a new session opens on",
        "The namespace a new cluster session opens on. Empty means all namespaces. A session takes its namespace from the switcher, so nothing reads this yet.",
        "ns scope context where",
        Some("The session takes it from the switcher."),
    ),
    SettingSpec::new(
        Setting::DeepLinkSwitch,
        SettingsCategory::Cluster,
        "Follow deep links",
        "Switch to the cluster a link names",
        "Switch to the cluster a k8s-gpui:// link names, instead of opening it in the current one. The deep-link handler is the shell's, so nothing reads this yet.",
        "url link protocol deeplink open",
        Some("The shell owns the deep-link handler."),
    ),
    SettingSpec::new(
        Setting::ConnectionTimeout,
        SettingsCategory::Cluster,
        "Connection timeout",
        "How long a connection may take",
        "How long a cluster connection may take before it is reported as failed. The client is built with its own timeout, so nothing reads this yet.",
        "timeout seconds hang network",
        Some("The client sets its own timeout."),
    ),
    SettingSpec::new(
        Setting::WatchRate,
        SettingsCategory::Cluster,
        "Watch rate",
        "How often live rows refresh",
        "How often live resources are refreshed when nothing else is happening. The watch interval is set where the source is built, so nothing reads this yet.",
        "poll refresh live update interval",
        Some("The source sets the watch interval."),
    ),
    SettingSpec::new(
        Setting::Helm,
        SettingsCategory::Cluster,
        "Helm",
        "Inspect and manage releases",
        "Install the Helm CLI to inspect and manage releases. K8s Studio does not include Helm.",
        "releases cli tool integration",
        None,
    ),
    SettingSpec::new(
        Setting::ClusterMetrics,
        SettingsCategory::Cluster,
        "Cluster metrics",
        "CPU and memory per pod",
        "Use metrics-server in the selected cluster to read CPU and memory values.",
        "cpu memory prometheus server tool integration",
        None,
    ),
    // -- Editor -----------------------------------------------------------
    SettingSpec::new(
        Setting::TabWidth,
        SettingsCategory::Editor,
        "Tab width",
        "Spaces for one indent level",
        "Spaces inserted for one indentation level in the YAML editor. The editor's tab stop is a constant, so nothing reads this yet.",
        "indent spaces width",
        Some("The editor's tab stop is a constant."),
    ),
    SettingSpec::new(
        Setting::SoftSpaces,
        SettingsCategory::Editor,
        "Soft spaces",
        "Spaces instead of tab characters",
        "Insert spaces instead of a tab character. The editor already inserts spaces, so nothing reads this yet.",
        "tabs indent whitespace",
        Some("The editor already inserts spaces."),
    ),
    SettingSpec::new(
        Setting::FormatOnSave,
        SettingsCategory::Editor,
        "Format on save",
        "Reformat on write",
        "Reformat the YAML when a change is written to the cluster. Formatting happens in the apply path, so nothing reads this yet.",
        "format pretty print apply",
        Some("Formatting happens in the apply path."),
    ),
    SettingSpec::new(
        Setting::ValidateWhileTyping,
        SettingsCategory::Editor,
        "Validate while typing",
        "Check as you edit",
        "Check the YAML as it is edited rather than on apply. The editor already validates on change, so nothing reads this yet.",
        "diagnostics lint error schema",
        Some("The editor already validates on change."),
    ),
    SettingSpec::new(
        Setting::WordWrap,
        SettingsCategory::Editor,
        "Word wrap",
        "Wrap long lines to the editor",
        "Wrap long lines to the editor's width. The editor does not wrap, so nothing reads this yet.",
        "wrap soft line break",
        Some("The editor does not wrap."),
    ),
    // -- Data & privacy ---------------------------------------------------
    // The security row is first on the page on purpose: §15.1 calls it a
    // setting that was missing, it defaults off, and a reader has to be able to
    // find its current state without reading to the bottom of the page.
    SettingSpec::new(
        Setting::RecordLogContent,
        SettingsCategory::DataPrivacy,
        "Record log content",
        "Off. Nothing is written unless you allow it",
        "Log lines are not written to disk unless you turn this on. Off by default, and nothing in this build writes log lines anywhere, so there is nothing to record yet.",
        "security privacy secret telemetry record capture",
        Some("Off, and nothing here records it."),
    ),
    SettingSpec::new(
        Setting::LogRetention,
        SettingsCategory::DataPrivacy,
        "Log retention",
        "How long recorded lines are kept",
        "How long recorded log lines are kept before they are deleted. Nothing is recorded in this build, so nothing expires yet.",
        "privacy days keep expire delete",
        Some("Nothing is recorded, so nothing expires."),
    ),
    SettingSpec::new(
        Setting::Cache,
        SettingsCategory::DataPrivacy,
        "Cache",
        "Discovery results and snapshots",
        "Cache discovery results and snapshots to reduce startup time. Secrets are never stored.",
        "disk snapshot discovery offline",
        None,
    ),
    SettingSpec::new(
        Setting::CacheLocation,
        SettingsCategory::DataPrivacy,
        "Cache location",
        "Where the cache is written",
        "Where the cache is written. The button opens that folder. The cache root is the platform's cache directory, and this build offers no other path.",
        "path folder directory disk where",
        Some("The cache root is the platform's folder."),
    ),
    SettingSpec::new(
        Setting::CacheLimit,
        SettingsCategory::DataPrivacy,
        "Cache size limit",
        "How much disk the cache may use",
        "How much disk the cache may use. Older snapshots are dropped first. No eviction runs in this build, so the cache grows until it is cleared.",
        "quota cap megabytes evict",
        Some("No eviction runs; the cache grows."),
    ),
    // -- Updates ----------------------------------------------------------
    SettingSpec::new(
        Setting::UpdateCheck,
        SettingsCategory::Updates,
        "Check for updates",
        "How often the app looks",
        "How often the app looks for a newer build on its own. The updater runs when something asks it to, so nothing reads this yet.",
        "update upgrade download periodic",
        Some("The updater only runs when asked."),
    ),
    SettingSpec::new(
        Setting::UpdateChannel,
        SettingsCategory::Updates,
        "Channel",
        "Which stream of builds to trust",
        "Which stream of builds to trust. A channel is a source the updater is configured with, so nothing reads this yet.",
        "stable beta canary release stream",
        Some("The build's updater has one channel."),
    ),
    SettingSpec::new(
        Setting::Version,
        SettingsCategory::Updates,
        "Version",
        "The installed build",
        "The installed K8s Studio build. Copy it into a bug report.",
        "about build number release",
        None,
    ),
    SettingSpec::new(
        Setting::CheckForUpdates,
        SettingsCategory::Updates,
        "Check now",
        "Look for a newer build",
        "Look for a newer K8s Studio build for this installation.",
        "update upgrade download now",
        None,
    ),
];

/// A row's answer, in one type so the search and the control can both read it.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
    /// An enum answer, held as the label the control shows.
    Choice(String),
}

impl Value {
    /// The text a search can match, whether or not it is a value a reader types.
    fn as_text(&self) -> String {
        match self {
            Value::Bool(value) => if *value { "on" } else { "off" }.to_owned(),
            Value::Number(value) => {
                if value.fract() == 0.0 {
                    format!("{value:.0}")
                } else {
                    value.to_string()
                }
            }
            Value::Text(value) => value.clone(),
            Value::Choice(value) => value.clone(),
        }
    }
}

/// The label the control shows for a text size, in the words a person says.
fn data_font_label(size: f32) -> String {
    if size == settings::PRODUCT_DATA_FONT_SIZE {
        format!("{size:.0} px (default)")
    } else {
        format!("{size:.0} px")
    }
}

/// The data font size the surfaces currently measure with.
fn data_font_size(cx: &App) -> f32 {
    f32::from(crate::settings::data_typography(cx).size)
}

fn slug(title: &str) -> String {
    title
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Reading and writing values
// ---------------------------------------------------------------------------

/// The user settings as the file holds them, defaults folded in.
///
/// One parse per read rather than one per row: the surface asks for thirty
/// values in a frame, and re-reading a file thirty times to render thirty rows
/// is how a settings window turns into a stutter.
fn user_settings(cx: &App) -> UserSettings {
    raw_user_settings(cx)
        .map(|raw| settings::parse(&raw))
        .unwrap_or_default()
}

fn raw_user_settings(cx: &App) -> Option<String> {
    cx.try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings().map(str::to_owned))
}

/// What a setting holds before anybody has chosen.
///
/// Every default is the conservative one, and one of them matters more than the
/// rest: §15.1 calls `recordLogContent` a security setting that was missing, and
/// a security setting whose unset state is a choice is a setting that leaks by
/// default.
fn default_value(setting: Setting) -> Value {
    use Setting::*;
    match setting {
        Accent => Value::Choice(ACCENT_NAMES[0].to_owned()),
        Density => Value::Choice("Comfortable".to_owned()),
        UiFont => Value::Choice("Inter".to_owned()),
        ReduceMotion => Value::Choice(ReduceMotionChoice::System.label().to_owned()),
        KeymapPreset => Value::Choice(keymap::KeymapPreset::default().label().to_owned()),
        DefaultNamespace => Value::Text(String::new()),
        WatchRate => Value::Choice("2 s".to_owned()),
        TabWidth => Value::Number(2.),
        LogRetention => Value::Number(7.),
        CacheLimit => Value::Number(512.),
        UpdateCheck => Value::Choice("Weekly".to_owned()),
        UpdateChannel => Value::Choice("Stable".to_owned()),
        // Off, always. Nothing is recorded until somebody asks for it.
        RecordLogContent | TypeAhead | VimMode | DeepLinkSwitch => Value::Bool(false),
        SoftSpaces | FormatOnSave | ValidateWhileTyping | WordWrap => Value::Bool(true),
        ConnectionTimeout => Value::Number(10.),
        _ => Value::Text(String::new()),
    }
}

/// The value a setting currently holds.
/// The Theme row's answer: the label of the option the menu would tick.
///
/// The store answers a theme in *modes*, and deliberately so: `{"mode":"dark",
/// "theme":"K8s Studio Dark"}` reads back as `Dark` because the mode is the part
/// of the choice a theme registry cannot overrule. But this row's options are
/// the thirteen theme *names*, so a trigger that printed the mode showed a value
/// that is not in its own list, and the menu ticked nothing at all — a reader who
/// opened the Theme row could not tell which theme was in force and had to guess
/// it from the app's colours.
///
/// So the row answers with the option rather than with the mode: the product theme
/// that carries the mode in force, or `System` when the app is following the
/// desktop. One function, so the trigger's text and the menu's tick can never
/// disagree — which is the same defect the segmented control's disabled state had
/// and the reason it is worth naming.
fn theme_answer(cx: &App) -> String {
    match settings::theme_choice(cx) {
        ThemeChoice::System => ThemeChoice::System.label().to_owned(),
        ThemeChoice::Light => settings::PRODUCT_THEME_LIGHT.to_owned(),
        ThemeChoice::Dark => settings::PRODUCT_THEME_DARK.to_owned(),
        ThemeChoice::Named(name) => name,
    }
    .to_owned()
}

fn read_setting(setting: Setting, cx: &App, stored: &UserSettings) -> Value {
    use Setting::*;
    match setting {
        Theme => Value::Choice(theme_answer(cx)),
        TextSize => Value::Choice(data_font_label(data_font_size(cx))),
        Contrast => Value::Bool(settings::increase_contrast_enabled(cx)),
        ReduceMotion => Value::Choice(reduce_motion_choice(cx).label().to_owned()),
        KeymapPreset => Value::Choice(keymap::status(cx).preset.label().to_owned()),
        Cache => Value::Bool(settings::disk_cache_enabled_from_app(cx)),
        _ => match setting.storage_key() {
            Some(key) => match stored.extra.get(key) {
                Some(value) => stored_value(setting, value),
                None => default_value(setting),
            },
            None => default_value(setting),
        },
    }
}

fn stored_value(setting: Setting, value: &serde_json::Value) -> Value {
    match setting.control() {
        Control::Switch => Value::Bool(value.as_bool().unwrap_or(false)),
        Control::Number { .. } => Value::Number(value.as_f64().unwrap_or(0.)),
        Control::Text | Control::Path => Value::Text(value.as_str().unwrap_or_default().to_owned()),
        _ => Value::Choice(value.as_str().unwrap_or_default().to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Reduce motion
// ---------------------------------------------------------------------------

/// The three answers to "how much motion", including the one that is not an
/// override.
///
/// `reduce_motion` is stored as an `Option` and `None` is the state the app
/// boots into, so a two-value control cannot show it: the first press on a
/// switch would turn an inherited value into a stored one without ever saying
/// that it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReduceMotionChoice {
    System,
    On,
    Off,
}

impl ReduceMotionChoice {
    const ALL: [Self; 3] = [Self::System, Self::On, Self::Off];

    fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::On => "On",
            Self::Off => "Off",
        }
    }

    /// The value written to the settings file, where `None` removes the override.
    fn mode(self) -> Option<ReduceMotionMode> {
        match self {
            Self::System => None,
            Self::On => Some(ReduceMotionMode::On),
            Self::Off => Some(ReduceMotionMode::Off),
        }
    }

    /// Whether the app runs with motion reduced under this choice.
    ///
    /// `System` stores nothing, so the app falls back to the motion setting it
    /// starts with, and that is the honest answer to show rather than an
    /// override the row has just given up.
    fn applied(self) -> bool {
        matches!(self, Self::On)
    }
}

/// The reduce-motion choice the file asks for, without the built-in default folded in.
fn stored_reduce_motion(cx: &App) -> Option<ReduceMotionMode> {
    raw_user_settings(cx)
        .as_deref()
        .and_then(|content| settings::parse(content).reduce_motion)
}

fn reduce_motion_choice(cx: &App) -> ReduceMotionChoice {
    match stored_reduce_motion(cx) {
        Some(ReduceMotionMode::On) => ReduceMotionChoice::On,
        Some(ReduceMotionMode::Off) => ReduceMotionChoice::Off,
        None => ReduceMotionChoice::System,
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Checking,
    Available,
    Unavailable,
    Error,
}

impl Capability {
    fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking…",
            Self::Available => "Available",
            Self::Unavailable => "Not found",
            Self::Error => "Error",
        }
    }

    /// Which health verdict this state reports.
    ///
    /// `DESIGN.md` §4 gives the hollow ring to `info / syncing` and the dash to
    /// "no verdict", so a probe still running is `Info` and a tool that is not
    /// installed is `Muted`.
    fn severity(self) -> Severity {
        match self {
            Self::Checking => Severity::Info,
            Self::Available => Severity::Success,
            Self::Unavailable => Severity::Muted,
            Self::Error => Severity::Error,
        }
    }

    fn is_checking(self) -> bool {
        matches!(self, Self::Checking)
    }
}

impl From<HelmCapability> for Capability {
    fn from(value: HelmCapability) -> Self {
        match value {
            HelmCapability::Checking => Self::Checking,
            HelmCapability::Available => Self::Available,
            HelmCapability::NotInstalled => Self::Unavailable,
            HelmCapability::Timeout | HelmCapability::Error => Self::Error,
        }
    }
}

// ---------------------------------------------------------------------------
// Globals the surface installs
// ---------------------------------------------------------------------------

/// The settings layout a person left behind.
///
/// A settings window is a place a person returns to, so the page outlives the
/// view that drew it: closing the window and opening it again lands on the same
/// page instead of resetting to Appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SettingsLayout {
    category: Option<SettingsCategory>,
}

impl Global for SettingsLayout {}

fn layout_state(cx: &App) -> SettingsLayout {
    cx.try_global::<SettingsLayout>()
        .copied()
        .unwrap_or_default()
}

fn remember_layout(cx: &mut App, layout: SettingsLayout) {
    cx.set_global(layout);
}

/// What the surface reads about the running session.
///
/// The Helm and metrics probes and the updater are the shell's, and the shell is
/// not in the Settings window, so a window-hosted view has to read those facts
/// from somewhere that outlives the view that owns them. They are published once
/// and every view reads the same copy, which is the pattern `SettingsLayout`
/// already uses for the page a person left on: a global written by the owner of
/// the fact and read by the views that draw it.
///
/// This is not a second settings store. The *values* still live in
/// [`SettingsStore`], [`DiskCache`] and `SettingsSaveStatus`, which every window
/// in the process already shares, and this global holds only what the shell
/// probes and the app's updater know.
#[derive(Clone)]
pub struct SettingsEnvironment {
    helm: HelmCapability,
    metrics: Capability,
    helm_error: Option<String>,
    metrics_error: Option<String>,
    update_state: UpdateUiState,
    update_actions: Option<UpdateActions>,
}

impl Default for SettingsEnvironment {
    fn default() -> Self {
        Self {
            helm: HelmCapability::Checking,
            metrics: Capability::Checking,
            helm_error: None,
            metrics_error: None,
            // A build that never published an update state cannot update itself,
            // and saying so is the first thing the Updates page has to be true
            // about.
            update_state: unsupported_update_state(),
            update_actions: None,
        }
    }
}

impl Global for SettingsEnvironment {}

impl SettingsEnvironment {
    /// What the session reports, read by every view that draws it.
    pub fn read(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// What the probes report, and why not when they could not.
    pub fn set_capabilities(
        cx: &mut App,
        helm: HelmCapability,
        metrics: Capability,
        helm_error: Option<String>,
        metrics_error: Option<String>,
    ) {
        let mut environment = Self::read(cx);
        environment.helm = helm;
        environment.metrics = metrics;
        environment.helm_error = helm_error;
        environment.metrics_error = metrics_error;
        cx.set_global(environment);
    }
}

/// A keybinding conflict that belongs to one shortcut row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BindingConflict {
    keystrokes: String,
    /// The other actions bound to the same key in the same context.
    others: Vec<String>,
    context: String,
}

impl BindingConflict {
    /// One line under the row, next to the binding it contradicts.
    fn label(&self) -> String {
        let context = if self.context.trim().is_empty() {
            "in every surface".to_owned()
        } else {
            format!("in {}", keyboard_context_description(&self.context))
        };
        format!(
            "{} is also bound to {} {context}. The last binding wins.",
            self.keystrokes,
            self.others.join(", ")
        )
    }
}

#[derive(Clone, Debug)]
struct KeyboardCommand {
    action_name: String,
    label: String,
    description: String,
    action_input: Option<String>,
    context: Option<String>,
    when: Option<String>,
    editable: bool,
}

#[derive(Clone, Debug)]
struct KeyboardSection {
    title: String,
    commands: Vec<KeyboardCommand>,
}

/// A memoized [`keyboard_sections`] answer, valid while its inputs hold.
struct KeyboardSections {
    keymap_generation: u64,
    updater_available: bool,
    action_count: usize,
    sections: Rc<[KeyboardSection]>,
}

struct ParameterizedKeyboardAction {
    action_name: &'static str,
    label: &'static str,
    group: &'static str,
    description: &'static str,
    value_field: &'static str,
    value_offset: u64,
}

#[derive(Clone)]
struct Recording {
    command: KeyboardCommand,
    previous_focus: Option<FocusHandle>,
}

/// Which of the surface's drop-downs has its menu on screen.
///
/// One slot for all of them, so opening a second one closes the first: a
/// settings page answers a setting at a time, and two stacked popovers over one
/// row are a surface nobody can read. `None` is also what a new query or a new
/// page returns the surface to, because the trigger that opened the menu is gone
/// with the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenMenu(Setting);

#[derive(Clone)]
struct OpenMenuState {
    menu: Entity<PopupMenu>,
}

/// The two steps of a confirmation in the danger zone.
///
/// Deleting the cache or the settings file cannot be undone, so the first press
/// only arms the step: the button changes to the destructive one, the zone says
/// what it deletes, and Escape or Cancel returns to `Idle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DangerStep {
    #[default]
    Idle,
    ClearCache,
    ResetAll,
}

impl DangerStep {
    fn armed(self) -> bool {
        !matches!(self, DangerStep::Idle)
    }
}

type ThemeHandler = Rc<dyn Fn(ThemeChoice, &mut Window, &mut App) -> Result<(), String>>;
type KeymapHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type NoticeHandler = Rc<dyn Fn(String, Severity, &mut App)>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub struct SettingsView {
    recording_focus: FocusHandle,
    search_focus: FocusHandle,
    search_input: Entity<TextInput>,
    search_query: String,
    /// Whether the content area is showing the shortcut reference.
    ///
    /// There is one search box, not two, and it searches whatever list is on
    /// screen. A second box would answer a second question from the same place,
    /// and the reader would have to work out which of two identical fields
    /// applied to the rows in front of them.
    reference: bool,
    /// Whether the window is too narrow for the two-column row.
    compact: bool,
    /// Whether the categories are drawn as a rail beside the content rather than
    /// as a strip above it.
    ///
    /// One boolean read from the window's own width, because the two layouts
    /// differ in more than where the names are drawn: they differ in what the
    /// chrome bands are inset by, and a band that asked the layout engine to
    /// place itself would put the search field on a different spine from the
    /// rows underneath it.
    nav_rail: bool,
    /// The rail's own scroll handle, kept apart from the content's because the
    /// two panes scroll independently and one handle would tie them together.
    nav_scroll: ScrollHandle,
    /// Where the chrome bands start, measured from the window's leading edge.
    ///
    /// Read from the window once per frame rather than asked of the layout
    /// engine, because "line the title bar, the search line and the rows up
    /// with the content column" is a number and the surface needs the number on
    /// eight separate bands that all have to agree to the pixel. `Spatial
    /// grammar`'s "repeated gaps are quality invariants" is not satisfiable when
    /// eight boxes each ask a flex container to do the arithmetic on its own.
    ///
    /// It is a leading edge, not a symmetric margin: with the rail up it is the
    /// rail's width plus the gutter beside it, so every band in the chrome
    /// plane starts on the same x the content column does; without it, the
    /// narrow fallback centres the measure because a capped column pinned to
    /// one edge of a wide window reads as content that failed to fill it.
    measure_inset: gpui_kit::Pixels,
    /// Where the full-width chrome bands start.
    ///
    /// The same number as [`Self::measure_inset`] when the categories are a
    /// strip, and the rail's own width plus the gutter beside it when they are
    /// a rail: the title bar spans the window, so its contents have to start
    /// where the content column does rather than at the window's edge.
    chrome_inset: gpui_kit::Pixels,
    open_menu: Option<OpenMenu>,
    menu: Option<OpenMenuState>,
    category: SettingsCategory,
    scroll: ScrollHandle,
    helm: HelmCapability,
    metrics: Capability,
    helm_error: Option<String>,
    metrics_error: Option<String>,
    error: Option<String>,
    update_state: UpdateUiState,
    update_actions: Option<UpdateActions>,
    on_theme: Option<ThemeHandler>,
    on_create_or_show_keymap: Option<KeymapHandler>,
    on_reload_keymap: Option<KeymapHandler>,
    on_notice: Option<NoticeHandler>,
    on_close: Option<CloseHandler>,
    /// Whether this view has a window of its own, which decides who names the
    /// visible pane. See [`SettingsView::set_owns_window`].
    owns_window: bool,
    /// The name this view last published to its window, so a pane change writes
    /// the title once rather than on every frame.
    window_title: Option<String>,
    recording: Option<Recording>,
    recording_intercept: Option<Subscription>,
    danger_step: DangerStep,
    /// Async settings write state. A write failure arrives here, not in the
    /// `Result` of the click.
    save_pending: bool,
    save_error: Option<String>,
    keymap_generation: u64,
    keyboard_sections: RefCell<Option<KeyboardSections>>,
    /// The value each text field last showed, so an external change writes into
    /// the field once rather than on every frame.
    text_synced: BTreeMap<Setting, String>,
    _keymap_poll: Task<()>,
    _save_status_observer: Subscription,
    _settings_observer: Subscription,
    _disk_cache_observer: Subscription,
    _environment_observer: Subscription,
    _search_intercept: Subscription,
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let search_input = new_search_field(cx);
        let search_focus = search_input.read(cx).focus_handle(cx);
        let search_listener = cx.listener(|view, event: &KeystrokeEvent, window, cx| {
            view.handle_search_escape(&event.keystroke, window, cx);
        });
        let search_intercept = cx.intercept_keystrokes(search_listener);
        settings::initialize_disk_cache(cx);
        let settings_observer = cx.observe_global::<settings::SettingsStore>(|_, cx| {
            settings::sync_disk_cache(cx);
        });
        let disk_cache_observer = cx.observe_global::<DiskCache>(|_, cx| {
            cx.notify();
        });
        // The settings file is written on a background task, so the `Result` of
        // a click is not the result of the save. `SettingsSaveStatus` carries
        // the async outcome.
        let save_status_observer = cx.observe_global::<settings::SettingsSaveStatus>(|view, cx| {
            view.apply_save_status(cx);
        });
        let layout = layout_state(cx);
        let environment = SettingsEnvironment::read(cx);
        let environment_observer = cx.observe_global::<SettingsEnvironment>(|view, cx| {
            view.apply_environment(cx);
        });
        let save = settings::save_status(cx);
        let keymap_generation = keymap::status(cx).epoch;
        let keymap_poll = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(KEYMAP_POLL_INTERVAL).await;
                if view
                    .update(cx, |view, cx| view.refresh_keymap_generation(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            recording_focus: cx.focus_handle().tab_stop(false),
            search_focus,
            search_input,
            search_query: String::new(),
            reference: false,
            compact: false,
            nav_rail: false,
            nav_scroll: ScrollHandle::new(),
            measure_inset: px(0.),
            chrome_inset: px(0.),
            open_menu: None,
            menu: None,
            category: layout.category.unwrap_or(SettingsCategory::Appearance),
            scroll: ScrollHandle::new(),
            helm: environment.helm,
            metrics: environment.metrics,
            helm_error: environment.helm_error,
            metrics_error: environment.metrics_error,
            error: None,
            update_state: environment.update_state,
            update_actions: environment.update_actions,
            on_theme: None,
            on_create_or_show_keymap: None,
            on_reload_keymap: None,
            on_notice: None,
            on_close: None,
            owns_window: false,
            window_title: None,
            recording: None,
            recording_intercept: None,
            danger_step: DangerStep::Idle,
            save_pending: save.pending,
            save_error: save.error,
            keymap_generation,
            keyboard_sections: RefCell::new(None),
            text_synced: BTreeMap::new(),
            _keymap_poll: keymap_poll,
            _save_status_observer: save_status_observer,
            _settings_observer: settings_observer,
            _disk_cache_observer: disk_cache_observer,
            _environment_observer: environment_observer,
            _search_intercept: search_intercept,
        }
    }

    /// The window title the shell should show while this window is open.
    pub fn pane_title(&self) -> String {
        if self.reference {
            return "Settings · Shortcuts".to_owned();
        }
        if !self.search_query.trim().is_empty() {
            return "Settings · Search".to_owned();
        }
        format!("Settings · {}", self.category.label())
    }

    pub fn set_capabilities(
        &mut self,
        helm: HelmCapability,
        metrics: Capability,
        cx: &mut Context<Self>,
    ) {
        self.helm = helm;
        self.metrics = metrics;
        cx.notify();
    }

    pub fn set_capability_errors(
        &mut self,
        helm_error: Option<String>,
        metrics_error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.helm_error = helm_error;
        self.metrics_error = metrics_error;
        cx.notify();
    }

    pub fn set_theme_handler(
        &mut self,
        handler: impl Fn(ThemeChoice, &mut Window, &mut App) -> Result<(), String> + 'static,
    ) {
        self.on_theme = Some(Rc::new(handler));
    }

    pub fn set_keymap_handlers(
        &mut self,
        create_or_show: impl Fn(&mut Window, &mut App) + 'static,
        reload: impl Fn(&mut Window, &mut App) + 'static,
    ) {
        self.on_create_or_show_keymap = Some(Rc::new(create_or_show));
        self.on_reload_keymap = Some(Rc::new(reload));
    }

    pub fn set_notice_handler(&mut self, handler: impl Fn(String, Severity, &mut App) + 'static) {
        self.on_notice = Some(Rc::new(handler));
    }

    /// How this view's window is closed, when it has one.
    ///
    /// As a tab the close chord belongs to the shell, so Escape asks the shell to
    /// close a tab. As a window there is no tab, and `UI-SPEC` §9.3 is explicit
    /// that Escape must never do nothing, so the host says how to close the window
    /// instead. Without a handler the view asks the shell, which is what a tab
    /// needs and is the reason the fallback is not the other way round.
    pub fn set_close_handler(&mut self, handler: impl Fn(&mut Window, &mut App) + 'static) {
        self.on_close = Some(Rc::new(handler));
    }

    /// Say that this view has a window of its own, so the visible pane reaches
    /// the desktop's window list.
    ///
    /// A settings tab borrows the main window's title bar and the shell names the
    /// tab; a settings window is its own window, and Alt-Tab has to be able to
    /// tell "Settings · Keyboard" from a list of Pods. The view therefore names
    /// its own window only when a host says it owns one, and the shell's tab
    /// labelling is untouched when it does not.
    pub fn set_owns_window(&mut self) {
        self.owns_window = true;
    }

    pub fn set_update_state(&mut self, state: UpdateUiState, cx: &mut Context<Self>) {
        self.update_state = state.normalized();
        cx.notify();
    }

    pub fn set_update_actions(&mut self, actions: Option<UpdateActions>, cx: &mut Context<Self>) {
        self.update_actions = actions;
        if self.update_actions.is_none() {
            self.update_state = unsupported_update_state();
        }
        cx.notify();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.search_focus.clone()
    }

    // -- navigation --------------------------------------------------------

    /// Opens or closes the shortcut reference.
    ///
    /// `UI-REDESIGN` L13 binds `?` to it. The shipped keymap does not carry
    /// that binding, so this is the seam the shell needs: bind any action to a
    /// call to this method and the reference is reachable from wherever the
    /// reader is, which is the whole point of it.
    pub fn toggle_shortcut_reference(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.reference;
        self.reference = next;
        if next {
            self.category = SettingsCategory::Keyboard;
        }
        // The query goes with the list it was filtering. `cache` means one row
        // on one page and no command on the other, and a field that keeps
        // offering a reader nothing is worse than a field that starts empty.
        self.search_input
            .update(cx, |input, cx| input.set_text("", window, cx));
        self.set_search_query("", cx);
        self.reset_pane_state(cx);
        self.remember_layout(cx);
        if next {
            // Focus lands in the search box, because the first thing a reader
            // does on arriving at a list of sixty-eight commands is narrow it.
            let focus = self.search_focus.clone();
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Whether the shortcut reference is on screen.
    pub fn shortcut_reference_open(&self) -> bool {
        self.reference
    }

    fn set_search_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.search_query == query {
            return;
        }
        self.search_query = query.to_owned();
        self.reset_pane_state(cx);
        cx.notify();
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_text("", window, cx));
        self.set_search_query("", cx);
    }

    /// Returns the surface to the state a first visit has.
    ///
    /// A new query or a new page replaces the rows on screen, so everything
    /// that belonged to the rows that are gone goes with them. An armed danger
    /// step would otherwise leave a destructive button waiting for a second
    /// press on a page nobody is looking at, an open pop-up would float with no
    /// trigger under it, and the list would keep the scroll offset of a longer
    /// page and open halfway down a shorter one.
    fn reset_pane_state(&mut self, _cx: &mut App) {
        self.danger_step = DangerStep::Idle;
        self.open_menu = None;
        self.menu = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
    }

    fn select_category(&mut self, category: SettingsCategory, cx: &mut Context<Self>) {
        self.category = category;
        self.reference = false;
        // A category tab is "show me this page", not "forget what I searched
        // for", so the query stays and the results follow it.
        self.reset_pane_state(cx);
        self.remember_layout(cx);
        cx.notify();
    }

    fn remember_layout(&mut self, cx: &mut Context<Self>) {
        remember_layout(
            cx,
            SettingsLayout {
                category: Some(self.category),
            },
        );
    }

    // -- search ------------------------------------------------------------

    fn query(&self) -> String {
        self.search_query.trim().to_ascii_lowercase()
    }

    /// Whether a row's text answers the query.
    ///
    /// Every word, in any order, rather than the query as one run of characters.
    /// A settings box is a place people type two words into — `dark theme`,
    /// `cache size`, `log retention` — and the substring rule said no to all
    /// three unless the words happened to be typed in the order the row happens
    /// to print them, so `dark theme` found nothing at all on a screen with a
    /// Theme row on it. The empty state then says "No matching settings", which
    /// is worse than silence: it is a confident wrong answer about a setting the
    /// reader can see.
    ///
    /// Splitting rather than a fuzzy match on purpose. Every word has to be
    /// somewhere on the row, so a two-word query narrows and a typo in one word
    /// still finds nothing, which is the behaviour a filter box is expected to
    /// have and the one a reader can predict.
    fn matches(&self, haystack: &str) -> bool {
        let query = self.query();
        if query.is_empty() {
            return true;
        }
        let haystack = haystack.to_ascii_lowercase();
        query.split_whitespace().all(|word| haystack.contains(word))
    }

    /// Whether one row answers the query, by the words the row shows plus the
    /// extra words only the search box knows.
    fn setting_matches(&self, spec: &SettingSpec, value: &Value) -> bool {
        if self.query().is_empty() {
            return true;
        }
        // One haystack for the whole row, so a word in the title and a word in
        // the description narrow together: `motion accessibility` finds Reduce
        // motion, which is the query a reader types after not remembering which
        // of the two words is on screen.
        self.matches(&format!(
            "{} {} {} {} {}",
            spec.title,
            spec.description,
            spec.keywords,
            spec.category.label(),
            value.as_text()
        ))
    }

    /// The rows a query keeps, in draw order, with the value each one holds.
    fn results(&self, cx: &App) -> Vec<(Setting, Value)> {
        let stored = user_settings(cx);
        SPECS
            .iter()
            .filter_map(|spec| {
                let value = self.read(spec.setting, cx, &stored);
                self.setting_matches(spec, &value)
                    .then_some((spec.setting, value))
            })
            .collect()
    }

    /// The value one row shows, including the rows whose answer is a fact about
    /// the environment rather than a stored preference.
    fn read(&self, setting: Setting, cx: &App, stored: &UserSettings) -> Value {
        match setting {
            // A probe's verdict is the sentence and the reason is the tooltip.
            // `feedback.md` says status detail belongs in a disclosure, and a
            // control column with four lines of grey in it is a form, not a
            // status.
            Setting::Helm => Value::Text(Capability::from(self.helm).label().to_owned()),
            Setting::ClusterMetrics => Value::Text(self.metrics.label().to_owned()),
            Setting::Version => Value::Text(settings::app_version().to_owned()),
            Setting::CacheLocation => Value::Text(
                k8s_core::paths::cache_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Unavailable".to_owned()),
            ),
            Setting::CheckForUpdates => Value::Text(self.update_status()),
            other => read_setting(other, cx, stored),
        }
    }

    /// The pages a query keeps. A page with nothing to show is not offered, so
    /// the rail never names a page whose rows the filter already removed.
    fn matching_categories(&self, cx: &App) -> Vec<SettingsCategory> {
        let results = self.results(cx);
        SettingsCategory::ALL
            .into_iter()
            .filter(|category| {
                results
                    .iter()
                    .any(|(setting, _)| setting.category() == *category)
            })
            .collect()
    }

    /// The number the toolbar states while a query is filtering.
    ///
    /// A filtered list and an empty one look the same without a number, and it
    /// is the only thing the toolbar has left to say: the window title, the tab
    /// title and the page heading already name the page.
    fn result_count(&self, cx: &App) -> usize {
        self.results(cx).len()
    }

    // -- errors ------------------------------------------------------------

    fn notice(&self, message: String, severity: Severity, cx: &mut App) {
        if let Some(handler) = &self.on_notice {
            handler(message, severity, cx);
        }
    }

    fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn clear_error(&mut self) {
        self.error = None;
    }

    /// The reason the settings file could not be parsed, if it could not be.
    ///
    /// `settings::parse` answers a broken file with the built-in defaults, which
    /// is right for every surface that just needs *a* value and wrong for the
    /// one surface whose whole job is showing the reader what their file says.
    /// So the parse is repeated here, where a failure can be reported next to
    /// the values it produced, and the banner names what the app fell back to.
    fn settings_file_failure(&self, cx: &App) -> Option<String> {
        settings_file_failure(&raw_user_settings(cx)?)
    }

    fn apply_save_status(&mut self, cx: &mut Context<Self>) {
        let status = settings::save_status(cx);
        self.save_pending = status.pending;
        self.save_error = status.error;
        cx.notify();
    }

    /// Takes the session's facts from [`SettingsEnvironment`].
    ///
    /// The fields are written together rather than through the setters above,
    /// because the Updates page has one story: a state and the actions that can
    /// change it, published as a pair. Splitting them would let a repaint land
    /// between the two and show a state with nothing behind it.
    fn apply_environment(&mut self, cx: &mut Context<Self>) {
        let environment = SettingsEnvironment::read(cx);
        self.helm = environment.helm;
        self.metrics = environment.metrics;
        self.helm_error = environment.helm_error;
        self.metrics_error = environment.metrics_error;
        self.update_state = environment.update_state;
        self.update_actions = environment.update_actions;
        cx.notify();
    }

    /// Writes the settings file again after a write failed.
    ///
    /// It writes the value the file refused, not the value the store holds: a
    /// failed write puts the store back, so re-saving the store would write
    /// what the reader is trying to change away from and report success.
    fn retry_settings_save(&mut self, cx: &mut Context<Self>) {
        match settings::retry_rejected_save(cx) {
            Ok(()) => {
                self.notice("Saving settings again.".to_owned(), Severity::Info, cx);
            }
            Err(_error) => {
                self.set_error(
                    "The settings file still cannot be written. Check write access to it, then try again."
                        .to_owned(),
                );
            }
        }
        cx.notify();
    }

    fn refresh_keymap_generation(&mut self, cx: &mut Context<Self>) {
        let generation = keymap::status(cx).epoch;
        if generation == self.keymap_generation {
            return;
        }
        self.keymap_generation = generation;
        cx.notify();
    }

    // -- writing -----------------------------------------------------------

    /// Writes one setting and repaints, so there is no Apply button to forget.
    ///
    /// The write *is* the click. That is the whole of §15.2's fourth rule, and
    /// it is only possible because the settings store writes on a background
    /// task and publishes the outcome. The failure is the case that still needs
    /// a sentence, and it gets a banner and a row note rather than a button.
    fn write_setting(
        &mut self,
        setting: Setting,
        value: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(setting, value, Some(window), cx);
    }

    /// The same write for a control whose callback has no window to lend.
    ///
    /// Only the theme handler and the reference's focus move need one, and
    /// neither is on a control that answers with a window-less callback, so
    /// every other row takes this and the signature of `apply` stays honest
    /// about the one case that does.
    fn write_setting_without_window(
        &mut self,
        setting: Setting,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        self.commit(setting, value, None, cx);
    }

    fn commit(
        &mut self,
        setting: Setting,
        value: Value,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.clear_error();
        match self.apply(setting, &value, window, cx) {
            Ok(()) => {
                if setting.category() == SettingsCategory::Appearance {
                    // An appearance setting changes every surface behind this
                    // window, so the whole workbench repaints rather than the
                    // page the reader happens to be on.
                    cx.refresh_windows();
                }
                cx.notify();
            }
            Err(error) => {
                self.set_error(error);
                cx.notify();
            }
        }
    }

    fn apply(
        &mut self,
        setting: Setting,
        value: &Value,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        use Setting::*;
        match setting {
            Theme => {
                let Some(handler) = self.on_theme.clone() else {
                    return Err(
                        "Theme changes are not wired into this build. Check the theme registry, then try again."
                            .to_owned(),
                    );
                };
                let choice = if value.as_text() == ThemeChoice::System.label() {
                    ThemeChoice::System
                } else {
                    ThemeChoice::named(value.as_text())
                };
                match window {
                    Some(window) => handler(choice, window, cx),
                    None => Err(
                        "The theme can only be changed from the Theme control on this page."
                            .to_owned(),
                    ),
                }
            }
            TextSize => {
                let Some(size) = text_size_from_label(&value.as_text()) else {
                    return Err("Pick one of the offered text sizes.".to_owned());
                };
                settings::set_data_font_size(cx, size)
            }
            Contrast => match value {
                Value::Bool(enabled) => settings::set_increase_contrast(cx, *enabled),
                _ => Ok(()),
            },
            ReduceMotion => {
                let choice = ReduceMotionChoice::ALL
                    .into_iter()
                    .find(|choice| choice.label() == value.as_text())
                    .unwrap_or(ReduceMotionChoice::System);
                let mode = choice.mode();
                let result = settings::update(cx, |settings| settings.reduce_motion = mode);
                // `System` stores nothing, so there is no value for the settings
                // observer to install and the app has to put its own motion
                // setting back itself.
                cx.set_reduce_motion(choice.applied());
                result
            }
            KeymapPreset => {
                let preset = if value.as_text() == keymap::KeymapPreset::Vscode.label() {
                    keymap::KeymapPreset::Vscode
                } else {
                    keymap::KeymapPreset::Lens
                };
                keymap::set_preset(cx, preset).map_err(|error| {
                    format!(
                        "The keymap preset did not load: {error}. Check the preset file, then try again."
                    )
                })?;
                self.keymap_generation = keymap::status(cx).epoch;
                Ok(())
            }
            Cache => match value {
                Value::Bool(enabled) => {
                    settings::update(cx, |settings| settings.disk_cache = Some(*enabled))
                }
                _ => Ok(()),
            },
            ShortcutReference => {
                if let Some(window) = window {
                    self.toggle_shortcut_reference(window, cx);
                }
                Ok(())
            }
            CheckForUpdates => {
                self.check_for_updates(cx);
                Ok(())
            }
            Helm | ClusterMetrics | Version | CacheLocation => Ok(()),
            _ => {
                let Some(key) = setting.storage_key() else {
                    return Ok(());
                };
                let stored = match setting.control() {
                    Control::Switch => serde_json::Value::Bool(matches!(value, Value::Bool(true))),
                    Control::Number { .. } => {
                        let parsed = value.as_text().parse::<f64>().unwrap_or_default();
                        serde_json::Number::from_f64(parsed)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    }
                    _ => serde_json::Value::String(value.as_text()),
                };
                settings::update(cx, |settings| {
                    settings.extra.insert(key.to_owned(), stored);
                })
            }
        }
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(actions) = self.update_actions.as_ref() else {
            self.set_error(UPDATES_UNAVAILABLE_MESSAGE.to_owned());
            self.notice(
                UPDATES_UNAVAILABLE_MESSAGE.to_owned(),
                Severity::Warning,
                cx,
            );
            return;
        };
        let check = actions.check.clone();
        cx.defer(move |cx| check(cx));
    }

    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(actions) = self.update_actions.clone() else {
            self.set_error(UPDATES_UNAVAILABLE_MESSAGE.to_owned());
            cx.notify();
            return;
        };
        actions.run_restart(cx);
    }

    /// The one line the Updates page shows for the updater.
    fn update_status(&self) -> String {
        let unavailable = self.update_actions.is_none();
        let phase = if unavailable {
            UpdatePhase::Unsupported
        } else {
            self.update_state.phase
        };
        if phase == UpdatePhase::Unsupported {
            return UPDATES_UNAVAILABLE_MESSAGE.to_owned();
        }
        self.update_state.status_text()
    }

    fn update_severity(&self) -> Severity {
        if self.update_actions.is_none() {
            return Severity::Muted;
        }
        match self.update_state.phase {
            UpdatePhase::Ready | UpdatePhase::UpToDate => Severity::Success,
            UpdatePhase::Failed => Severity::Error,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting => {
                Severity::Info
            }
            UpdatePhase::Idle | UpdatePhase::Unsupported => Severity::Muted,
        }
    }

    /// The button a ready update needs, or nothing while the update is not ready.
    fn update_restart_label(&self) -> Option<&'static str> {
        (self.update_state.phase == UpdatePhase::Ready && self.update_actions.is_some())
            .then_some("Restart to update")
    }

    // -- danger zone -------------------------------------------------------

    fn on_danger(&mut self, which: DangerStep, window: &mut Window, cx: &mut Context<Self>) {
        if self.danger_step == which {
            match which {
                DangerStep::ClearCache => self.clear_cache(cx),
                DangerStep::ResetAll => self.reset_all(window, cx),
                DangerStep::Idle => {}
            }
        } else {
            self.danger_step = which;
        }
        cx.notify();
    }

    fn cancel_danger(&mut self, cx: &mut Context<Self>) {
        self.danger_step = DangerStep::Idle;
        cx.notify();
    }

    /// Deletes the cache folder.
    ///
    /// The removal runs on a background task because a cache with ten thousand
    /// snapshots is ten thousand unlinks, and the settings window is the one
    /// place a reader is most likely to be typing when it happens.
    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        self.danger_step = DangerStep::Idle;
        let Some(root) = k8s_core::paths::cache_dir() else {
            self.set_error(
                "The cache folder is unavailable on this system, so nothing was deleted."
                    .to_owned(),
            );
            return;
        };
        let view = cx.weak_entity();
        let task = cx
            .background_executor()
            .spawn(async move { std::fs::remove_dir_all(&root) });
        cx.spawn(async move |_view, cx| {
            let outcome = task.await;
            let _ = view.update(cx, |view, cx| {
                match outcome {
                    Ok(()) => {
                        view.clear_error();
                        view.notice("Cache cleared.".to_owned(), Severity::Info, cx);
                    }
                    Err(error) => view.set_error(format!(
                        "The cache folder was not deleted: {error}. Check write access to it, then try again."
                    )),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Puts every setting in this window back to its default.
    ///
    /// The appearance settings go back through their own handlers, because a
    /// reset that only emptied the file would leave the window looking the way
    /// the reader had just told it not to.
    fn reset_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.danger_step = DangerStep::Idle;
        if let Err(error) = settings::update(cx, |settings| *settings = UserSettings::default()) {
            self.set_error(format!(
                "Settings were not reset: {error}. Check write access to the settings file, then try again."
            ));
            return;
        }
        cx.set_reduce_motion(ReduceMotionChoice::System.applied());
        if let Some(handler) = self.on_theme.clone() {
            let _ = handler(ThemeChoice::System, window, cx);
        }
        // The user's own shortcut overrides are preferences too, and they live
        // in a different file. A reset that left them behind would say "every
        // preference in this window" and mean a subset, which is the one word
        // in that confirmation this screen must not get wrong.
        if let Err(error) = keymap::restore_defaults(cx) {
            self.set_error(format!(
                "Settings were reset, but the built-in shortcuts were not restored: {error}. Check write access to the keymap file, then try again."
            ));
        } else {
            self.keymap_generation = keymap::status(cx).epoch;
        }
        self.text_synced.clear();
        self.clear_error();
        // No success toast: every control in the window visibly moves back to
        // its default, which is the confirmation.
    }

    // -- shortcut recording ------------------------------------------------

    fn handle_keymap_update_result(
        &mut self,
        result: Result<keymap::UserBindingUpdate, String>,
        success_message: &'static str,
        not_applied_message: &'static str,
        failure_message: &'static str,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(keymap::UserBindingUpdate::SavedAndApplied) => {
                self.clear_error();
                self.notice(success_message.to_owned(), Severity::Info, cx);
            }
            Ok(keymap::UserBindingUpdate::SavedNotApplied) => {
                self.set_error(not_applied_message.to_owned());
            }
            Err(_error) => {
                self.set_error(failure_message.to_owned());
            }
        }
    }

    fn handle_keybinding_update(
        &mut self,
        command: &KeyboardCommand,
        result: Result<keymap::UserBindingUpdate, String>,
        success_message: &'static str,
        not_applied_message: &'static str,
        failure_message: &'static str,
        cx: &mut Context<Self>,
    ) {
        if matches!(result, Ok(keymap::UserBindingUpdate::SavedAndApplied))
            && let Some(conflict) = conflict_for_command(command, cx)
        {
            self.set_error(format!(
                "The shortcut was saved, but {label} Change one of them, then record the shortcut again.",
                label = conflict.label()
            ));
            return;
        }
        self.handle_keymap_update_result(
            result,
            success_message,
            not_applied_message,
            failure_message,
            cx,
        );
    }

    fn start_recording(
        &mut self,
        command: KeyboardCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(window, cx);
        self.clear_error();
        self.danger_step = DangerStep::Idle;
        let previous_focus = window.focused(cx);
        self.recording = Some(Recording {
            command,
            previous_focus,
        });
        let listener = cx.listener(|view, event: &KeystrokeEvent, window, cx| {
            view.handle_recording_keystroke(&event.keystroke, window, cx);
        });
        self.recording_intercept = Some(cx.intercept_keystrokes(listener));
        window.focus(&self.recording_focus, cx);
        cx.notify();
    }

    fn stop_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous_focus = self
            .recording
            .take()
            .and_then(|recording| recording.previous_focus);
        self.recording_intercept.take();
        if let Some(previous_focus) = previous_focus {
            window.focus(&previous_focus, cx);
        }
        cx.notify();
    }

    fn handle_recording_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(recording) = self.recording.clone() else {
            return;
        };
        cx.stop_propagation();
        if keystroke.key == "escape" {
            self.recording = None;
            self.recording_intercept.take();
            self.clear_error();
            if let Some(previous_focus) = recording.previous_focus {
                window.focus(&previous_focus, cx);
            }
            cx.notify();
            return;
        }
        if matches!(
            keystroke.key.as_str(),
            "" | "shift" | "control" | "alt" | "platform" | "function"
        ) {
            return;
        }
        if !recording_keystroke_allowed(keystroke) {
            self.set_error(format!(
                "Use a modifier with the key, or press F2–F24 on its own. {RECORDING_REJECTED_KEYS} cannot be recorded."
            ));
            cx.notify();
            return;
        }
        self.recording = None;
        self.recording_intercept.take();
        let result = action_for_command(&recording.command, cx).and_then(|action| {
            keymap::update_user_binding_with_outcome(
                action.as_ref(),
                recording.command.action_input.as_deref(),
                recording.command.context.as_deref(),
                Some(keystroke),
                cx,
            )
        });
        if let Some(previous_focus) = recording.previous_focus {
            window.focus(&previous_focus, cx);
        }
        self.handle_keybinding_update(
            &recording.command,
            result,
            "Keyboard shortcut saved.",
            "The shortcut was saved, but it is not active. Fix the keymap file, then reload the keymap.",
            "The shortcut was not saved. Check write access to the keymap file, then record the shortcut again.",
            cx,
        );
        cx.notify();
    }

    fn clear_binding(&mut self, command: &KeyboardCommand, cx: &mut Context<Self>) {
        let result = action_for_command(command, cx).and_then(|action| {
            keymap::update_user_binding_with_outcome(
                action.as_ref(),
                command.action_input.as_deref(),
                command.context.as_deref(),
                None,
                cx,
            )
        });
        self.handle_keybinding_update(
            command,
            result,
            "Keyboard shortcut cleared.",
            "The shortcut was cleared in the keymap, but it is still active. Fix the keymap file, then reload the keymap.",
            "The shortcut was not cleared. Check write access to the keymap file, then select Clear again.",
            cx,
        );
        cx.notify();
    }

    fn copy_keymap_path(&self, cx: &mut App) {
        let Some(path) = keymap::user_keymap_path() else {
            self.notice(
                "The keymap file path is unavailable. Check access to the user configuration directory."
                    .to_owned(),
                Severity::Warning,
                cx,
            );
            return;
        };
        let text = path.display().to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.notice(format!("Copied {text}."), Severity::Info, cx);
    }

    /// Opens a file in the platform's own file manager.
    ///
    /// The settings file and the cache folder are both places a reader has to go
    /// outside the app to do anything about, and the platform opener is the only
    /// way to get them there. `keymap::default_file_opener` already names the
    /// three platforms' openers, so this reuses it rather than re-listing them.
    fn reveal(path: &std::path::Path) -> Result<(), String> {
        std::process::Command::new(keymap::default_file_opener())
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "Cannot open {}: {error}. Check the opener path, then try again.",
                    path.display()
                )
            })
    }

    /// The command list the reference draws, rebuilt only when an input it
    /// reads has moved.
    fn keyboard_sections(&self, cx: &App) -> Rc<[KeyboardSection]> {
        let keymap_generation = keymap::status(cx).epoch;
        let updater_available = self.update_actions.is_some();
        let action_count = cx.all_action_names().len();
        if let Some(cached) = self.keyboard_sections.borrow().as_ref()
            && cached.keymap_generation == keymap_generation
            && cached.updater_available == updater_available
            && cached.action_count == action_count
        {
            return cached.sections.clone();
        }
        let sections = keyboard_sections(cx, updater_available)
            .into_iter()
            .collect::<Rc<[KeyboardSection]>>();
        *self.keyboard_sections.borrow_mut() = Some(KeyboardSections {
            keymap_generation,
            updater_available,
            action_count,
            sections: sections.clone(),
        });
        sections
    }

    /// Escape, on the window rather than on one control.
    ///
    /// §9.3's rule is that Escape always has an effect, and it used to be true
    /// only in two places: an armed danger step, and the search field. With
    /// focus on a category tab, on a switch, on a drop-down, or on nothing at
    /// all, Escape did nothing whatsoever — the exact failure the rule names.
    ///
    /// So the ladder lives here, on the surface itself, and unwinds one layer per
    /// press in the order a reader expects: an open menu, then an armed step,
    /// then a recording, then the reference, then the query, then out of the
    /// window. The search field keeps its own interceptor for the one thing only
    /// it can do — cancel an in-flight text composition — and this one runs when
    /// that has nothing to cancel.
    fn on_settings_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "escape" || event.keystroke.modifiers.modified() {
            return;
        }
        if self.open_menu.take().is_some() {
            self.menu = None;
            cx.stop_propagation();
            self.reset_pane_state(cx);
            cx.notify();
            return;
        }
        if self.danger_step.armed() {
            cx.stop_propagation();
            self.danger_step = DangerStep::Idle;
            cx.notify();
            return;
        }
        if self.recording.is_some() {
            cx.stop_propagation();
            self.stop_recording(window, cx);
            return;
        }
        if self.reference {
            cx.stop_propagation();
            self.toggle_shortcut_reference(window, cx);
            return;
        }
        if !self.search_query.trim().is_empty() {
            cx.stop_propagation();
            self.clear_search(window, cx);
            return;
        }
        // Nothing left on the surface to unwind, so the press belongs to the
        // window. A host that owns one says how to close it; a tab asks the
        // shell, which is the reason the fallback is not the other way round.
        if let Some(close) = self.on_close.clone() {
            cx.stop_propagation();
            close(window, cx);
        } else {
            cx.stop_propagation();
            window.dispatch_action(Box::new(crate::shell::CloseTab), cx);
        }
    }

    /// Escape, while the search field holds focus.
    ///
    /// A keystroke interceptor, not a key handler, and it has to be one: the
    /// field's own `Escape` action reverts its text and then stops propagation,
    /// so a handler on the surface would never see the press. The field owns
    /// exactly one step of the ladder — clearing the query — and the two layers
    /// above it are checked here too, because a reader who opened a menu with the
    /// keyboard and then pressed Escape should not have the query cleared
    /// instead.
    fn handle_search_escape(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keystroke.key != "escape" || keystroke.modifiers.modified() {
            return;
        }
        if !self.search_focus.is_focused(window) {
            return;
        }
        if self.open_menu.take().is_some() {
            self.menu = None;
            self.reset_pane_state(cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.danger_step.armed() {
            self.danger_step = DangerStep::Idle;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        cx.stop_propagation();
        if !self.search_query.trim().is_empty() {
            self.clear_search(window, cx);
        } else if self.reference {
            self.toggle_shortcut_reference(window, cx);
        } else if let Some(close) = self.on_close.clone() {
            // A window closes itself; there is no tab for the shell to close.
            close(window, cx);
        } else {
            window.dispatch_action(Box::new(crate::shell::CloseTab), cx);
        }
    }

    /// Answers the reference's own action, wherever the reference is drawn.
    ///
    /// The action is the seam `UI-REDESIGN` L13 asks for and the keymap can name.
    /// Dispatched inside the surface, it answers a press that arrived here; the
    /// shell routes the same action to the window when the press arrived in a
    /// surface that has no reference of its own.
    fn on_open_shortcut_reference(
        &mut self,
        _: &OpenShortcutReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_shortcut_reference(window, cx);
    }
}

/// The one search field.
///
/// The app's own `TextInput`, so it answers Escape the same way every other
/// field in the product does, carries a clear button, and keeps the focus handle
/// that the state it creates on its first frame owns. A field that rolled its
/// own would drift from the rest of the app on the first state it did not
/// remember to implement.
fn new_search_field(cx: &mut Context<SettingsView>) -> Entity<TextInput> {
    let view = cx.weak_entity();
    cx.new(|cx| {
        TextInput::new(SETTINGS_PLACEHOLDER, cx, move |text, cx| {
            let view = view.clone();
            let text = text.to_owned();
            cx.defer(move |cx| {
                let _ = view.update(cx, |view, cx| view.set_search_query(&text, cx));
            })
        })
        .with_accessibility(
            "Search settings",
            "Enter a name, a description or a category to filter. Press Escape to clear the search.",
            "Clear settings search",
        )
        .with_width(settings_search_width() - space::LG * 2.)
    })
}

/// Why the settings file could not be read, or `None` when it could.
///
/// The reason is the parser's own, so the reader sees the line and the column
/// rather than "something is wrong with your settings".
fn settings_file_failure(raw: &str) -> Option<String> {
    settings::parse_jsonc::<UserSettings>(raw).err()
}

/// The placeholder one text field needs, or nothing.
///
/// A field that starts empty with no placeholder is a box, not an answer: the
/// default namespace is empty, and the only thing on screen that said "empty
/// means all namespaces" was a tooltip. A placeholder says it where the reader
/// is looking, and disappears the moment there is something to read instead.
fn field_placeholder(setting: Setting) -> Option<&'static str> {
    match setting {
        Setting::DefaultNamespace => Some("All namespaces"),
        _ => None,
    }
}

fn text_size_from_label(label: &str) -> Option<f32> {
    let digits: String = label
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    let size = digits.parse::<f32>().ok()?;
    DATA_FONT_SIZES
        .iter()
        .copied()
        .find(|candidate| *candidate == size)
}

// ---------------------------------------------------------------------------
// The text and number field
// ---------------------------------------------------------------------------

/// The editing state behind one text or number row.
///
/// gpui-kit's `InputState` owns stepping, bounds and clamping: `+`/`-` and
/// Up/Down step by `step`, and out-of-range text is tolerated while typing and
/// clamped on blur. Re-implementing either on a `Change` event breaks both, so
/// the row configures the engine and writes the setting when the text parses.
///
/// The one thing the engine does not know is where in the tab order this row
/// sits, and the handle it publishes is stamped here — the same thing the app's
/// own `TextInput` wrapper does, for the same reason.
struct FieldState {
    input: Entity<InputState>,
    digits_only: bool,
    min: f64,
    max: f64,
    /// The text the engine is believed to hold, so an external change writes
    /// once instead of on every frame.
    written: String,
    /// Whether the reader has changed the field and the change has not landed
    /// yet. A text field commits on blur or Enter, so a half-typed namespace is
    /// not a value anybody asked to store, and a sync must not reach in and
    /// finish the word for them.
    dirty: bool,
    /// The tab slot the handle was last stamped with, and whether the row was a
    /// tab stop at the time. An inert row is not a stop, so the stamp has to
    /// carry the pair: stamping the slot without the stop is how a control
    /// nothing reads stays reachable by Tab.
    stamped: (isize, bool),
    /// Whether the row's answer is read by anything. A field on an inert row
    /// drops its writes here as well as on the control, because the engine can
    /// still be driven by a key the platform delivers to the window.
    enabled: bool,
    _subscription: Subscription,
}

impl FieldState {
    #[expect(clippy::too_many_arguments)]
    fn new(
        value: &str,
        spec: Control,
        tab_index: isize,
        enabled: bool,
        view: WeakEntity<SettingsView>,
        setting: Setting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (digits_only, min, max, step) = match spec {
            Control::Number { min, max, step, .. } => (true, min, max, step),
            _ => (false, f64::MIN, f64::MAX, 1.),
        };
        let initial = value.to_owned();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .default_value(initial.clone())
                .step(step)
                .min(min)
                .max(max);
            state.set_clean_on_escape(true);
            state
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |field: &mut Self, input, event: &InputEvent, window, cx| {
                // A number is a value from the first digit, so it commits as it
                // is typed and a stepper's `+` lands immediately. Text is a word
                // being typed, so it commits when the field loses focus or takes
                // Enter — a settings file rewritten on every keystroke is a
                // settings file being written while the reader is thinking.
                let commit = match event {
                    InputEvent::Change if field.digits_only => true,
                    InputEvent::Blur | InputEvent::PressEnter { .. } => true,
                    _ => false,
                };
                if !field.enabled {
                    // The control is disabled, so nothing should have reached the
                    // engine. A stepper's `+` on a disabled `InputState`, or a
                    // paste delivered to the window, still would, and writing
                    // from here is the last place that can refuse.
                    return;
                }
                if !commit {
                    if matches!(event, InputEvent::Change) {
                        field.dirty = true;
                    }
                    return;
                }
                let text = input.read(cx).value().to_string();
                if text == field.written {
                    field.dirty = false;
                    return;
                }
                field.dirty = false;
                let value = if field.digits_only {
                    // An intermediate like "-" or "1." cannot be read yet, so it
                    // is left for the next keystroke rather than written as
                    // zero. The engine clamps on blur.
                    let Ok(parsed) = text.parse::<f64>() else {
                        return;
                    };
                    Value::Number(parsed.clamp(field.min, field.max))
                } else {
                    Value::Text(text)
                };
                field.written = match &value {
                    Value::Number(number) => {
                        if number.fract() == 0.0 {
                            format!("{number:.0}")
                        } else {
                            number.to_string()
                        }
                    }
                    Value::Text(text) => text.clone(),
                    _ => String::new(),
                };
                let view = view.clone();
                let _ = view.update(cx, |view, cx| {
                    view.write_setting(setting, value, window, cx);
                });
            },
        );
        Self {
            input,
            digits_only,
            min,
            max,
            written: initial,
            dirty: false,
            stamped: (tab_index, enabled),
            enabled,
            _subscription: subscription,
        }
    }

    /// Puts the engine's text, the row's tab slot and the row's live state back
    /// in step with the file.
    fn sync(
        &mut self,
        value: &str,
        tab_index: isize,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enabled = enabled;
        if !self.dirty && value != self.written {
            self.written = value.to_owned();
            self.input.update(cx, |input, cx| {
                input.set_value(SharedString::from(value), window, cx)
            });
        }
        if self.stamped != (tab_index, enabled) {
            // `InputState` publishes its own handle, and stamping the clone
            // writes the slot into the focus registry that owns it. That is the
            // same seam the app's own `TextInput` wrapper uses for its field.
            self.stamped = (tab_index, enabled);
            let _ = self
                .input
                .read(cx)
                .focus_handle(cx)
                .tab_index(tab_index)
                .tab_stop(enabled);
        }
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// One band of the page, held to the content measure and placed at the content
/// column's leading edge.
///
/// The inset the caller passes is that leading edge, read once per frame from
/// the window's width, and the band's contents are capped at the measure on the
/// *inside* of it. So with the rail up a band starts over the rail's own width
/// and runs to the end of the measure; without it the window is narrow enough
/// that the same number centres the measure, which is the right answer there
/// because a capped column pinned to one edge of a window wider than itself
/// reads as content that failed to fill the window.
///
/// The bands that carry a background or a hairline keep it on *this* element's
/// full width — chrome belongs to the window, the document belongs to the
/// measure — and only their contents are capped. A hairline under a 640px tab
/// strip floating in the middle of a wide window would be a line drawn around
/// nothing.
fn measure_band(inset: gpui_kit::Pixels) -> Div {
    div()
        .w_full()
        .px(inset)
        .child(div().w_full().max_w(px(SETTINGS_CONTENT_MAX_WIDTH)))
}

/// A push button whose plane, ink, hover and press all come from `design`.
///
/// gpui-kit's own variants resolve those four from the component theme's
/// `button`, `button_secondary` and `muted_foreground` tokens, and this product's
/// theme file does not map any of them — so on this surface `ghost()` and
/// `secondary()` both resolved to bare text, which is how two destructive
/// commands ended up looking like two links. A custom variant takes all four
/// from the role layer instead, so a button here is the product's own
/// `surface.raised` and moves with the theme.
///
/// `framed` adds the one stroke this product allows a control to carry and the
/// spec reserves for fields, overlays and dividers. It is set on a drop-down
/// trigger — a field that answers with a choice — and left off a push button,
/// which is the arrangement `Interface language` asks for: a `Button` is an
/// application action and an outlined box is a control the reader fills in.
///
/// `radius::MD` because that is the product's radius for a button and a card,
/// and it is what the segmented track beside it is drawn with: the two kinds of
/// control share one column on every page, and a `radius::SM` button under a
/// `radius::MD` track is two corners a reader can see differ without being able
/// to say why.
fn token_button(id: impl Into<gpui_kit::ElementId>, label: &str, framed: bool, cx: &App) -> Button {
    let plane = design::role::surface_raised(cx);
    let ink = design::role::fg_primary(cx);
    let button = Button::new(id)
        .label(label.to_owned())
        .with_size(Size::Size(design::size::CONTROL))
        .rounded(design::radius::MD)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(plane)
                .foreground(ink)
                .hover(design::state::hover_on(plane, ink))
                .active(design::state::press_on(plane, ink)),
        );
    if framed { button.outline() } else { button }
}

/// The one push button on this surface whose press is irreversible, at rest.
///
/// The same plate as every other button here, in `danger_word` instead of
/// `fg.primary`, with its hover and press mixed from *that* ink. It is the guide's
/// "destructive styling applied to the destructive result" taken literally: the
/// reader finds out which of two quiet buttons deletes their settings by reading
/// the two words, not by finding out afterwards. The caller replaces it with
/// `.danger()` once the step is armed, because the fill is what the second press
/// asks for and only that press has been promised by a caption naming exactly what
/// it will delete.
fn destructive_button(id: impl Into<gpui_kit::ElementId>, label: &str, cx: &App) -> Button {
    let plane = design::role::surface_raised(cx);
    let ink = design::role::danger_word(cx);
    Button::new(id)
        .label(label.to_owned())
        .with_size(Size::Size(design::size::CONTROL))
        .rounded(design::radius::MD)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(plane)
                .foreground(ink)
                .hover(design::state::hover_on(plane, ink))
                .active(design::state::press_on(plane, ink)),
        )
}

/// The title, one line of description, and the optional group eyebrow of one row.
///
/// The stack is the row's hierarchy and the gaps inside it are what make it
/// legible: `space::XS` between a title and its own description, against the
/// 16px the row's own padding puts between one row's description and the next
/// row's title. `Vertical rhythm shows grouping` is a number, not an intention —
/// equal gaps would say a description belongs to the row below it as much as to
/// the row it names.
///
/// `note` is the pending reason, and it *replaces* the hint rather than joining
/// it: a row whose answer is stored and not read has two things to say, and the
/// one that matters is why the control beside it is off. The hint is one hover
/// away and in the search results, so nothing is lost.
///
/// The stack is flexible so the keymap block, which owns a path and three
/// controls instead of one, can use it at the page's full width. The two-column
/// rows narrow it to the shared label column instead.
fn setting_row_label(
    title: impl Into<SharedString>,
    note: Option<&str>,
    hint: Option<&str>,
    category: Option<&str>,
    compact: bool,
    cx: &Context<SettingsView>,
) -> Div {
    let mut label = v_flex().flex_1().min_w(px(0.)).gap(space::XS);
    // In a search the reader is scanning hits from six pages at once, so each
    // row says which page it came from. `caption` is uppercased here because
    // this is a group label in all but name; §2.3 allows it in section titles
    // and nowhere else, and this is one.
    label = label.when_some(category, |label, category| {
        let text = category.to_ascii_uppercase();
        label.child(
            Label::new(text)
                .text_size(design::text::CAPTION)
                .line_height(design::text::CAPTION_LINE_HEIGHT)
                .font_weight(design::text::SEMIBOLD)
                .text_color(design::role::fg_tertiary(cx)),
        )
    });
    // `body 13/medium` in `fg.primary`: the name of the thing, at the weight a
    // label carries rather than the weight body copy carries. It used to be
    // `label_body`'s 400, which made a row's title and its description two
    // weights of the same size in the same voice — a form with no voice.
    label = label.child(
        label_body(title)
            .font_weight(design::text::MEDIUM)
            .text_color(design::role::fg_primary(cx)),
    );
    // Exactly one line under the title, always. A row whose answer is stored and
    // not read says *that* instead, because it is the two facts a reader of that
    // row needs: what the control is called, and why it is off. It is not an
    // error and it does not get an error's colour — the setting is saved, and the
    // reader asked a question this screen can answer.
    //
    // `text::LABEL` rather than a caption: this is help text a reader reads to
    // decide, not a count, and at 11px it was the smallest type on the surface
    // carrying a full sentence. The inert line is drawn a step quieter than a
    // live hint rather than in a different colour, so a page is readable as two
    // kinds of row without spending a hue on the difference. Every one of these
    // sentences is written to fit the label column on one line: a page whose rows
    // are four different heights is a page of twenty rows, not eight.
    let (line, inert) = match note {
        Some(note) => (Some(note), true),
        None => (hint, false),
    };
    label = label.when_some(line, |label, line| {
        let color = if inert {
            design::role::fg_tertiary(cx)
        } else {
            design::role::fg_secondary(cx)
        };
        label.child(
            Label::new(line.to_owned())
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .text_color(color),
        )
    });
    if compact { label.w_full() } else { label }
}

/// The value column of a settings row.
///
/// The column holds the measure's trailing edge: the row gives it a fixed width
/// and the leftover space to a spacer, so every control ends on the same line,
/// the way a desktop settings pane reads. Its children are sized by their
/// content, so a segmented control and a keycap both sit flush against that
/// line instead of leaving a ragged gap before it.
fn setting_control(compact: bool) -> Div {
    let control = h_flex()
        .min_w(px(0.))
        .min_h(design::size::CONTROL)
        .flex_none()
        .justify_end()
        .items_center()
        .gap(space::SM);
    if compact {
        control.w_full().justify_start()
    } else {
        control.w(px(SETTINGS_CONTROL_WIDTH))
    }
}

/// The group heading inside a tab: a quiet uppercase label, and the one line
/// that says what the group is for.
///
/// One treatment for every group on the surface, and the surface's own section
/// scale rather than a heading size: `text::CAPTION`, uppercased and semibold in
/// `fg.tertiary`, which is the level §1.4 gives to a group head and the level
/// `Interface language` allows uppercase at. It used to be the title role's
/// 15px semibold in `fg.primary` — a page-title step — so eleven group heads in
/// the reference were the loudest thing on a screen whose real page name is in
/// the title bar and the navigation beside it, and the type scale had nothing
/// between a group head and a window title.
///
/// The step down is carried by size, weight and ink rather than by tracking:
/// gpui's text system has no letter-spacing, and a tracked-out eyebrow the
/// engine cannot draw is a description of a treatment this surface does not have.
///
/// It carries no rule of its own. The chrome plane above it already ends in one
/// — the category strip's hairline, or the search line's with the rail up — and a
/// second hairline thirty pixels lower says nothing the row of names has not
/// already said — it just puts two parallel lines on the screen where one would
/// do.
///
/// An empty description draws no second line at all. A group inside a page is
/// named by its title and nothing else: the rows under it already say where
/// their own shortcuts are live, so a sentence repeated above every group is
/// the same sentence eleven times, and once it is wrong — "application
/// commands" printed over the Inspector section, whose commands are live only
/// when the Inspector is — a reader who trusts it is worse off than one who
/// read nothing.
///
/// `lead` is the gap between whatever is above and this head, and it is the one
/// number that differs by position rather than by group: a page's first group
/// sits under a hairline the page has to clear rather than under another group,
/// and every group after it — on a settings page and in the reference alike — is
/// a run inside one long list.
fn section_header(
    title: &str,
    description: &str,
    lead: gpui_kit::Pixels,
    cx: &Context<SettingsView>,
) -> AnyElement {
    let heading = format!("settings-heading-{}", slug(title));
    let id = format!("settings-section-{}", slug(title));
    // 24 above and 8 below for a group's own gap, so a group's rows start 8px
    // after its label and the next group's label starts 32px after this group's
    // last row: the gap that says "a new group" is four times the gap that says
    // "the next row of this one", which is the same ratio the row grid uses
    // inside a row. The first head on a page takes `lead` instead, because it is
    // clearing the chrome plane's rule rather than another group.
    h_flex()
        .id(id)
        .w_full()
        .px(SETTINGS_PAGE_INSET)
        .pt(lead)
        .pb(space::SM)
        .gap(space::MD)
        .items_center()
        .child(
            v_flex()
                .id(heading.clone())
                // The heading is the only place a page names its own groups, so it
                // is the element a test looks for: `debug_bounds` reads selectors,
                // never element ids.
                .debug_selector(move || heading)
                .flex_1()
                .min_w(px(0.))
                .gap(space::XS)
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(title.to_owned())
                .child(
                    Label::new(title.to_ascii_uppercase())
                        .text_size(design::text::CAPTION)
                        .line_height(design::text::CAPTION_LINE_HEIGHT)
                        .font_weight(design::text::SEMIBOLD)
                        .text_color(design::role::fg_tertiary(cx)),
                )
                .when(!description.is_empty(), |heading| {
                    heading.child(
                        Label::new(description.to_owned())
                            .text_size(design::text::LABEL)
                            .line_height(design::text::LABEL_LINE_HEIGHT)
                            .text_color(design::role::fg_tertiary(cx)),
                    )
                }),
        )
        .into_any_element()
}

/// A category page's own group head.
///
/// [`section_header`] with the one difference a page needs and the reference
/// does not: the first group of a page clears the chrome plane's hairline by the
/// page's own padding, and only a group that follows another group gets the full
/// section gap. Drawing `space::XL` above the first head as well would put a
/// second 24px band of nothing under a rule that already ended the chrome, and
/// the eight rows of a page are exactly the kind of budget that does not have 12
/// spare pixels in it.
///
/// No description: a row's own second line says what the setting is, and a
/// sentence above a group would say the same thing about four rows at once.
fn page_section_head(title: &str, first: bool, cx: &Context<SettingsView>) -> AnyElement {
    section_header(title, "", if first { space::MD } else { space::XL }, cx)
}

/// True when the two-column row cannot hold its label and its fixed control
/// column.
///
/// The floor is the arithmetic, not the window's minimum width, and that is the
/// whole correction. The two were the same number by accident, and the accident
/// put the switch nineteen pixels under the window's own minimum: a window
/// tiled to 941 of a declared 960 fell past the test, every row turned its label
/// above its control, and the form became a phone settings screen — twice the
/// height, a third of the scanability, and no alignment to speak of. A window
/// manager is entitled to hand out a window nineteen pixels narrower than the
/// one that was asked for, and the layout has to survive that rather than
/// bet that it will not happen.
///
/// So the stacked form is what a window narrower than the label column, the
/// control column and the gutters actually add up to needs, which is where it is
/// the only form that fits. Above that the two columns hold, at the window's
/// minimum and well past it. The arithmetic reads the *page's* inset rather than
/// a copied `space::LG`, so a change to [`SETTINGS_PAGE_INSET`] moves the floor
/// with it instead of leaving the two disagreeing.
///
/// `nav_rail` is the rail's own width, subtracted because the row grid has to fit
/// in what is left of the window rather than in the window: at the rail's own
/// 856px threshold the pane beside it is the measure exactly, and a form that
/// asked the window instead drew a two-column row wider than the column holding
/// it.
fn compact_settings_layout(viewport_width: f32, nav_rail: bool) -> bool {
    let rail = if nav_rail { SETTINGS_NAV_WIDTH } else { 0. };
    let row_floor = SETTINGS_LABEL_WIDTH
        + SETTINGS_CONTROL_WIDTH
        + f32::from(SETTINGS_PAGE_INSET) * 2.
        + f32::from(space::XL);
    viewport_width - rail < row_floor
}

impl SettingsView {
    fn status_banner(
        &self,
        id: &'static str,
        text: String,
        reason: Option<String>,
        action: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The action sits on the trailing side of the capped measure, so the
        // sentence reads first and the recovery follows it instead of competing
        // with it for the eye. `feedback.md` asks for an error as close to the
        // problem as it can be, and a `w_full` row with a spacer in the middle
        // put `Retry` 1888px from the promise on a wide window. The banner is
        // capped on the *outer* box and starts on the measure's leading edge
        // like every other band, so the recovery lands on the same spine as the
        // row it belongs to.
        let mut line = h_flex()
            .w_full()
            .py(space::SM)
            .gap(space::SM)
            .items_center()
            // The sentence is allowed to wrap and the recovery control is allowed
            // to drop below it. A single-line banner at the page's own measure is
            // 608px of room for an 81-character sentence, a glyph and a button,
            // and the sentence does not fit: it ran under the alert's trailing
            // edge and put `Retry` outside the region it belongs to. Wrapping is
            // the arrangement `Alignment details` asks for — the sentence reads
            // first and the recovery follows it, on the next line when the
            // measure says so rather than off the end of the band.
            .flex_wrap()
            .child(
                Icon::new(IconName::TriangleAlert)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    .text_color(design::role::danger(cx)),
            )
            .child(
                Label::new(text.clone())
                    .text_size(design::text::LABEL)
                    .line_height(design::text::LABEL_LINE_HEIGHT)
                    .flex_1()
                    .min_w(px(0.))
                    .text_color(design::role::danger_word(cx)),
            );
        line = line.when_some(action, |this, action| this.child(action));
        let mut alert = h_flex()
            .id(id)
            .debug_selector(move || id.to_owned())
            .w_full()
            .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
            .flex_none()
            .px(SETTINGS_PAGE_INSET)
            .role(Role::Alert)
            .aria_label(text)
            .child(line);
        if let Some(reason) = reason {
            alert = alert.aria_description(reason.clone());
            alert = alert.tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx));
        }
        measure_band(self.chrome_inset)
            .child(alert)
            .into_any_element()
    }

    /// The strip between the toolbar and the rows.
    ///
    /// It lives outside the scroll container so a failure cannot scroll out of
    /// sight, and it shows the pending write as well as the failed one: the
    /// click that started the write returns before the file is touched, so
    /// "Saving…" is the honest state in between.
    fn render_status_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(reason) = self.save_error.clone() {
            let retry = div()
                .flex_none()
                .debug_selector(|| "settings-save-retry".to_owned())
                .child(
                    token_button("settings-save-retry", RETRY_LABEL, false, cx)
                        .h(design::size::CONTROL)
                        .accessibility_label("Write the settings file again")
                        .tooltip("Save the settings file again.")
                        .on_click(cx.listener(|view, _, _, cx| view.retry_settings_save(cx))),
                );
            return Some(
                self.status_banner(
                    "settings-save-error",
                    SAVE_FAILED_MESSAGE.to_owned(),
                    Some(reason),
                    Some(retry.into_any_element()),
                    cx,
                )
                .into_any_element(),
            );
        }
        if let Some(reason) = self.settings_file_failure(cx) {
            let open = div().flex_none().child(
                token_button("settings-file-open", SETTINGS_FILE_OPEN_LABEL, false, cx)
                    .h(design::size::CONTROL)
                    .accessibility_label("Open the settings file")
                    .tooltip("Open the settings file in the platform's file manager.")
                    .on_click(cx.listener(|_, _, _, _| {
                        if let Some(path) = settings::user_settings_path() {
                            let _ = SettingsView::reveal(&path);
                        }
                    })),
            );
            return Some(
                self.status_banner(
                    "settings-file-unreadable",
                    SETTINGS_FILE_UNREADABLE.to_owned(),
                    Some(reason),
                    Some(open.into_any_element()),
                    cx,
                )
                .into_any_element(),
            );
        }
        if let Some(error) = self.error.clone() {
            return Some(
                self.status_banner("settings-error", error, None, None, cx)
                    .into_any_element(),
            );
        }
        if !self.save_pending {
            return None;
        }
        Some(
            measure_band(self.chrome_inset)
                .child(
                    h_flex()
                        .id("settings-save-pending")
                        .debug_selector(|| "settings-save-pending".to_owned())
                        .w_full()
                        .flex_none()
                        .px(SETTINGS_PAGE_INSET)
                        .py(space::SM)
                        .gap(space::SM)
                        .items_center()
                        .role(Role::Status)
                        .aria_label("Saving settings.")
                        .child(spinner(
                            IconName::LoaderCircle,
                            design::role::fg_tertiary(cx),
                            Size::Size(design::icon::IN_ROW),
                        ))
                        .child(
                            Label::new(SAVING_LABEL)
                                .text_size(design::text::LABEL)
                                .line_height(design::text::LABEL_LINE_HEIGHT)
                                .text_color(design::role::fg_tertiary(cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_recording_status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.recording.as_ref().map(|recording| {
            let label = format!("Recording shortcut for {}", recording.command.label);
            let hint = recording_hint();
            measure_band(self.chrome_inset)
                .child(
                    h_flex()
                        .id("settings-recording-status")
                        .w_full()
                        .px(SETTINGS_PAGE_INSET)
                        .py(space::SM)
                        .gap(space::SM)
                        .items_center()
                        .track_focus(&self.recording_focus)
                        .role(Role::Status)
                        .aria_label(label.clone())
                        .aria_description(hint)
                        .border_l_2()
                        .border_color(design::role::accent(cx))
                        .child(
                            Icon::new(IconName::Keyboard)
                                .with_size(Size::Size(design::icon::IN_ROW))
                                .text_color(design::icon::resting(cx)),
                        )
                        .child(
                            Label::new(label)
                                .text_size(design::text::LABEL)
                                .line_height(design::text::LABEL_LINE_HEIGHT)
                                .text_color(design::role::fg_primary(cx)),
                        ),
                )
                .into_any_element()
        })
    }

    /// The window's own title bar.
    ///
    /// It names the window and it says how the window closes, and it says
    /// nothing else: §15.2's structure has the search field on its own line
    /// under this one, and a title bar that restates the page underneath it is
    /// the duplicate §3.1 does not allow.
    fn render_title_bar(&self, cx: &Context<Self>) -> AnyElement {
        // The band keeps the chrome plane and the hairline for the whole window
        // width; the row inside it is held to the measure, so the title and the
        // close hint sit on the same spine as the search field and the rows under
        // them. Right-aligned to the measure rather than to the window's edge for
        // the same reason — a key chip 1900px from the word that explains it is
        // not a hint.
        div()
            .flex_none()
            .w_full()
            .bg(design::role::surface_chrome(cx))
            .child(
                measure_band(self.chrome_inset).child(
                    h_flex()
                        .h(design::size::TITLE_BAR)
                        .min_h(design::size::TITLE_BAR)
                        .px(SETTINGS_PAGE_INSET)
                        .gap(space::SM)
                        .items_center()
                        .child(
                            Icon::new(IconName::Settings)
                                .with_size(Size::Size(design::icon::IN_ROW))
                                // The window's own mark, beside a title at full
                                // strength. In the count tier it read as a title the
                                // window could not show.
                                .text_color(design::icon::resting(cx)),
                        )
                        .child(
                            label_panel_title("Settings").text_color(design::role::fg_primary(cx)),
                        )
                        .child(div().flex_1())
                        .child(
                            h_flex()
                                .id("settings-close-hint")
                                .gap(space::XS)
                                .items_center()
                                // "Close window" rather than the fragment "to close":
                                // a hint that trails the title reads as a stray word
                                // beside a key rather than as an instruction, and
                                // `Interface language` asks a label to name what the
                                // key does. Sentence case, no period, and the chip
                                // beside it.
                                .child(
                                    Label::new("Close window")
                                        .text_size(design::text::LABEL)
                                        .line_height(design::text::LABEL_LINE_HEIGHT)
                                        .text_color(design::role::fg_tertiary(cx)),
                                )
                                .child(Kbd::new(close_chord()).into_any_element()),
                        ),
                ),
            )
            .into_any_element()
    }

    /// The search line, and the number of rows a query kept.
    ///
    /// One field, searching whatever list is on screen. `⌘F`-style filtering is
    /// the native answer to "sixty settings with no way to find one", so the
    /// field is the first thing under the title and it is never hidden — a
    /// search box that disappears in one mode is a search box a reader has to
    /// look for twice.
    ///
    /// The count sits against the field's trailing edge, not against the
    /// window's. It used to be pushed to the far right by a spacer, which put
    /// the one number that says how many rows survived 800px from the box that
    /// produced it on a wide window.
    fn render_search_row(&self, cx: &Context<Self>) -> AnyElement {
        let searching = !self.search_query.trim().is_empty();
        let mut row = h_flex()
            .flex_none()
            .w_full()
            .min_h(design::size::TITLE_BAR)
            .px(SETTINGS_PAGE_INSET)
            .gap(space::MD)
            .items_center()
            .child(
                div()
                    .w(settings_search_width())
                    .flex_none()
                    .track_focus(&self.search_focus)
                    .debug_selector(|| "settings-search".to_owned())
                    .child(self.search_input.clone()),
            );
        row = row.when(searching, |row| {
            row.child(
                div()
                    .flex_none()
                    .debug_selector(|| "settings-search-results".to_owned())
                    .child(
                        Label::new(self.search_result_label(cx))
                            .text_size(design::text::LABEL)
                            .line_height(design::text::LABEL_LINE_HEIGHT)
                            .text_color(design::role::fg_tertiary(cx)),
                    ),
            )
        });
        // Chrome, not the app field: the title bar, the search line and the tab
        // strip are one window band, and the single hairline under the strip is
        // the only boundary that band needs. Three bands each drawing a rule put
        // two hairlines 40px apart saying the same thing twice, which is what
        // "hairlines belong on the boundary owner" rules out. With the rail up
        // there is no strip, so the search line is the last band of the plane and
        // the rule moves here rather than being drawn twice.
        let band = div()
            .flex_none()
            .w_full()
            .bg(design::role::surface_chrome(cx))
            .when(self.nav_rail, |band| {
                band.border_b_1()
                    .border_color(design::role::border_subtle(cx))
            });
        band.child(measure_band(self.chrome_inset).child(row.child(div().flex_1())))
            .into_any_element()
    }

    /// The category rail: the six names in a column beside the page.
    ///
    /// A rail rather than a strip because the strip put the navigation in a
    /// 40px band with a hairline under it, which read as a fourth chrome band
    /// above the rows — and because a horizontal strip of names cannot grow: a
    /// second locale's longer spelling put six words and a scrollbar into a
    /// window whose content was already a narrow ribbon. A column has room for
    /// the longest name at any height, and it gives the window the structure
    /// every other surface in this app has — the resource sidebar beside the
    /// table, the inspector beside both — instead of a form floating in the
    /// middle of a field.
    ///
    /// The selected category is the one place on this surface that spends an
    /// accent: a wash, a 2px leading rail and `fg.primary`. Structure first,
    /// colour last: with the accent hidden, the selected row is still the only
    /// row carrying a plate and a rule.
    fn render_nav_rail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let matching = self.matching_categories(cx);
        let searching = !self.search_query.trim().is_empty();
        let mut items = v_flex()
            .id("settings-nav-rail-items")
            .flex_none()
            .w_full()
            .gap(space::XXS)
            .items_stretch()
            // The six names are a list of tabs, and saying so is the difference
            // between a column of words a reader has to count and a set of
            // controls a screen reader can announce as one. The strip below
            // carries the same role for the same reason.
            //
            // The `id` above is not decoration: `role()` lives on
            // `StatefulInteractiveElement`, so a builder that never took an
            // identity cannot carry an accessible role at all.
            .role(Role::TabList)
            .aria_label("Settings categories");
        for (index, category) in SettingsCategory::ALL.into_iter().enumerate() {
            let selected = self.category == category && !searching;
            let available = !searching || matching.contains(&category);
            let selector = format!("settings-category-{}", category.label());
            let focus = window
                .use_keyed_state(selector.clone(), cx, |_, cx| cx.focus_handle())
                .read(cx)
                .clone()
                .tab_index(tab_order::TABS_FIRST + index as isize)
                .tab_stop(available);
            let ring = design::focus::border(cx);
            // Hover is mixed over the plate the item is actually drawn on. The
            // selected item is an accent wash over chrome, so a wash mixed over
            // bare chrome replaced the accent on the one row that carries it —
            // pointing at the category you are already on turned its plate
            // grey, which is the one place a hover must not take anything away.
            let plate = if selected {
                design::composite_surface(
                    design::role::surface_chrome(cx),
                    design::role::accent_wash(cx),
                )
            } else {
                design::role::surface_chrome(cx)
            };
            let hover = design::state::hover_on(plate, design::role::fg_primary(cx));
            let ink = match (selected, available) {
                (true, _) => design::role::fg_primary(cx),
                (false, true) => design::role::fg_secondary(cx),
                (false, false) => design::role::fg_disabled(cx),
            };
            let tip = if self.reference && category == SettingsCategory::Keyboard {
                "Close the shortcut reference".to_owned()
            } else {
                category.summary().to_owned()
            };
            let press = cx.weak_entity();
            let key = press.clone();
            // The rule is reserved on every row, never added to the selected
            // one, so selecting a category does not move the name beside it —
            // the same reason the segmented track reserves its focus ring.
            items = items.child(
                h_flex()
                    .id(selector.clone())
                    .debug_selector(move || selector)
                    .flex_none()
                    .w_full()
                    // The name is the one thing in a fixed-width lane that a
                    // locale decides the width of, so it is the one thing that
                    // has to be allowed to run out of room: the rail is a fixed
                    // 200px because a category name is what has to stay on one
                    // line, and a name that wrapped inside a 32px row instead
                    // took the row's height away from every row under it.
                    .min_w(px(0.))
                    .h(design::size::ROW)
                    .items_center()
                    .px(space::SM)
                    .gap(space::XS)
                    .rounded(design::radius::SM)
                    .when(selected, |item| item.bg(design::role::accent_wash(cx)))
                    // The selection is the wash, the weight and the ink — the
                    // border stays a transparent reservation for the focus ring.
                    // A leading-edge rail as the selection marker is the web
                    // habit the guide forbids, and the rest of the app dropped.
                    .border_l_2()
                    .border_color(design::role::surface_chrome(cx).alpha(0.))
                    .track_focus(&focus)
                    .focus_visible(move |this| this.border_color(ring))
                    .role(Role::Tab)
                    .aria_label(category.label())
                    .aria_selected(selected)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(
                        Label::new(category.label())
                            .text_size(design::text::BODY)
                            .line_height(design::text::BODY_LINE_HEIGHT)
                            .font_weight(if selected {
                                design::text::MEDIUM
                            } else {
                                design::text::REGULAR
                            })
                            .text_color(ink)
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .when(available, |this| {
                        this.hover(move |style| style.bg(hover))
                            .on_click(move |_, _, cx| {
                                if let Some(view) = press.upgrade() {
                                    view.update(cx, |view, cx| view.select_category(category, cx));
                                }
                            })
                            // Enter and Space, because a rail item is the focus
                            // stop and a focus stop that only answers a pointer
                            // is not one.
                            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                                if !matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    return;
                                }
                                cx.stop_propagation();
                                if let Some(view) = key.upgrade() {
                                    view.update(cx, |view, cx| view.select_category(category, cx));
                                }
                            })
                    }),
            );
        }
        // Chrome, and the window's own vertical boundary: the rail is a strip of
        // the chrome plane, so the hairline between it and the page is the rail's
        // to draw rather than the page's. The rail scrolls on its own handle, so
        // a category list longer than the window does not take the rows with it.
        v_flex()
            .id("settings-nav")
            .debug_selector(|| "settings-nav".to_owned())
            .flex_none()
            .w(px(SETTINGS_NAV_WIDTH))
            .min_h(px(0.))
            .bg(design::role::surface_chrome(cx))
            .border_r_1()
            .border_color(design::role::border_subtle(cx))
            .overflow_y_scroll()
            .track_scroll(&self.nav_scroll)
            .px(space::SM)
            .py(space::SM)
            .child(items)
            .into_any_element()
    }

    /// The six category tabs.
    ///
    /// The narrow fallback, drawn only when the window cannot hold the rail and
    /// the measure side by side. A rail is the better layout everywhere there is
    /// room for it; below that room a strip of names beats a rail of truncated
    /// ones, and the band is the one this window's chrome already had.
    fn render_tabs(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let matching = self.matching_categories(cx);
        let searching = !self.search_query.trim().is_empty();
        // 32px, the same step the rows use, so the strip and the form it opens
        // share one density rather than the strip being 4px shorter than a row
        // for no reason.
        //
        // Every tab is built by this one loop from the same three numbers —
        // `px(space::SM)`, `size::ROW` and a reserved 2px rule — so all six
        // names have identical padding and a name that wraps or a longer locale
        // cannot shift one of them off the row's baseline.
        let mut tabs = h_flex()
            .id("settings-tab-strip-items")
            .flex_none()
            .w_full()
            .h(design::size::ROW)
            .px(SETTINGS_PAGE_INSET)
            .gap(space::XXS)
            .items_stretch()
            // The same list the rail announces, drawn on one line: switching
            // layouts must not change what the surface says it is. The `id` is
            // what lets a builder carry a role at all — see `render_nav_rail`.
            .role(Role::TabList)
            .aria_label("Settings categories");
        for (index, category) in SettingsCategory::ALL.into_iter().enumerate() {
            let selected = self.category == category && !searching;
            // A search draws every page that has a hit, so the strip highlights
            // nothing and the pages the filter emptied are disabled: a tab that
            // does nothing visible says the opposite of what a tab is for.
            let available = !searching || matching.contains(&category);
            // A word in a strip, with a rule under it. Built here rather than
            // taken from gpui-kit's `Button` because a ghost button takes its
            // label colour from three component-theme tokens this app's theme
            // does not map — `secondary_foreground` when selected and
            // `accent_foreground` on hover — and the result on this surface was
            // a tab whose name turned accent-blue on hover and then *disappeared*
            // the moment the pointer left. A tab is also not a push button, so it
            // has none of a button's states to get wrong: two ink weights, one
            // 2px rule, and a hover wash. The wrapper owns the focus stop, the
            // role and the name, which is the arrangement the resource sidebar
            // documents for the same reason.
            let ring = design::focus::border(cx);
            // Hover is mixed onto the plane the tab actually sits on. The strip
            // carries `surface.chrome` itself now, so this is no longer a
            // coincidence with the title bar above it — it is the same surface the
            // pixel under the pointer is painted on, which is the rule `state::hover`
            // documents for taking the local ink rather than reading one.
            let hover = design::state::hover(cx, design::role::surface_chrome(cx));
            let selector = format!("settings-category-{}", category.label());
            let focus = window
                .use_keyed_state(selector.clone(), cx, |_, cx| cx.focus_handle())
                .read(cx)
                .clone()
                .tab_index(tab_order::TABS_FIRST + index as isize)
                .tab_stop(available);
            let ink = match (selected, available) {
                (true, _) => design::role::fg_primary(cx),
                (false, true) => design::role::fg_tertiary(cx),
                (false, false) => design::role::fg_disabled(cx),
            };
            // `subtitle` — 13/500, the token §2.3 gives a button's own text, and
            // the size the mockup's category navigation is drawn at.
            let label = Label::new(category.label())
                .text_size(design::text::SUBTITLE)
                .line_height(design::text::SUBTITLE_LINE_HEIGHT)
                .font_weight(design::text::MEDIUM)
                .text_color(ink);
            let tip = if self.reference && category == SettingsCategory::Keyboard {
                "Close the shortcut reference".to_owned()
            } else {
                category.summary().to_owned()
            };
            let press = cx.weak_entity();
            let key = press.clone();
            let tab = div()
                .id(selector.clone())
                .debug_selector(move || selector)
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .px(space::SM)
                .rounded(design::radius::SM)
                // The selection is the tab's own surface, like a dock pill,
                // not a one-sided underline: the bottom border stays a
                // transparent reservation for the focus ring.
                .when(selected, |this| this.bg(design::role::accent_wash(cx)))
                .border_b_2()
                .border_color(design::role::border_subtle(cx).alpha(0.))
                .track_focus(&focus)
                .focus_visible(move |this| this.border_color(ring))
                .role(Role::Tab)
                .aria_label(category.label())
                .aria_selected(selected)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .child(label)
                .when(available, |this| {
                    this.hover(move |style| style.bg(hover))
                        .on_click(move |_, _, cx| {
                            if let Some(view) = press.upgrade() {
                                view.update(cx, |view, cx| view.select_category(category, cx));
                            }
                        })
                        // Enter and Space, because the tab is the focus stop
                        // and a focus stop that only answers a pointer is not
                        // one. `on_key_down` rather than a binding: the strip
                        // owns the keys its own tabs answer.
                        .on_key_down(move |event: &KeyDownEvent, _, cx| {
                            if !matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                return;
                            }
                            cx.stop_propagation();
                            if let Some(view) = key.upgrade() {
                                view.update(cx, |view, cx| view.select_category(category, cx));
                            }
                        })
                });
            tabs = tabs.child(tab);
        }
        // The one hairline that says "the window ends here", on the band that owns
        // it. The title bar above and the search line between no longer draw one
        // each, so the strip's 112px chrome band has a single boundary instead of
        // three parallel rules 40px apart. With the rail up there is no strip and
        // `render_search_row` owns this rule instead.
        div()
            .flex_none()
            .w_full()
            .bg(design::role::surface_chrome(cx))
            .border_b_1()
            .border_color(design::role::border_subtle(cx))
            .child(measure_band(self.chrome_inset).child(tabs))
            .into_any_element()
    }

    /// The count the field states while a query is filtering.
    ///
    /// §3.1 does not let a title restate the window title, the tab title and the
    /// page heading at once, and the title bar and the category navigation
    /// already name the page, so the field prints no name of its own. What is
    /// left for it to say is how many rows the query kept: a filtered list and an
    /// empty one look the same without a number.
    fn search_result_label(&self, cx: &App) -> String {
        if self.reference {
            let count = self.matching_command_count(cx);
            return format!(
                "{count} command{suffix}",
                suffix = if count == 1 { "" } else { "s" }
            );
        }
        let count = self.result_count(cx);
        format!(
            "{count} setting{suffix}",
            suffix = if count == 1 { "" } else { "s" }
        )
    }

    /// How many commands a query keeps out of the reference.
    fn matching_command_count(&self, cx: &App) -> usize {
        let query = self.query();
        self.keyboard_sections(cx)
            .iter()
            .map(|section| {
                section
                    .commands
                    .iter()
                    .filter(|command| command_matches(command, &section.title, &query))
                    .count()
            })
            .sum()
    }

    // -- the content area --------------------------------------------------

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let content = if self.reference {
            self.render_reference(cx)
        } else if !self.search_query.trim().is_empty() {
            self.render_search_results(window, cx)
        } else {
            self.render_category_page(window, cx)
        };
        let selector = "settings-content";
        // The wrapper carries the measure's leading edge and the measure box
        // carries the measure itself, so the box the tests point at — and the box
        // every row's trailing edge is measured from — is the capped column and
        // nothing else. The inset is the window's, resolved once per frame: with
        // the rail up it is zero, because the content pane is already everything
        // left of the measure and a second inset inside it would double the
        // gutter beside the rail's rule; without the rail it centres the measure,
        // which is the whole of the layout in a window too narrow for two
        // columns. A definite height on both, so the empty state inside (which is
        // `size_full` and wants to centre itself in the space it has) resolves
        // against the scroll viewport instead of against a chain of auto-height
        // parents. The empty state used to claim the whole column and draw its
        // action on top of the danger zone.
        div()
            .w_full()
            .h_full()
            .px(self.measure_inset)
            .child(
                div()
                    .id("settings-content")
                    .debug_selector(move || selector.to_owned())
                    .w_full()
                    .h_full()
                    .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
                    .child(content),
            )
            .into_any_element()
    }

    /// One page: its rows grouped by [`Setting::group`], and the danger zone.
    ///
    /// No heading. §15.2's structure has none, the mockup has none, and the
    /// navigation beside the page already carries the page's name in the
    /// strongest ink on the screen — so a heading repeats it in a second size and
    /// a second weight, and spends the top of every page on the one thing a
    /// reader already knows. The category's one-line summary is not lost either:
    /// it is the name's own tooltip, which is where a reader asks what a page is
    /// for.
    ///
    /// The groups are what replaces the empty half of the page instead. Round 1
    /// compressed eight rows to one line each and left a document 500px shorter
    /// than its window, with the footer pinned to the floor and a void between
    /// the two; the honest repairs were a taller document or a shorter one, and
    /// the only taller document available without inventing rows is real
    /// structure. A group head costs 34px at the top of a page and 46px between
    /// two groups, and it says what the rows beneath it are for.
    fn render_category_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let category = self.category;
        let stored = user_settings(cx);
        let compact = self.compact;
        let mut content = v_flex().w_full();
        let mut group = "";
        for setting in SPECS
            .iter()
            .filter(|spec| spec.category == category)
            .map(|spec| spec.setting)
        {
            let name = setting.group();
            if name != group {
                // The first group gets the page's own padding and every group
                // after it the full section gap: the chrome band above already
                // ends in a hairline, so the page's first head needs air, not a
                // second boundary.
                content = content.child(page_section_head(name, group.is_empty(), cx));
                group = name;
            }
            let value = self.read(setting, cx, &stored);
            content = content.child(
                self.render_setting_row(setting, &value, None, compact, window, cx)
                    .into_any_element(),
            );
        }
        content = content.child(self.render_danger_zone(cx));
        content.into_any_element()
    }

    /// The search result list: one flat list across all six pages, with each hit
    /// carrying the page it came from.
    ///
    /// Grouping under real headers would be tidier and would answer a narrower
    /// question. A reader who searched wants to see everything that matched, in
    /// one pass, and know which page each row is on — which is why the category
    /// is on the row rather than in a header above a run of them, and why the
    /// page's own groups are not drawn here: a group is only true of the rows
    /// around it, and a filtered list is not the rows around anything.
    fn render_search_results(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let results = self.results(cx);
        let stored = user_settings(cx);
        let compact = self.compact;
        if results.is_empty() {
            // The empty state takes the space it is given and centres its group
            // in it, so the danger zone keeps the floor below — the same
            // arrangement as a page with rows in it, which is what keeps the one
            // control on this window that cannot be undone in the same place
            // whether the filter kept four settings or none of them.
            return v_flex()
                .w_full()
                .h_full()
                .child(self.render_no_matches(cx))
                .child(self.render_danger_zone(cx))
                .into_any_element();
        }
        let mut content = v_flex().w_full();
        for (setting, _) in results {
            let value = self.read(setting, cx, &stored);
            content = content.child(
                self.render_setting_row(
                    setting,
                    &value,
                    Some(setting.category().label()),
                    compact,
                    window,
                    cx,
                )
                .into_any_element(),
            );
        }
        content = content.child(self.render_danger_zone(cx));
        content.into_any_element()
    }

    /// The state a search with no hits reaches.
    ///
    /// §4.13, taken literally: a 24px muted icon, one line, one action. The line
    /// is "No matching settings" and nothing more — it names *matching*, so it
    /// already says this is a filter and not an empty product, and the query that
    /// emptied the list is still sitting in the field 30px above it. The old
    /// second sentence repeated the query back ("Nothing matches “cache limit”.
    /// All 31 settings are in these six pages.") which is two sentences of
    /// explanation for a state the reader can undo with one click.
    fn render_no_matches(&self, cx: &mut Context<Self>) -> AnyElement {
        let action = div()
            .id("settings-clear-search-action")
            .debug_selector(|| "settings-clear-search".to_owned())
            .child(
                // Not `primary()`. This is the recovery from a filter the reader
                // set themselves one keystroke ago, and the one control this
                // window wears `primary` on is the armed destructive commit at
                // the bottom of the page: a filled accent plate here would put
                // the loudest thing on the screen above a message saying nothing
                // matched. The empty state's action is the reader's way back, not
                // the screen's main event.
                token_button("settings-clear-search", "Clear search", false, cx)
                    .h(design::size::CONTROL)
                    .accessibility_label("Clear the settings search")
                    .tooltip("Clear the search and show every page.")
                    .on_click(cx.listener(move |view, _, _, cx| view.set_search_query("", cx))),
            );
        div()
            .id("settings-no-matches")
            .debug_selector(|| "settings-no-matches".to_owned())
            .w_full()
            // `flex_1` and not `h_full`: a percentage height has nothing definite
            // to resolve against under a scroller, and the empty state this wraps
            // already centres itself — so the box takes the space the danger zone
            // below it leaves rather than claiming all of it.
            .flex_1()
            .min_h(px(0.))
            .child(empty_state_with_action(
                IconName::Funnel,
                "No matching settings",
                String::new(),
                Some(action.into_any_element()),
            ))
            .into_any_element()
    }

    // -- one settings row --------------------------------------------------

    fn render_setting_row(
        &mut self,
        setting: Setting,
        value: &Value,
        category_label: Option<&str>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let spec = setting.spec();
        let tab = setting.tab();
        // A check box prints its own label beside itself. `Forms and settings`
        // asks for a visible label next to the field it names, and a 16px box
        // stranded on the far edge of a two-column row with its only name in the
        // column to its left is a control the reader has to hunt for — it was the
        // least legible control on the surface and the one every boolean row
        // shares. The sentence is a short name for the choice rather than the
        // row's description, which stays in the label column where the rest of
        // the form keeps its help text.
        let beside_box = setting.box_label().filter(|_| !compact);
        let label = setting_row_label(
            spec.title,
            spec.pending,
            Some(spec.hint),
            category_label,
            compact,
            cx,
        )
        .when_some(self.save_failure_note(setting, cx), |label, note| {
            label.child(note)
        })
        .when(setting == Setting::CheckForUpdates, |label| {
            label.when_some(self.render_update_note(cx), |label, note| label.child(note))
        });
        let control = self.render_control(setting, value, tab, beside_box, window, cx);
        let row = if compact {
            v_flex().w_full().gap(space::SM).items_start().child(label)
        } else {
            h_flex()
                .items_center()
                // The label keeps its fixed measure so no description re-wraps,
                // the control column keeps its fixed width so the widest control
                // still has room, and the spacer between them takes what is
                // left. The control column therefore ends on the measure's
                // trailing edge instead of floating a third of the way across a
                // wide window, which is the alignment `layout.md` asks a form
                // for — and the two columns are on the same spine on every one of
                // the six pages because every row is built here.
                .child(label.w(px(SETTINGS_LABEL_WIDTH)).flex_none())
                .child(div().flex_1().min_w(px(f32::from(space::XL))))
        };
        let selector = setting.selector();
        let description = spec.description;
        row.w_full()
            .min_h(design::size::ROW)
            // The page inset, so a row's label column and the search field,
            // the navigation and the bands above it start on one leading edge.
            .px(SETTINGS_PAGE_INSET)
            // 8 above and 8 below: a description sits 4px under its own title and
            // 16px above the next row's title, so the two gaps say two different
            // relationships without a rule between the rows.
            .py(space::SM)
            .id(selector.clone())
            .debug_selector(move || selector.clone())
            // The whole sentence is one hover away, so a row stays a pair and a
            // page of eight of them still fits a window without scrolling.
            .tooltip(move |window, cx| Tooltip::new(description.to_owned()).build(window, cx))
            .child(setting_control(compact).child(control))
    }

    /// The note a row carries while the write that changed it failed.
    ///
    /// Only the rows whose answer lives in the settings file can be the ones a
    /// failed write lost, so the note names those and stays off the rest: a
    /// warning on a row that did not change is noise that teaches a reader to
    /// ignore warnings.
    fn save_failure_note(&self, setting: Setting, cx: &Context<Self>) -> Option<AnyElement> {
        self.save_error.as_ref()?;
        let stored_in_file = setting.storage_key().is_some()
            || matches!(
                setting,
                Setting::Theme
                    | Setting::TextSize
                    | Setting::Contrast
                    | Setting::ReduceMotion
                    | Setting::KeymapPreset
                    | Setting::Cache
            );
        if !stored_in_file {
            return None;
        }
        let id = format!("settings-row-note-{}", slug(setting.title()));
        Some(
            h_flex()
                .id(id.clone())
                .debug_selector(move || id)
                .gap(space::XS)
                .items_start()
                .role(Role::Status)
                .aria_label(SAVE_NOT_SAVED_NOTE)
                .child(
                    Icon::new(design::health_icon(Severity::Error))
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(design::role::danger(cx)),
                )
                .child(
                    Label::new(SAVE_NOT_SAVED_NOTE)
                        .text_size(design::text::CAPTION)
                        .line_height(design::text::CAPTION_LINE_HEIGHT)
                        .text_color(design::role::danger_word(cx)),
                )
                .into_any_element(),
        )
    }

    // -- the controls ------------------------------------------------------

    fn render_control(
        &mut self,
        setting: Setting,
        value: &Value,
        tab: isize,
        box_label: Option<&'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = setting.spec().pending.is_none();
        let control = match setting.control() {
            Control::Switch => {
                self.render_switch(setting, value, tab, box_label, enabled, window, cx)
            }
            Control::Number { .. } | Control::Text => {
                let text = match value {
                    Value::Number(number) => {
                        if number.fract() == 0.0 {
                            format!("{number:.0}")
                        } else {
                            number.to_string()
                        }
                    }
                    other => other.as_text(),
                };
                self.render_field(setting, &text, tab, window, enabled, cx)
            }
            Control::Path => self.render_path(setting, value, cx),
            Control::Choices => self.render_choices(setting, value, tab, window, enabled, cx),
            Control::Readonly => self.render_read_only(setting, cx),
            Control::Action => self.render_action(setting, tab, cx),
        };
        let selector = setting.control_selector();
        // A check box with its label beside it is a control that shows a value
        // and the name of that value, so it takes the value lane rather than
        // sizing itself to its own two words. Sized to its label it was the
        // narrowest control in the column and therefore the rightmost: `Contrast`
        // drew its box 47px right of the `Theme` and `Accent` fields above and
        // below it, and one row out of eight coming to a different place on the
        // page is a defect the eye finds before it reads anything.
        //
        // The lane goes on this wrapper rather than on the control inside it
        // because the wrapper is what `the_settings_rows_share_one_control_column`
        // measures, and it is what the pointer's hover has to cover if the lane
        // is to be the control's own frame.
        //
        // `box_label.is_some()` and not a `compact` argument, because the two are
        // the same fact and this file already carries four functions past
        // clippy's argument budget: the stacked form drops the name a check box
        // prints beside itself, so a box with no name is a lone 28px square with
        // no lane to share, and the lane is only ever wanted when the two are
        // drawn together.
        let lane = matches!(setting.control(), Control::Switch) && box_label.is_some();
        div()
            .flex_none()
            .debug_selector(move || selector.clone())
            .min_w(px(0.))
            .when(lane, |frame| frame.w(px(SETTINGS_VALUE_LANE)))
            .child(control)
            .into_any_element()
    }

    /// The box a boolean is answered with, and the name that goes with it.
    ///
    /// §15.3 names a switch, and this is the toggle that answers "on or off"
    /// with one control and one keystroke. It is a check box rather than a pill
    /// for two reasons that are both about the desktop: a pill track is the iOS
    /// control for a touch surface, and gpui-kit 0.6.6's styled `Switch` does
    /// not forward a tab index, so every pill in the window would land on the
    /// same tab slot and break the reading order Tab walks. The check box takes
    /// the shared 28px control target, so a pointer and the focus ring are the
    /// same size as the buttons beside it.
    ///
    /// The name beside it is part of the same drawing rather than a separate
    /// label: `box_label` supplies it, and `Checkbox` is what `Forms and
    /// settings` prescribes for an independent choice. It used to be drawn as a
    /// bare box on the trailing edge of the row with the setting named a hundred
    /// pixels away in the label column — and the row's own second line, the
    /// sentence that explains the setting, was doing the work a checkbox's label
    /// should do, in the quietest ink on the page.
    ///
    /// Drawn here rather than taken from gpui-kit's `Checkbox`, which is wrong
    /// for this screen in three ways at once. Its focus ring is keyed on *being
    /// focused* rather than on *focus being visible*, so a mouse click puts a ring
    /// on the box — §2's rule 10, and the rule the spec calls the line between
    /// refined and amateur. Its corner radius comes from the component theme
    /// rather than from `design::radius`, so it is the same `rounded_lg()` bypass
    /// §4.9 already flags on dialogs, and switching themes would not move it. And
    /// it exposes no way to ask for a keyboard-only ring, so there was nothing to
    /// configure and no knob to turn: the behaviour had to be replaced, not set.
    /// `table_view::view`'s `filter_checkbox` is the same drawing for the same
    /// reasons, and the two are kept in step deliberately — which is why the box
    /// is 14px at `radius::XS` and not the 16px at `radius::SM` this file drew:
    /// two checkboxes that mean the same thing, drawn at two sizes and two
    /// corners, is the drift the claim above exists to prevent, and the radius
    /// contract agrees (`XS` is a square set into another square; `SM` is a chip,
    /// a badge or an input). The 28px wrapper beside it is the hit target, so the
    /// smaller box costs nothing to press.
    #[expect(clippy::too_many_arguments)]
    fn render_switch(
        &mut self,
        setting: Setting,
        value: &Value,
        tab: isize,
        box_label: Option<&'static str>,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let checked = matches!(value, Value::Bool(true));
        let selector = setting.control_selector();
        let title = setting.title();
        let view = cx.weak_entity();
        let focus = window
            .use_keyed_state(selector.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone()
            .tab_index(tab)
            .tab_stop(enabled);
        let ring = design::focus::border(cx);
        let hover = design::state::hover(cx, design::role::surface_raised(cx));
        // A value the reader cannot change does not get to be the loudest thing
        // on the page. The Editor page is four rows of "already on, nothing reads
        // this", and four accent-filled boxes on it is an accent on every row of
        // the screen — §2's rule 8 counts two, and the selected category has one.
        //
        // So a checked box on a row nothing reads is a neutral box with a tick,
        // which still says "on" and says it in the ink of every other value on
        // the page. The colour is not lost: it is spent on the rows a reader can
        // actually change.
        let (fill, edge, box_ink, name_ink) = match (checked, enabled) {
            (true, true) => (
                design::role::accent(cx),
                design::role::accent(cx),
                design::role::accent_fg(cx),
                design::role::fg_primary(cx),
            ),
            (true, false) => (
                design::role::fg_tertiary(cx),
                design::role::fg_tertiary(cx),
                design::role::surface_raised(cx),
                design::role::fg_disabled(cx),
            ),
            (false, true) => (
                design::role::surface_overlay(cx).alpha(0.),
                design::role::border_base(cx),
                design::role::fg_tertiary(cx),
                design::role::fg_primary(cx),
            ),
            (false, false) => (
                design::role::surface_overlay(cx).alpha(0.),
                design::role::border_base(cx).alpha(design::state::DISABLED_ALPHA),
                design::role::fg_disabled(cx),
                design::role::fg_disabled(cx),
            ),
        };
        let mark = div()
            .flex_none()
            .w(px(14.))
            .h(px(14.))
            .rounded(design::radius::XS)
            .border_1()
            .border_color(edge)
            .bg(fill)
            .flex()
            .items_center()
            .justify_center()
            .when(checked, |mark| {
                mark.child(
                    Icon::new(IconName::Check)
                        .with_size(Size::Size(px(10.)))
                        .text_color(box_ink),
                )
            });
        // The box and its name are one target: a label beside a control that is
        // not part of it is a label the pointer can miss, and 6px of gap between a
        // 16px box and the word that names it is a gap a reader aims across. So
        // the wrapper carries the ring, the role, the stop and the click, and the
        // 28px frame it reserves around the box is the same frame the buttons in
        // this control column use.
        //
        // `text::LABEL` in `fg.primary`, not the muted second line the label
        // column prints: this is the name of the control, and a name drawn in the
        // quietest ink on the surface is a name that disappears.
        h_flex()
            .id(selector.clone())
            .debug_selector(move || selector.clone())
            .flex_none()
            .min_h(design::size::CONTROL)
            .px(space::XXS)
            .gap(space::SM)
            .items_center()
            .rounded(design::radius::MD)
            // The border is always there and carries no ink until the keyboard
            // asks, so appearing focus moves nothing.
            .border_1()
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(move |this| this.border_color(ring))
            .role(Role::CheckBox)
            .aria_label(title)
            .aria_toggled(if checked {
                Toggled::True
            } else {
                Toggled::False
            })
            .track_focus(&focus)
            .when(enabled, |this| {
                this.hover(move |style| style.bg(hover))
                    .on_click(move |_, _, cx| {
                        let Some(view) = view.upgrade() else {
                            return;
                        };
                        view.update(cx, |view, cx| {
                            view.write_setting_without_window(setting, Value::Bool(!checked), cx);
                        });
                    })
            })
            .child(mark)
            .when_some(box_label, |this, name| {
                this.child(
                    Label::new(name)
                        .text_size(design::text::LABEL)
                        .line_height(design::text::LABEL_LINE_HEIGHT)
                        .font_weight(design::text::MEDIUM)
                        .text_color(name_ink)
                        .into_any_element(),
                )
            })
            .into_any_element()
    }

    /// The editor one text or number row is answered with.
    ///
    /// `enabled` is the row's own answer to "does anything read this": an inert
    /// row's field is not focusable, not typeable and not a tab stop, because a
    /// field a reader can fill in for a setting nothing consumes is the cheapest
    /// kind of lie this screen can tell. The empty namespace field also gets a
    /// placeholder, because an empty box with no hint is not an answer a reader
    /// can read — the answer is "all namespaces", and only the tooltip said so.
    fn render_field(
        &mut self,
        setting: Setting,
        value: &str,
        tab: isize,
        window: &mut Window,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spec = setting.control();
        let view = cx.weak_entity();
        let state = window.use_keyed_state(
            format!("settings-field-{}", slug(setting.title())),
            cx,
            |window, cx| FieldState::new(value, spec, tab, enabled, view, setting, window, cx),
        );
        state.update(cx, |state, cx| state.sync(value, tab, enabled, window, cx));
        let input = state.read(cx).input.clone();
        let suffix = match spec {
            Control::Number { unit, .. } if !unit.is_empty() => Some(
                Label::new(unit)
                    .text_size(design::text::CAPTION)
                    .into_any_element(),
            ),
            _ => None,
        };
        let selector = setting.control_selector();
        let title = setting.title();
        let placeholder = field_placeholder(setting);
        // The tab stop is stamped on the state's own handle by `FieldState::sync`,
        // which is the same seam the app's own `TextInput` uses; the `Input`
        // element only carries the slot.
        if let Some(placeholder) = placeholder {
            let placeholder = SharedString::from(placeholder);
            state.update(cx, |state, cx| {
                state.input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder.clone(), window, cx);
                });
            });
        }
        let field = Input::new(&input)
            .id(selector.clone())
            .h(design::size::CONTROL)
            .w(px(SETTINGS_FIELD_WIDTH))
            .tab_index(tab)
            .disabled(!enabled)
            .aria_label(title);
        let field = match suffix {
            Some(suffix) => field.suffix(suffix),
            None => field,
        };
        div()
            .id(selector.clone())
            .flex_none()
            .debug_selector(move || selector)
            .child(field)
            .into_any_element()
    }

    /// An enum row, answered by a segmented control or a drop-down.
    ///
    /// Which one is derived from the number of answers, not chosen per row: two
    /// to four fit on screen at once and are shown at once, and anything more
    /// becomes a drop-down. Either way every legal value is on the surface,
    /// which is §15.3's whole point — a text box that silently rejects `Comfy`
    /// is worse than no setting at all.
    fn render_choices(
        &mut self,
        setting: Setting,
        value: &Value,
        tab: isize,
        window: &mut Window,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = setting.choices(cx);
        let current = value.as_text();
        if Control::segmented(choices.len()) {
            return self.render_segmented(setting, &choices, &current, tab, window, enabled, cx);
        }
        self.render_drop_down(setting, &current, tab, window, enabled, cx)
    }

    /// A segmented control: a track, two to four segments, one of them raised.
    ///
    /// Three things are decided here rather than taken from the button layer, and
    /// each of them was a measured failure:
    ///
    /// * **The track.** `ButtonGroup::outline()` drew a 1px stroke around the
    ///   group, which is the one stroke the spec does not allow here — the
    ///   permitted strokes are an input, an overlay and a panel divider. The
    ///   mockup answers it with a `surface.inset` track at `radius::MD` with 2px
    ///   padding and 2px gaps, and that is what is drawn here.
    /// * **The selection.** It is a background on a div, not a button state, so
    ///   disabling the control dims the choice instead of erasing it. gpui-kit
    ///   layers a disabled button's background over its selected background, and
    ///   a disabled segmented control therefore showed no choice at all: on the
    ///   Density and UI font rows there was nothing on screen to say which value
    ///   was in force.
    /// * **The tab stop.** The group is one stop, a radio group, and the arrow
    ///   keys move between the answers. It used to be two stops with the first
    ///   segment on one and every other segment on the other, so `System / On /
    ///   Off` could be answered from the keyboard only by pressing Enter on
    ///   `System`.
    #[expect(clippy::too_many_arguments)]
    fn render_segmented(
        &mut self,
        setting: Setting,
        choices: &[String],
        current: &str,
        tab: isize,
        window: &mut Window,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.weak_entity();
        let choices = choices.to_vec();
        let title = setting.title();
        let current = current.to_owned();
        let chosen = choices.iter().position(|choice| *choice == current);
        let id = format!("settings-segmented-{}", slug(title));
        // The group owns the focus, not its segments, so the ring is drawn once
        // around the whole control and Tab reaches the row once.
        //
        // The slot goes on the *handle*, not on the element. GPUI's `div` applies
        // `tab_index` and `tab_stop` only when it mints the handle itself; an
        // element that was given a handle by `track_focus` keeps that handle's
        // own values, and a handle starts life with `tab_stop: false`. So the
        // control was on screen, focusable, and unreachable by Tab — Reduce
        // motion and every other segmented row included. The clone shares the
        // handle's identity, so stamping it here is what puts the row in the
        // frame's tab map.
        let focus = window
            .use_keyed_state(id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone()
            .tab_index(tab)
            .tab_stop(enabled);
        // The ring, reserved rather than added. gpui keys an element's layout on
        // its computed style, and the focus-visible refinement is part of that
        // style, so a ring that arrives as a fresh `border_1` is two pixels
        // wider than the same track with no focus: the labels inside it move a
        // pixel and every row below it shifts with them, once per Tab. The width
        // is therefore always there and only the colour moves. This is the
        // arrangement the Dock's controls use, and the one that makes a keyboard
        // user's page hold still.
        let ring = design::focus::border(cx);
        // One silhouette. `radius::MD` on the track, one step in from it on the
        // two end segments, and square middles, which is the concentric
        // arrangement `Radius, spacing, and density` asks for: the selected fill
        // follows the track's inner corners instead of floating in it as a pill,
        // and the unselected surface behind it is not visible at all because it is
        // the track's own plane. The segments used to round every corner at
        // `radius::SM`, which left four little pills inside a rounded rectangle.
        let mut track = div()
            .id(id.clone())
            .debug_selector(move || format!("settings-segmented-{}", slug(title)))
            .flex_none()
            .flex()
            .items_stretch()
            .h(design::size::CONTROL)
            // The track is a control that holds other controls, so it carries the
            // control height and the 2px inset the mockup draws: 2px of padding
            // and 2px gaps between segments, which is also the whole of the
            // separation — one divider, one thickness, every boundary. No stroke:
            // a segmented control is not one of the three places this product is
            // allowed to draw one. The one that is there carries no ink until the
            // keyboard asks for it.
            .p(space::XXS)
            .gap(space::XXS)
            .rounded(design::radius::MD)
            .bg(design::role::surface_inset(cx))
            .border_1()
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(move |this| this.border_color(ring))
            .role(Role::RadioGroup)
            .track_focus(&focus);
        for (index, choice) in choices.iter().enumerate() {
            let is_chosen = chosen == Some(index);
            // A row nothing reads still has to answer "which value is in force",
            // so the chosen answer keeps its raised background and the next ink
            // down rather than being dimmed with the rest of the track. Dimming
            // the whole control is what put three identical grey words on
            // Density and UI font, which read as a control that had lost its
            // value rather than as one that is not taking input.
            let (label_color, segment_bg) = match (is_chosen, enabled) {
                (true, true) => (
                    design::role::fg_primary(cx),
                    design::role::surface_raised(cx),
                ),
                (true, false) => (
                    design::role::fg_secondary(cx),
                    design::role::surface_raised(cx),
                ),
                (false, true) => (
                    design::role::fg_tertiary(cx),
                    design::role::surface_inset(cx),
                ),
                (false, false) => (
                    design::role::fg_disabled(cx),
                    design::role::surface_inset(cx),
                ),
            };
            let label = Label::new(choice.clone())
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .font_weight(design::text::MEDIUM)
                .text_color(label_color)
                .into_any_element();
            let pick = view.clone();
            let picked = choice.clone();
            let hover = design::state::hover_on(segment_bg, design::role::fg_primary(cx));
            let first = index == 0;
            let last = index + 1 == choices.len();
            track = track.child(
                div()
                    .id(format!("settings-segment-{}-{}", slug(title), index))
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(space::SM)
                    // The two end segments take the track's radius less the
                    // track's own 2px inset, and the middle ones stay square, so
                    // the selected fill and the surface behind it share one
                    // silhouette instead of three pills inside a rounded
                    // rectangle. The inner corner is `radius::MD` less the
                    // track's 2px pad, which is `radius::SM`; drawing the full
                    // `MD` there put the fill's corner outside the track's own
                    // arc.
                    .when(first, |segment| {
                        segment
                            .rounded_tl(design::radius::SM)
                            .rounded_bl(design::radius::SM)
                    })
                    .when(last, |segment| {
                        segment
                            .rounded_tr(design::radius::SM)
                            .rounded_br(design::radius::SM)
                    })
                    .bg(segment_bg)
                    .role(Role::RadioButton)
                    .aria_label(format!("{title}: {choice}"))
                    .aria_selected(is_chosen)
                    .when(enabled, |this| {
                        this.hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| {
                                let Some(view) = pick.upgrade() else {
                                    return;
                                };
                                view.update(cx, |view, cx| {
                                    view.write_setting(
                                        setting,
                                        Value::Choice(picked.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            })
                    })
                    .child(label),
            );
        }
        // One stop, and the arrow keys answer the group the way a radio group
        // answers: move the selection. `enabled` is checked here as well as on
        // the segments, because a key that reaches a row nothing reads must not
        // write a value.
        let key_view = view.clone();
        let key_choices = choices.clone();
        track
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if !enabled {
                    return;
                }
                let step = match event.keystroke.key.as_str() {
                    "left" | "up" => Some(-1isize),
                    "right" | "down" => Some(1),
                    "home" => Some(isize::MIN),
                    "end" => Some(isize::MAX),
                    _ => None,
                };
                let Some(step) = step else {
                    return;
                };
                // The position to move from is read out of the store rather than
                // out of the frame that drew this control. A frame is a frame
                // behind a burst of keys — two arrows inside one key event turn
                // into one move, and the answer after them is measured from
                // where the control was drawn rather than from where it is — and
                // a segmented control that answers a second key with the first
                // key's answer is worse than one that only takes clicks.
                let stored = user_settings(cx);
                let from_label = read_setting(setting, cx, &stored).as_text();
                let from = key_choices
                    .iter()
                    .position(|choice| *choice == from_label)
                    .unwrap_or(chosen.unwrap_or(0)) as isize;
                let next = if step == isize::MIN {
                    0
                } else if step == isize::MAX {
                    key_choices.len() - 1
                } else {
                    (from + step).rem_euclid(key_choices.len() as isize) as usize
                };
                cx.stop_propagation();
                let Some(view) = key_view.upgrade() else {
                    return;
                };
                let choice = key_choices[next].clone();
                view.update(cx, |view, cx| {
                    view.write_setting(setting, Value::Choice(choice), window, cx);
                });
            })
            .into_any_element()
    }

    fn render_drop_down(
        &mut self,
        setting: Setting,
        current: &str,
        tab: isize,
        _window: &mut Window,
        enabled: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let label = if current.is_empty() {
            setting.title().to_owned()
        } else {
            current.to_owned()
        };
        let title = setting.title();
        let selector = setting.control_selector();
        // One lane for every drop-down. The trigger used to size itself to its
        // own label, so Theme measured 160px and Accent 196px in the same column
        // of the same page; a column whose right edge moves is not a column.
        //
        // A row nothing reads is drawn here rather than handed to the component
        // layer's `disabled`. gpui-kit's disabled pair resolves to
        // `input_background().opacity(0.5)` over `muted_foreground.opacity(0.5)`
        // — three component-theme tokens this product's theme does not map — and
        // the result on this surface was an Accent select that looked exactly
        // like the Theme select beside it: a filled box with a caret. A control
        // that cannot be used has to read as unusable *and* say why, and the
        // reason is the row's own second line, so the frame below is the
        // product's own disabled field: the same plane and hairline as the live
        // one, the label a step quieter, and no popover to open.
        //
        // `h_flex()` and not `div()`. A bare `div` is `display: block` in gpui
        // 0.6.6 and every one of its children is a block of its own, so the
        // `gap`, `items_center` and `justify_between` below were inert and the
        // field drew `Electric blue` on one line with the caret on a second one
        // inside the frame: a 56px-tall select whose text and whose caret were
        // not in the same row. Three flex properties that silently did nothing,
        // on the one control on the page that is drawn by hand.
        if !enabled {
            let reason = setting.spec().pending.unwrap_or_default().to_owned();
            let tip = format!("{title}: {label}. {reason}");
            return h_flex()
                .id(selector.clone())
                .debug_selector(move || selector.clone())
                .flex_none()
                .w(px(SETTINGS_VALUE_LANE))
                .h(design::size::CONTROL)
                .px(space::SM)
                .gap(space::SM)
                .items_center()
                .justify_between()
                .rounded(design::radius::MD)
                .bg(design::role::surface_raised(cx))
                .border_1()
                // `border.base`, the input's own hairline, and not the subtler
                // `border.subtle` a divider uses: a disabled field is still a
                // field, so its frame has to be the frame a live field beside it
                // is drawn with, and the ink is the only thing saying "off".
                .border_color(design::role::border_base(cx))
                .role(Role::ComboBox)
                .aria_label(format!("{title}: {label}, unavailable"))
                .aria_description(reason)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .child(
                    Label::new(label)
                        .text_size(design::text::LABEL)
                        .line_height(design::text::LABEL_LINE_HEIGHT)
                        .text_color(design::role::fg_disabled(cx)),
                )
                .child(
                    // The caret gpui-kit's own `Caret` draws for a live trigger:
                    // the same `IconName::ChevronDown` at `Size::Medium`, so the
                    // two selects in the same column differ in their ink and in
                    // nothing else. At 12px against a live 16px it read as a
                    // stray mark rather than as the field's own affordance.
                    Icon::new(IconName::ChevronDown)
                        .with_size(Size::Medium)
                        .text_color(design::role::fg_disabled(cx)),
                )
                .into_any_element();
        }
        let tooltip = format!("{title}: {label}");
        let open = self.open_menu == Some(OpenMenu(setting));
        let menu = if open { self.menu.clone() } else { None };
        // The stroke stays here, unlike the push buttons on this surface. A combo
        // box is a field that answers with a choice rather than with text, and
        // §4.7 makes the field one of the three places this product is allowed to
        // draw a 1px border — the mockup draws it as an input too.
        let trigger = token_button(selector, &label, true, cx)
            .h(design::size::CONTROL)
            .w(px(SETTINGS_VALUE_LANE))
            .tab_index(tab)
            .tab_stop(true)
            .dropdown_caret(true)
            .role(Role::ComboBox)
            .accessibility_label(title)
            .tooltip(tooltip);
        let popover = Popover::new(format!("settings-menu-{}", slug(setting.title())))
            // The menu opens beside its trigger, never over it: a menu the
            // trigger cannot press a second time to close is a menu the trigger
            // opened and cannot close. §4.4 says covering the content a pop-over
            // belongs to is the last thing it should do, and the trigger is the
            // content this one belongs to.
            .anchor(Anchor::RightCenter)
            .open(open)
            // The menu dismisses itself on a click outside and on Escape, so
            // the popover leaves that to it rather than also closing on the same
            // press: two owners of one dismissal toggle the menu straight back
            // open.
            .overlay_closable(false)
            .on_open_change(popover_open(cx.entity().downgrade(), setting))
            .trigger(trigger);
        let popover = match menu
            .as_ref()
            .map(|open| open.menu.read(cx).focus_handle(cx))
        {
            Some(focus) => popover.track_focus(&focus),
            None => popover,
        };
        let popover = match menu {
            Some(open) => popover.content(move |_, _, _| open.menu.clone()),
            None => popover,
        };
        popover.into_any_element()
    }

    /// A path, shown, with the platform's own way into the folder beside it.
    ///
    /// The path is machine-shaped text, so it is set in the mono face at its own
    /// size rather than in the body face at 13px, and it truncates: a home
    /// directory with a long name, or a container path, otherwise pushes the
    /// button that opens it off the end of the control column. The whole path is
    /// the row's tooltip and its accessible name, so truncating costs the reader
    /// nothing they cannot get back with a hover.
    fn render_path(&self, setting: Setting, value: &Value, cx: &mut Context<Self>) -> AnyElement {
        let path = value.as_text();
        let tooltip = path.clone();
        h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .child(
                div()
                    .id("settings-cache-path")
                    .flex_1()
                    .min_w(px(0.))
                    .aria_label(format!("Cache folder: {path}"))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(
                        Label::new(path)
                            .text_size(design::text::MONO_SM)
                            .line_height(design::text::MONO_SM_LINE_HEIGHT)
                            .text_color(design::role::fg_secondary(cx))
                            .truncate(),
                    ),
            )
            .child(
                token_button("settings-reveal-cache", "Open folder", false, cx)
                    .h(design::size::CONTROL)
                    .tab_index(setting.tab())
                    .accessibility_label("Open the cache folder")
                    .tooltip("Open the cache folder in the platform's file manager.")
                    .on_click(cx.listener(|_, _, _, _| {
                        if let Some(path) = k8s_core::paths::cache_dir() {
                            let _ = SettingsView::reveal(&path);
                        }
                    })),
            )
            .into_any_element()
    }

    /// A value nothing writes here: a build number, a probe result, a download
    /// in flight.
    ///
    /// These are the rows a loading state lives in. A probe that is still
    /// running shows the shared spinner, and only then: §2.1 rule 12 says a
    /// skeleton over a value nobody is waiting for is worse than no marker, so a
    /// finished probe is a status word and a glyph, not a shimmer.
    fn render_read_only(&self, setting: Setting, cx: &mut Context<Self>) -> AnyElement {
        let value = self.read(setting, cx, &user_settings(cx));
        let (severity, checking) = match setting {
            Setting::Helm => {
                let status = Capability::from(self.helm);
                (status.severity(), status.is_checking())
            }
            Setting::ClusterMetrics => (self.metrics.severity(), self.metrics.is_checking()),
            _ => (Severity::Muted, false),
        };
        let color = severity_color(severity, cx);
        // A build number is not a health verdict, so it gets no status glyph:
        // a dash in front of the version would read as "this build is broken".
        let glyph = (severity != Severity::Muted).then(|| design::health_icon(severity));
        // Only the version row has an action beside its value, and the value is
        // what sized that action's position: the two sat shoulder to shoulder
        // with the slack left over on the right, so `Copy` ended wherever this
        // build's version string happened to stop — the one control on the page
        // that was not on the control column's trailing edge. The value takes
        // the lane instead, and the button lands on the line every drop-down and
        // segmented control on this surface already lands on.
        let value_text = label_body(value.as_text()).text_color(design::role::fg_secondary(cx));
        let value_text = if setting == Setting::Version {
            value_text.flex_1().min_w(px(0.)).truncate()
        } else {
            value_text
        };
        let mut status_line = h_flex()
            .id("settings-readonly")
            .min_h(design::size::CONTROL)
            .flex_none()
            .w_full()
            .gap(space::XS)
            .items_center()
            .role(Role::Status)
            .aria_label(format!("{}: {}", setting.title(), value.as_text()))
            .when_some(glyph, |line, glyph| {
                // The same size either way round: a status mark that grew or
                // shrank when the read went from checking to answered would be
                // the state changing shape, which is what the shape is not for.
                line.child(if checking {
                    spinner(glyph, color, Size::Size(design::icon::IN_ROW))
                } else {
                    Icon::new(glyph)
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(color)
                        .into_any_element()
                })
            })
            .child(value_text);
        if let Some(reason) = self.capability_reason(setting) {
            let reason = reason.to_owned();
            status_line = status_line.aria_description(reason.clone());
            status_line = status_line
                .tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx));
        }
        if setting == Setting::Version {
            let version = value.as_text();
            status_line = status_line.child(
                div()
                    .flex_none()
                    .debug_selector(|| "settings-copy-version".to_owned())
                    .child(
                        token_button("settings-copy-version", "Copy", false, cx)
                            .h(design::size::CONTROL)
                            .tab_index(setting.tab())
                            .accessibility_label("Copy the version number")
                            .tooltip("Copy the version number.")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(version.clone()));
                            })),
                    ),
            );
        }
        status_line.into_any_element()
    }

    /// Whether the updater is mid-download, which is the one busy state on this
    /// page and the only one worth a spinner.
    fn update_is_busy(&self) -> bool {
        matches!(
            self.update_state.phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting
        )
    }

    fn capability_reason(&self, setting: Setting) -> Option<&str> {
        match setting {
            Setting::Helm => self.helm.reason().or(self.helm_error.as_deref()),
            Setting::ClusterMetrics => self.metrics_error.as_deref(),
            _ => None,
        }
    }

    /// The line under the Check now row.
    ///
    /// The updater is the one thing on this page that changes without the reader
    /// doing anything, so its state has to be on the page and not in a toast that
    /// arrives once and leaves. A run in flight gets the shared spinner, which is
    /// the only busy marker on the screen; a finished run is a sentence.
    fn render_update_note(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.update_actions.is_none() && self.update_state.phase == UpdatePhase::Idle {
            return None;
        }
        let severity = self.update_severity();
        let color = severity_color(severity, cx);
        let text = self.update_status();
        let busy = self.update_is_busy();
        Some(
            h_flex()
                .id("settings-update-note")
                .gap(space::XS)
                .items_center()
                .role(Role::Status)
                .aria_label(text.clone())
                .child(if busy {
                    spinner(IconName::LoaderCircle, color, Size::Size(design::icon::IN_ROW))
                } else {
                    Icon::new(design::health_icon(severity))
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(color)
                        .into_any_element()
                })
                .child(label_small(text).text_color(design::role::fg_secondary(cx)))
                .into_any_element(),
        )
    }

    /// The two rows that go somewhere rather than change a value: the shortcut
    /// reference and the updater.
    ///
    /// They are the only two controls on the surface that are not an answer to a
    /// question about the reader's own machine, and the only two whose label
    /// changes with their state — `Open reference` becomes `Close reference`, and
    /// `Check now` becomes `Restart to Update` the moment a download lands. A
    /// label that lies about what the press will do is the one thing a button in
    /// this position must not do.
    fn render_action(
        &mut self,
        setting: Setting,
        tab: isize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, tooltip) = match setting {
            Setting::ShortcutReference => (
                if self.reference {
                    "Close reference"
                } else {
                    "Open reference"
                },
                "Every command and its key, searchable. Also on ? from anywhere.",
            ),
            _ => (
                if self.update_restart_label().is_some() {
                    "Restart to update"
                } else {
                    "Check now"
                },
                "Look for a newer K8s Studio build for this installation.",
            ),
        };
        // At most one primary button per screen, and this is the only candidate
        // on the page: a downloaded update that never restarts leaves the reader
        // on the old build with no way forward.
        let primary = setting == Setting::CheckForUpdates && self.update_restart_label().is_some();
        // `primary` when the download has landed, and an ordinary *plate* the rest
        // of the time — a default `Button` for an ordinary action, per
        // `Interface language`, and not `outline`: §4.6 says a push button never
        // wears a stroke, and the three places this product is allowed to draw
        // one are an input, an overlay and a panel divider. A push button with a
        // border is the web habit the spec rules out, and on the Updates page it
        // also put a second box on a screen whose other accent is the tab
        // underline.
        let button = if primary {
            token_button(setting.control_selector(), label, false, cx).primary()
        } else {
            token_button(setting.control_selector(), label, false, cx)
        };
        let button = button
            .h(design::size::CONTROL)
            .tab_index(tab)
            .accessibility_label(setting.title())
            .tooltip(tooltip);
        if setting == Setting::CheckForUpdates {
            let restart = self.update_restart_label().is_some();
            return button
                .on_click(cx.listener(move |view, _, _, cx| {
                    if restart {
                        view.restart_to_update(cx);
                    } else {
                        view.check_for_updates(cx);
                    }
                    cx.notify();
                }))
                .into_any_element();
        }
        button
            .on_click(cx.listener(|view, _, window, cx| {
                view.toggle_shortcut_reference(window, cx);
            }))
            .into_any_element()
    }

    // -- the shortcut reference --------------------------------------------

    /// The shortcut reference: a search over every command, its key, and the
    /// surface that key is live in.
    ///
    /// This is `UI-REDESIGN` L13 item 3, and it is a destination rather than a
    /// settings page for the reason L13 gives: it is far more useful than the
    /// settings list it replaces. It keeps the editing controls, because a
    /// reference a reader cannot change is half a reference — the point is to
    /// answer "what does this key do" and "that is not the key I expected".
    fn render_reference(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // `space::MD`, the page's own first gap and not a section gap: the
        // reference is reached from the chrome plane — the search field's own
        // hairline with the rail up, the strip's with it down — and neither of
        // those is a group head above it. Every later head on this page is a
        // group of commands and takes the full section gap, which is what makes
        // the first one the page's own lead-in rather than another group.
        let mut content = v_flex().w_full().child(section_header(
            REFERENCE_TITLE,
            REFERENCE_SUMMARY,
            space::MD,
            cx,
        ));
        let query = self.query();
        content = content.child(self.render_keymap_file(cx));
        let mut tab_index = tab_order::REFERENCE_FIRST;
        let mut shown = 0usize;
        for section in self.keyboard_sections(cx).iter() {
            let visible = section
                .commands
                .iter()
                .filter(|command| command_matches(command, &section.title, &query))
                .cloned()
                .collect::<Vec<_>>();
            if visible.is_empty() {
                continue;
            }
            shown += visible.len();
            // No second line. Every row below already names the context its
            // shortcut is live in, and a constant sentence under eleven group
            // titles was a sentence that was wrong for the one group whose
            // commands are not application-wide.
            content = content.child(section_header(&section.title, "", space::XL, cx));
            for command in visible {
                content = content.child(self.render_keyboard_command(&command, tab_index, cx));
                tab_index += 2;
            }
        }
        if shown == 0 {
            content = content.child(self.render_no_shortcuts(cx));
        }
        content.into_any_element()
    }

    /// The reference's empty state: a search that kept no command.
    ///
    /// One line and one action, like every other empty state on this surface.
    /// The reference's own keymap block stays above it, so the state sits under
    /// the keymap the reader is editing rather than in a column of its own.
    fn render_no_shortcuts(&self, cx: &mut Context<Self>) -> AnyElement {
        let action = div()
            .id("settings-clear-shortcut-search-action")
            .debug_selector(|| "settings-clear-shortcut-search".to_owned())
            .child(
                // The same plate as the settings surface's own empty state and
                // for the same reason: this is the way back from a filter the
                // reader set one keystroke ago, not a commitment. The one control
                // this window wears `primary` on is the armed destructive commit.
                token_button("settings-clear-shortcut-search", "Clear search", false, cx)
                    .h(design::size::CONTROL)
                    .accessibility_label("Clear the Shortcut Search")
                    .tooltip("Clear the search and show every command.")
                    .on_click(cx.listener(|view, _, window, cx| view.clear_search(window, cx))),
            );
        div()
            .id("settings-no-shortcuts")
            .debug_selector(|| "settings-no-shortcuts".to_owned())
            .w_full()
            .min_h(design::size::ROW * 3.)
            .pt(space::XL)
            .child(empty_state_with_action(
                IconName::Funnel,
                "No matching shortcuts",
                String::new(),
                Some(action.into_any_element()),
            ))
            .into_any_element()
    }

    /// The user keymap file block.
    ///
    /// This is a block and not a two-column row because it owns a path and three
    /// controls, and a path set at 16px next to a path set at 12px in the same
    /// window is two designs. Deleting the file is not offered here: §15.2 says
    /// only the danger zone confirms, and one place that asks to be sure is
    /// better than three.
    fn render_keymap_file(&self, cx: &Context<Self>) -> AnyElement {
        let path = keymap::user_keymap_path();
        let path_label = path.as_ref().map_or_else(
            || {
                "The file path is unavailable. Check access to the user configuration directory."
                    .to_owned()
            },
            |path| path.display().to_string(),
        );
        let mut path_text = h_flex()
            .id("settings-keymap-path")
            .flex_1()
            .min_w(px(0.))
            .child(
                Label::new(path_label.clone())
                    .text_size(design::text::BODY)
                    .text_color(design::role::fg_secondary(cx))
                    .truncate(),
            );
        if path.is_some() {
            path_text = path_text.aria_label(format!("Keymap file path: {path_label}"));
            path_text = path_text.tooltip({
                let path_label = path_label.clone();
                move |window, cx| Tooltip::new(path_label.clone()).build(window, cx)
            });
        }
        let path_line = h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .child(path_text)
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "settings-keymap-copy-path".to_owned())
                    .child(
                        token_button("settings-keymap-copy-path", "Copy path", false, cx)
                            .h(design::size::CONTROL)
                            .tab_stop(path.is_some())
                            .disabled(path.is_none())
                            .accessibility_label("Copy the keymap file path")
                            .tooltip("Copy the keymap file path.")
                            .on_click(cx.listener(|view, _, _, cx| view.copy_keymap_path(cx))),
                    ),
            );
        let actions = h_flex()
            .w_full()
            .flex_wrap()
            .gap(space::SM)
            .items_center()
            .child(
                token_button("settings-keymap-create-show", "Open keymap file", false, cx)
                    .h(design::size::CONTROL)
                    .accessibility_label("Open the User Keymap File")
                    .tooltip("Create or show the user keymap file.")
                    .on_click(cx.listener(|view, _, window, cx| {
                        if let Some(handler) = &view.on_create_or_show_keymap {
                            handler(window, cx);
                        }
                    })),
            )
            .child(
                token_button("settings-keymap-reload", "Reload keymap", false, cx)
                    .h(design::size::CONTROL)
                    .accessibility_label("Reload the User Keymap")
                    .tooltip(keymap_reload_description())
                    .on_click(cx.listener(|view, _, window, cx| {
                        if let Some(handler) = &view.on_reload_keymap {
                            handler(window, cx);
                        }
                    })),
            );
        v_flex()
            .id("settings-keymap-file")
            .w_full()
            .gap(space::SM)
            .px(SETTINGS_PAGE_INSET)
            .py(space::SM)
            .min_h(design::size::ROW)
            .child(setting_row_label(
                "User keymap file",
                None,
                Some(&format!(
                    "Saved changes reload automatically. {} shortcuts use {modifier}.",
                    platform_name(),
                    modifier = modifier_names()
                )),
                None,
                false,
                cx,
            ))
            .child(path_line)
            .child(actions)
            .into_any_element()
    }

    fn render_keyboard_command(
        &mut self,
        command: &KeyboardCommand,
        tab_index: isize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chord = command_chord(command, cx);
        let is_recording = self
            .recording
            .as_ref()
            .is_some_and(|recording| recording.command.action_name == command.action_name);
        // The keycap is the column a person scans, so it is named for a test
        // that can measure it instead of trusting that the group happens to line
        // up.
        let keycap = div()
            .flex_none()
            .debug_selector(|| "settings-keycap".to_owned())
            .child(keycap_element_from_chord(
                chord.as_deref(),
                is_recording,
                cx,
            ));
        // The keycap, Edit and Clear share one control column, so the column
        // itself is named for the test that measures the trailing edge. The
        // keycap is its leading child and stops short of that edge by design.
        let mut controls = setting_control(false)
            .debug_selector(|| "settings-keyboard-controls".to_owned())
            .child(keycap);
        if command.editable {
            let edit_command = command.clone();
            controls = controls.child(
                // The product's own plate, not `outline`: sixty-odd commands each
                // with a bordered Edit is a screen drawn entirely in 1px boxes,
                // which is the thing §2's rule 5 exists to stop. `secondary` was
                // the intent and it drew bare text, because this theme does not
                // map the component tokens it resolves from.
                token_button(
                    format!("settings-key-edit-{tab_index}"),
                    if is_recording { "Recording…" } else { "Edit" },
                    false,
                    cx,
                )
                .h(design::size::CONTROL)
                .tab_index(tab_index)
                .tab_stop(!is_recording)
                .disabled(is_recording)
                .accessibility_label(format!("Edit shortcut for {}", command.label))
                .tooltip("Record a new shortcut for this command.")
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.start_recording(edit_command.clone(), window, cx);
                })),
            );
        } else {
            controls = controls.child(
                label_small("Use the keymap file.").text_color(design::role::fg_tertiary(cx)),
            );
        }
        if command.editable && chord.is_some() {
            let clear_command = command.clone();
            controls = controls.child(
                token_button(
                    format!("settings-key-clear-{tab_index}"),
                    "Clear",
                    false,
                    cx,
                )
                .h(design::size::CONTROL)
                .tab_index(tab_index + 1)
                .tab_stop(!is_recording)
                .disabled(is_recording)
                .accessibility_label(format!("Clear shortcut for {}", command.label))
                .tooltip("Remove this shortcut from the user keymap.")
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.clear_binding(&clear_command, cx);
                })),
            );
        }
        // A shortcut reference row keeps its whole sentence, because a row
        // nobody can read is the failure this reference exists to prevent. It is
        // a list of commands, not an eight-row page, and it is read one row at a
        // time rather than scanned.
        let description = match command.when.as_deref() {
            Some(when) => format!(
                "{} Active when: {}.",
                command.description,
                keyboard_context_description(when)
            ),
            None => command.description.clone(),
        };
        // A conflict belongs to the row that owns the key, so it reads next to
        // that keycap rather than in a toast about a different surface.
        let conflict = conflict_for_command(command, cx);
        let mut label = setting_row_label(
            command.label.clone(),
            None,
            Some(&description),
            None,
            false,
            cx,
        );
        if let Some(conflict) = conflict {
            let id = format!("settings-key-conflict-{}", command.action_name);
            label = label.child(
                h_flex()
                    .id(id.clone())
                    .debug_selector(move || id)
                    .gap(space::XS)
                    .items_start()
                    .role(Role::Status)
                    .aria_label(conflict.label())
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .with_size(Size::Size(design::icon::IN_ROW))
                            .text_color(design::role::danger(cx)),
                    )
                    .child(label_small(conflict.label()).text_color(design::role::danger_word(cx)))
                    .into_any_element(),
            );
        }
        let row = h_flex()
            .w_full()
            .min_h(design::size::ROW)
            // The same inset, the same label column and the same control column
            // as a settings row: the reference is another page of this window, not
            // a different form.
            .px(SETTINGS_PAGE_INSET)
            .py(space::SM)
            .items_center()
            .child(label.w(px(SETTINGS_LABEL_WIDTH)).flex_none())
            .child(div().flex_1().min_w(px(f32::from(space::XL))))
            .child(controls);
        // Named the way every other row on this surface is, for the same reason:
        // `debug_bounds` reads selectors and never element ids, and a row with
        // no selector is a row no test can point at — which is how a reference
        // row drifts out of the shared label and control columns without
        // anything noticing.
        let selector = format!("settings-row-key-{}", slug(&command.label));
        let row = row.id(selector.clone()).debug_selector(move || selector);
        let row = if is_recording {
            row.border_l_2().border_color(design::role::accent(cx))
        } else {
            row
        };
        row.into_any_element()
    }

    // -- the danger zone ---------------------------------------------------

    /// The last group of the page: the two actions on this window that cannot be
    /// undone.
    ///
    /// It is a group head, a caption and two buttons — a section of the document,
    /// in the same two columns as every row above it — and not a footer band. It
    /// used to be a 1px red rectangle wrapped around two *unstyled* buttons
    /// (`ghost()` in this product's theme resolves to bare text), so the most
    /// consequential controls in the window were the only two on it a reader
    /// could not recognise as controls, inside the only stroke in the file that
    /// was not an input, an overlay or a divider. `Alert` in the guide is for
    /// exceptional information that interrupts the hierarchy; a permanently
    /// present red frame around a settings page's footer is a decorated
    /// container, and `Treat emphasis as a limited budget` says the danger hue
    /// belongs on the one thing that means it: the destructive button.
    ///
    /// What replaced the red rectangle is a region: a `border.subtle` hairline
    /// above it, the same eyebrow every group head on the surface wears, and
    /// both actions in `danger_word`. The hairline is the boundary the zone owns
    /// — the page above it does not draw one, so no two adjacent regions say the
    /// same thing twice — and it is the one stroke on this surface that is doing
    /// work rather than describing a control.
    ///
    /// **Why it is no longer pinned to the window floor.** Round 1 pinned it,
    /// because pinning is what keeps a destructive pair on screen while the rows
    /// above it scroll, and because a test reads its bottom. But pinning turned a
    /// short document into a broken-looking one: eight rows of one-line hints end
    /// around y=520 and the footer sat at the window's bottom with 500px of
    /// nothing between them, which is a void *inside* the page — the eye reads it
    /// as the layout failing, not as the document being short. Unpinned, the same
    /// page is what every other settings page in the world is: a document that
    /// ends, and window below it.
    ///
    /// The cost is real and was measured rather than assumed. With the category
    /// strip the chrome plane is three bands — 40, 40 and 32 — and ends at y=113;
    /// with the rail it is two, because the strip's row is gone, and ends at
    /// y=81. The longest page — Appearance, seven rows in three groups — ends at
    /// y=729 on the first and y=697 on the second, so the destructive pair is on
    /// screen with 31px to spare at the window's own 760px default height either
    /// way and the whole document clears the 900px window a test measures at
    /// with 171px in hand. Below that the pair scrolls with the rows, and its own
    /// caption scrolls with it. §15.2 asks for it at the bottom of the page,
    /// which is where it is.
    ///
    /// It belongs to the settings pages and not to the reference: sixty-eight
    /// commands with two destructive buttons under them is a target nobody aims
    /// at.
    fn render_danger_zone(&self, cx: &mut Context<Self>) -> AnyElement {
        let armed_clear = self.danger_step == DangerStep::ClearCache;
        let armed_reset = self.danger_step == DangerStep::ResetAll;
        // Both actions are irreversible, so both are drawn as destructive and
        // neither is a primary: the zone is the one region of this window where
        // `danger_word` belongs, and two buttons wearing it is the reader being
        // told what this region is before either is pressed. `danger_word` is
        // held to the body-text floor on this surface, so it stays a word — this
        // is not a red frame around the region, and not a filled plate.
        //
        // An armed button is *not* `disabled`. It is the commit, and gpui-kit's
        // base button treats `disabled` as "ignores pointer and keyboard
        // activation", so an armed-and-disabled button could not be pressed to
        // finish what arming started. It leaves the tab order instead
        // (`tab_stop(false)`), which is what the flag was reaching for, and it
        // takes the filled `.danger()` treatment — the one state on this surface
        // in which a press does something irreversible is the one that looks it.
        let clear_label = if armed_clear {
            CLEAR_CACHE_ARMED_LABEL
        } else {
            CLEAR_CACHE_LABEL
        };
        let mut clear_button = destructive_button("settings-danger-clear-cache", clear_label, cx)
            .h(design::size::CONTROL)
            .tab_index(tab_order::DANGER_FIRST)
            .tab_stop(!armed_clear)
            .accessibility_label("Clear the disk cache")
            .tooltip(CLEAR_CACHE_CONFIRM)
            .on_click(cx.listener(|view, _, window, cx| {
                view.on_danger(DangerStep::ClearCache, window, cx);
            }));
        clear_button = if armed_clear {
            clear_button.danger()
        } else {
            clear_button
        };
        let reset_label = if armed_reset {
            RESET_ALL_ARMED_LABEL
        } else {
            RESET_ALL_LABEL
        };
        let mut reset_button = destructive_button("settings-danger-reset-all", reset_label, cx)
            .h(design::size::CONTROL)
            .tab_index(tab_order::DANGER_FIRST + 2)
            .tab_stop(!armed_reset)
            .accessibility_label("Reset all settings")
            .tooltip(RESET_ALL_CONFIRM)
            .on_click(cx.listener(|view, _, window, cx| {
                view.on_danger(DangerStep::ResetAll, window, cx);
            }));
        reset_button = if armed_reset {
            reset_button.danger()
        } else {
            reset_button
        };
        // `Cancel` sits *before* the commit, not after it, so a reader's next
        // press on the wrong side of the pair leaves rather than deletes.
        let cancel = armed_clear.then(|| {
            div()
                .flex_none()
                .debug_selector(|| "settings-danger-cancel".to_owned())
                .child(
                    token_button("settings-danger-cancel", CANCEL_LABEL, false, cx)
                        .h(design::size::CONTROL)
                        .tab_index(tab_order::DANGER_FIRST + 1)
                        .accessibility_label("Cancel clear cache")
                        .tooltip("Keep the cache.")
                        .on_click(cx.listener(move |view, _, _, cx| view.cancel_danger(cx))),
                )
        });
        let caption = match self.danger_step {
            DangerStep::Idle => DANGER_ZONE_CAPTION.to_owned(),
            DangerStep::ClearCache => CLEAR_CACHE_CONFIRM.to_owned(),
            DangerStep::ResetAll => RESET_ALL_CONFIRM.to_owned(),
        };
        let armed = self.danger_step.armed();
        let heading = DANGER_ZONE_LABEL.to_ascii_uppercase();
        // The head and the buttons share one row of the form's own grid: the head
        // in the label column and the actions in the control column, with the
        // spacer a row carries between its two columns rather than a pair of
        // `gap`s — the two of them together would spend the 48px the gutter
        // already is and push the last button 16px past the measure's trailing
        // edge. So the pair of buttons starts on the same spine as every control
        // on the page and ends on the same line. They grow from that edge and wrap
        // rather than overflow the lane, which is what keeps `Cancel` — which only
        // exists once a step is armed — from pushing the destructive commit off
        // the measure.
        let row = h_flex()
            .w_full()
            .px(SETTINGS_PAGE_INSET)
            // The hairline is the zone's own boundary and the only one it draws.
            // The zone used to be a caption and two buttons with nothing around
            // them, which read as the bottom of the page rather than as a region
            // whose contents are the two things that cannot be undone — the one
            // place on this surface where a rule is doing safety work instead of
            // decoration.
            .border_t_1()
            .border_color(design::role::border_subtle(cx))
            .pt(space::XL)
            // Top-aligned, because the actions can be two lines tall: a centred
            // head would float between the two buttons instead of naming the
            // first one. The head column carries its own control height so the
            // label's line box is centred against *that* button rather than
            // against the stack.
            .items_start()
            .child(
                v_flex()
                    .id("settings-danger-zone-text")
                    .w(px(SETTINGS_LABEL_WIDTH))
                    .flex_none()
                    .min_h(design::size::CONTROL)
                    .justify_center()
                    // The same group-head treatment every section on this surface
                    // uses, and deliberately not in the danger hue: the label is
                    // structure, and only the destructive button is a statement
                    // about severity.
                    .child(
                        Label::new(heading)
                            .text_size(design::text::CAPTION)
                            .line_height(design::text::CAPTION_LINE_HEIGHT)
                            .font_weight(design::text::SEMIBOLD)
                            .text_color(design::role::fg_tertiary(cx)),
                    ),
            )
            // The same spacer floor a row carries, so the two lanes cannot be
            // pulled apart by a wide window: a row's label and control columns
            // and this row's head and actions then resolve to the same two x
            // values, and `space::LG` here was eight pixels narrower than the
            // rows' `space::XL`, which put the destructive button left of every
            // other control on the page.
            .child(div().flex_1().min_w(px(f32::from(space::XL))))
            .child(
                h_flex()
                    .id("settings-danger-zone-actions")
                    .w(px(SETTINGS_CONTROL_WIDTH))
                    .flex_none()
                    // `justify_end`, the same as `setting_control`, and for the same reason: the
                    // value column holds the measure's *trailing* edge so every control on the
                    // page ends on one line. This row was `justify_start`, so `Clear cache` and
                    // `Reset all settings` began 72px left of every dropdown beside them while
                    // claiming the same lane. Wrapping still happens for the armed confirmations,
                    // and it wraps leftward from the same line, so the lane is preserved when
                    // there is more than one row of it.
                    .justify_end()
                    .flex_wrap()
                    .items_center()
                    .gap(space::SM)
                    .when_some(cancel, |buttons, cancel| buttons.child(cancel))
                    .child(
                        div()
                            .flex_none()
                            .debug_selector(|| "settings-danger-clear-cache".to_owned())
                            .child(clear_button),
                    )
                    .child(
                        div()
                            .flex_none()
                            .debug_selector(|| "settings-danger-reset-all".to_owned())
                            .child(reset_button),
                    ),
            );
        let selector = "settings-danger-zone";
        // The caption sits below that row on the *full* measure rather than in the
        // label column beside it. It is a sentence about the two buttons, and in a
        // 288px column it wrapped onto two short lines with 500px of window above
        // it — the most cramped text on the page, explaining the least reversible
        // thing on it. On the measure it is one line of `text::LABEL` and the
        // armed confirmations, which are two and four times this long, still fit
        // in two and four lines of the page's own width.
        div()
            .id("settings-danger-zone")
            .debug_selector(move || selector.to_owned())
            .w_full()
            .flex_none()
            .child(
                v_flex()
                    .id("settings-danger-zone-box")
                    .debug_selector(|| "settings-danger-zone-box".to_owned())
                    .w_full()
                    .gap(space::SM)
                    .role(Role::Region)
                    .aria_label(DANGER_ZONE_LABEL)
                    .aria_description(caption.clone())
                    .child(row)
                    .child(
                        Label::new(caption)
                            .w_full()
                            .px(SETTINGS_PAGE_INSET)
                            .text_size(design::text::LABEL)
                            .line_height(design::text::LABEL_LINE_HEIGHT)
                            // A confirmation is not muted copy: while a step is
                            // armed this sentence *is* the consequence, so it
                            // takes the readable ink rather than the quiet one.
                            .text_color(if armed {
                                design::role::fg_secondary(cx)
                            } else {
                                design::role::fg_tertiary(cx)
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The search field builds its own focus handle with its state, on the
        // first frame the field draws, and the constructor only has the
        // placeholder it starts with. The boxes that track the fields have to be
        // given the handle the field itself registers, or a tab slot is taken
        // twice: once by the field and once by a handle nothing focuses.
        let search_focus = self.search_input.read(cx).focus_handle(cx);
        if search_focus != self.search_focus {
            self.search_focus = search_focus;
            cx.notify();
        }
        let viewport = window.viewport_size().width;
        let rail = settings_nav_rail(f32::from(viewport));
        let compact = compact_settings_layout(f32::from(viewport), rail);
        if compact != self.compact {
            self.compact = compact;
            cx.notify();
        }
        // Where the page and the chrome start, resolved once per frame from the
        // window's own width. With the rail up the page is left-aligned inside
        // its own pane at the measure and carries no further inset — the rows
        // bring their own page padding, and a second one here would double the
        // gutter between the rail's rule and the first label — while the chrome
        // bands start over the rail's own width so the title and the search
        // field share the rows' leading edge. Without the rail the window is
        // narrow enough that the centred measure is the whole of the layout.
        let (measure_inset, chrome_inset) = if rail {
            (px(0.), px(SETTINGS_NAV_WIDTH))
        } else {
            let centred = px((f32::from(viewport) - SETTINGS_CONTENT_MAX_WIDTH).max(0.) / 2.);
            (centred, centred)
        };
        if measure_inset != self.measure_inset || chrome_inset != self.chrome_inset {
            self.measure_inset = measure_inset;
            self.chrome_inset = chrome_inset;
            cx.notify();
        }
        if rail != self.nav_rail {
            self.nav_rail = rail;
            cx.notify();
        }
        // A window of its own names the pane it is showing, so the desktop's
        // window list can tell Settings from a table of Pods. A tab does not:
        // the shell names the tab and the main window keeps its own title.
        if self.owns_window {
            let title = self.pane_title();
            if self.window_title.as_deref() != Some(title.as_str()) {
                self.window_title = Some(title.clone());
                window.set_window_title(&title);
            }
        }
        let strip = self.render_status_strip(cx);
        let recording = self.render_recording_status(cx);
        let title = self.render_title_bar(cx);
        let search = self.render_search_row(cx);
        let content = self.render_content(window, cx);
        let scroll = div()
            .id("settings-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(content);
        // The window is a chrome band, and below it the navigation and the page
        // side by side — the same shape the main window has, and the shape that
        // stops a 640px form from floating in the middle of a 4K field with the
        // only navigation in a 40px strip above it. Below the width where the
        // rail and the measure both fit whole, the categories fall back to that
        // strip and the page keeps the centred measure it had.
        let page = v_flex()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .when(!self.nav_rail, |this| {
                this.child(self.render_tabs(window, cx))
            })
            .child(scroll);
        v_flex()
            .size_full()
            .min_w(px(0.))
            .bg(design::role::surface_app(cx))
            .text_color(design::role::fg_primary(cx))
            .key_context("Settings")
            .tab_group()
            .track_focus(&self.recording_focus)
            .on_key_down(cx.listener(Self::on_settings_key_down))
            .on_action(cx.listener(Self::on_open_shortcut_reference))
            .child(title)
            .when_some(recording, |this, status| this.child(status))
            .when_some(strip, |this, strip| this.child(strip))
            .child(search)
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    // `h_flex` centres its children on the cross axis, so
                    // without this the rail and the page take their content's
                    // height and float in the middle of the window: the page
                    // stops filling the window, `settings-scroll` stops
                    // scrolling, and a page taller than the window is centred
                    // out of both edges instead of scrolling.
                    .items_stretch()
                    .when(self.nav_rail, |this| {
                        this.child(self.render_nav_rail(window, cx))
                    })
                    .child(page),
            )
    }
}

// ---------------------------------------------------------------------------
// The keymap
// ---------------------------------------------------------------------------

/// The Inspector section of the built-in keymap.
///
/// The Inspector is a sibling of the Table, so its keys live in their own
/// context and an edit or a clear has to name this section: an override written
/// without a context is global, and a global Inspector shortcut would also fire
/// outside the panel.
const INSPECTOR_CONTEXT: &str = "Inspector && !CommandPalette";
/// The recording rule, in the words the error message uses, so the hint and the
/// failure agree.
const RECORDING_REJECTED_KEYS: &str = "a letter or number without a modifier, a bare F1, a bare key above F24, or an unmodified Home, End, Page Up, Page Down, arrow, Enter, Space, Tab, Insert, Backspace, or Delete key";

/// Bare keys the recorder refuses, with the name the error message gives each.
///
/// The rule and the sentence a person reads after pressing a refused key read
/// this one table, so a key the recorder turns down is always named in the
/// message that explains why. A key the rule refuses and the copy does not
/// mention is the worst of the three outcomes: the person presses it again and
/// learns nothing.
const RECORDING_REFUSED_BARE_KEYS: &[(&str, &str)] = &[
    (" ", "Space"),
    ("space", "Space"),
    ("enter", "Enter"),
    ("return", "Enter"),
    ("tab", "Tab"),
    ("up", "arrow"),
    ("down", "arrow"),
    ("left", "arrow"),
    ("right", "arrow"),
    ("arrowup", "arrow"),
    ("arrowdown", "arrow"),
    ("arrowleft", "arrow"),
    ("arrowright", "arrow"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Page Up"),
    ("pagedown", "Page Down"),
    ("insert", "Insert"),
    ("backspace", "Backspace"),
    ("delete", "Delete"),
];

/// A keycap, the recording hint, or the honest "no shortcut" line.
///
/// An unbound action says so in words rather than in colour alone, and a key the
/// recorder cannot take is never drawn as if it were bound.
fn keycap_element_from_chord(chord: Option<&str>, recording: bool, cx: &App) -> AnyElement {
    if recording {
        return label_small(recording_hint())
            .text_color(design::role::fg_tertiary(cx))
            .into_any_element();
    }
    if let Some(keycap) = chord.and_then(keycap_from_chord) {
        return keycap;
    }
    label_small("No shortcut assigned.")
        .text_color(design::role::fg_tertiary(cx))
        .into_any_element()
}

/// Turns a key chord such as `ctrl-shift-k` into the keycap element, or nothing
/// when the chord cannot be parsed, so a malformed keymap file cannot put a raw
/// string in a keycap.
fn keycap_from_chord(chord: &str) -> Option<AnyElement> {
    let keystroke = Keystroke::parse(chord).ok()?;
    Some(Kbd::new(keystroke).into_any_element())
}

/// The bound actions the reference does not name because the recorder would
/// refuse their key.
///
/// A row advertises a shortcut the app can write, so a binding on a key the
/// recorder rejects has nothing to offer: naming it would put a keycap in the
/// list that the Edit control cannot record and Clear cannot remove.
#[cfg(test)]
const BOUND_ON_KEYS_THE_RECORDER_REFUSES: &[(&str, &str)] = &[
    (
        "k8s_shell::Dismiss",
        "escape answers every dismissal, and the recorder refuses it",
    ),
    (
        "k8s_shell::FocusNext",
        "tab is the platform focus order, and the recorder refuses it",
    ),
    (
        "k8s_shell::FocusPrevious",
        "shift-tab walks the same focus order",
    ),
    (
        "k8s_table::SelectNext",
        "down is arrow navigation, which the recorder refuses",
    ),
    (
        "k8s_table::SelectPrevious",
        "up is arrow navigation, which the recorder refuses",
    ),
    (
        "k8s_table::SelectNextColumn",
        "tab moves between the columns",
    ),
];

/// The bound actions that have no row and a key the recorder accepts.
///
/// The reference is built from the command palette, so a binding the palette
/// does not mention has no row: the shortcut exists and nobody can find it or
/// change it. Each entry needs a `shell::commands` entry before the list can
/// name it, and the test below fails the moment a new binding lands here
/// unlisted.
#[cfg(test)]
const BOUND_WITHOUT_A_ROW: &[(&str, &str)] = &[
    (
        "k8s_table::SelectPreviousColumn",
        "half of the Tab chord pair: a cursor step rather than a command, and a row for it alone would send the reader looking for where next column went",
    ),
    (
        "k8s_app::CloseWindow",
        "the window close chord belongs to the window controls too",
    ),
];

/// The macOS application-menu chords, which no other platform binds.
///
/// They are bound only where the macOS menu bar exists, and the menu bar prints
/// each of them beside the name the person is clicking, so a reference row would
/// answer "No shortcut assigned." on every platform but the one that has the
/// chord. The list is empty off macOS so the check below is about this build's
/// keymap rather than about the keymap of another platform.
#[cfg(test)]
#[cfg(target_os = "macos")]
const APPLE_MENU_CHORDS: &[&str] = &[
    "k8s_app::Quit",
    "k8s_app::Hide",
    "k8s_app::HideOthers",
    "k8s_app::ShowAll",
    "k8s_app::MinimizeWindow",
    "k8s_app::ToggleFullScreen",
];
#[cfg(test)]
#[cfg(not(target_os = "macos"))]
const APPLE_MENU_CHORDS: &[&str] = &[];

/// The chord a reference row shows, or `None` when the action has no binding in
/// its context.
///
/// `keymap::binding_for_context` is the shared lookup: it honours the context
/// predicate and skips a binding the surface released with an `unbind` row, so
/// a row never advertises a key that will not fire. It reads a focus path
/// rather than a context expression, so the row's own section goes through
/// [`section_focus_path`] first. A parameterized action has one row per input
/// and keeps the instance lookup, because the shared helper matches by action
/// name and would answer with another input's chord.
fn command_chord(command: &KeyboardCommand, cx: &App) -> Option<String> {
    if command.action_input.is_some() {
        return action_for_command(command, cx)
            .ok()
            .and_then(|action| {
                current_binding_for_action(action.as_ref(), command.context.as_deref(), cx)
            })
            .and_then(|binding| binding.keystrokes().first().map(|key| key.unparse()));
    }
    let context = command
        .context
        .clone()
        .or_else(|| binding_context_for_action(&command.action_name, cx))
        .unwrap_or_default();
    keymap::binding_for_context(&command.action_name, &section_focus_path(&context), cx)
}

/// The focus path a keymap section answers for, so the shared lookup gets the
/// path it reads.
///
/// A section is a predicate over focus paths (`Shell && !CommandPalette`), and
/// `keymap::binding_for_context` evaluates a predicate against the path a hint
/// sits on (`Shell Table`). Handing it the section names the negated surfaces as
/// if they were focused, so the predicate refuses the very section that names it
/// and every row loses its keycap. The path is the section's own surfaces: the
/// negations stay in the predicate, which is what checks them.
fn section_focus_path(context: &str) -> String {
    let mut path: Vec<&str> = Vec::new();
    let mut negated = false;
    for term in context.split_whitespace() {
        if term == "&&" || term == "||" {
            // A term boundary ends the negation, so `!Terminal && Shell` still
            // keeps the Shell.
            negated = false;
        } else if term.starts_with('!') {
            // A negated name is a condition on the path, not a surface that can
            // hold focus.
            negated = true;
        } else if !negated {
            path.push(term);
        }
    }
    path.join(" ")
}

/// The context of the action's last binding.
///
/// A command whose canonical context is unknown is bound in exactly one place,
/// so its own binding names the surface the shortcut belongs to.
fn binding_context_for_action(action_name: &str, cx: &App) -> Option<String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings()
        .filter(|binding| binding.action().name() == action_name)
        .find_map(|binding| binding.predicate().map(|predicate| predicate.to_string()))
}

/// The conflict this row owns, if the keymap reports one.
///
/// `feedback.md` › Best practices asks status to sit next to the thing it
/// describes, so a key that two actions claim is reported on the row that shows
/// that key, not only in a toast about a surface the person may not be on.
fn conflict_for_command(command: &KeyboardCommand, cx: &App) -> Option<BindingConflict> {
    keymap::status(cx)
        .conflicts
        .iter()
        .find(|conflict| {
            conflict
                .actions
                .iter()
                .any(|entry| conflict_entry_matches(entry, command))
        })
        .map(|conflict: &KeyConflict| {
            let this = conflict_action_key(command);
            BindingConflict {
                keystrokes: conflict.keystrokes.clone(),
                others: conflict
                    .actions
                    .iter()
                    .filter(|entry| **entry != this)
                    .cloned()
                    .collect(),
                context: conflict.context.clone(),
            }
        })
}

/// A conflict entry is `name` or `name {json}`. Comparing the parsed input keeps
/// whitespace in the keymap file from hiding a conflict on a parameterized row.
fn conflict_entry_matches(entry: &str, command: &KeyboardCommand) -> bool {
    let Some((name, input)) = entry.split_once(' ') else {
        return command.action_name == entry && command.action_input.is_none();
    };
    if command.action_name != name {
        return false;
    }
    let Some(expected) = command.action_input.as_deref() else {
        return false;
    };
    let left = serde_json::from_str::<serde_json::Value>(expected);
    let right = serde_json::from_str::<serde_json::Value>(input);
    matches!((left, right), (Ok(left), Ok(right)) if left == right)
}

fn conflict_action_key(command: &KeyboardCommand) -> String {
    match command.action_input.as_deref() {
        Some(input) => format!("{} {input}", command.action_name),
        None => command.action_name.clone(),
    }
}

fn recording_keystroke_allowed(keystroke: &Keystroke) -> bool {
    let key = keystroke.key.to_ascii_lowercase();
    if key == "escape" || key.is_empty() {
        return false;
    }
    // A modifier is what makes a key a shortcut instead of a character, so a
    // modified keystroke always qualifies: `ctrl-f1` spends no text, and the
    // only F1 the app owns is the bare one that `KEYMAP.md` §4.1 gives to the
    // diagnostics frame overlay.
    if keystroke.modifiers.modified() {
        return true;
    }
    // A bare function key types nothing, so F2 through F24 are the one
    // unmodified key that can carry a shortcut on their own.
    if let Some(number) = key.strip_prefix('f') {
        return !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && matches!(number.parse::<u8>(), Ok(value) if (2..=24).contains(&value));
    }
    let mut characters = key.chars();
    if characters
        .next()
        .is_some_and(|character| character.is_alphanumeric())
        && characters.next().is_none()
    {
        return false;
    }
    if key.chars().count() > 1 && key.chars().all(|character| character.is_ascii_alphabetic()) {
        return false;
    }
    !RECORDING_REFUSED_BARE_KEYS
        .iter()
        .any(|(refused, _)| *refused == key.as_str())
}

/// The context an action is looked up in, or `None` when the keymap binds it
/// globally.
///
/// A row shows the chord its own surface can reach, and its Edit and Clear
/// controls write into the same section, so an action that the keymap binds
/// inside a context needs an entry here. A key without one falls back to the
/// context of the action's own last binding, which is only the right answer
/// while the action is bound in exactly one place.
fn canonical_context(action_name: &str) -> Option<&'static str> {
    match action_name {
        "k8s_app::OpenSettings"
        | "k8s_shell::SearchResources"
        | "k8s_shell::ToggleNotifications"
        | "k8s_shell::ReloadKubeconfigs"
        | "k8s_shell::ToggleLeftPanel"
        | "k8s_shell::ToggleRightPanel"
        | "k8s_shell::ToggleDock" => Some("!CommandPalette"),
        // The three switchers give way to a focused session, so they carry the
        // extra condition the keymap uses.
        "k8s_shell::OpenContextSwitcher"
        | "k8s_shell::OpenNamespaceSwitcher"
        | "k8s_shell::OpenResourceKindSwitcher" => {
            Some("Shell && !CommandPalette && !Terminal")
        }
        "k8s_shell::CloseTab"
        | "k8s_shell::CloseOtherTabs"
        | "k8s_shell::CloseAllTabs"
        | "k8s_shell::NextTab"
        | "k8s_shell::PreviousTab"
        | "k8s_shell::SwitchTab"
        | "k8s_shell::FocusYaml"
        | "k8s_shell::TogglePinTab"
        | "k8s_shell::MoveTabLeft"
        | "k8s_shell::MoveTabRight"
        | "k8s_shell::OpenOverview"
        | "k8s_shell::ApplyYaml"
        | "k8s_shell::OpenEvents"
        | "k8s_shell::OpenForwards"
        | "k8s_shell::RestartSelection"
        | "k8s_shell::ExecSelection"
        | "k8s_shell::PortForwardSelection"
        | "k8s_shell::ScaleSelection"
        | "k8s_shell::ReloadKeymap"
        | "k8s_shell::OpenLogs"
        | "k8s_shell::ToggleTheme"
        // The commands on the selected row are keyed in the shell block next to
        // Exec and Apply, so a row that looked them up in the table would
        // advertise no key and would write an override into a section the
        // built-in binding does not use.
        | "k8s_shell::DescribeSelection"
        | "k8s_shell::PauseUpdates"
        | "k8s_shell::ResumeUpdates" => Some("Shell && !CommandPalette"),
        // Copy is the one command on the selected row that is *not* keyed in the
        // shell block: every platform keymap binds it in the table, so a table
        // keeps the characters it owes the Editor and the Terminal. Reading it
        // out of the shell context found nothing, which left the reference row
        // showing no key at all for the one chord a reader is most likely to
        // press, and would have written a user override into a section the
        // built-in binding does not use.
        "k8s_shell::CopySelectedPodName" => Some("Table && !CommandPalette"),
        // The table's own keys. These three name no palette row, so nothing else
        // puts them in a section, and a row that looked them up globally would
        // advertise no key and would write an override that fires everywhere.
        "k8s_table::OpenDetails"
        | "k8s_table::OpenRowActions"
        | "k8s_ops::DeleteSelection"
        | "k8s_ops::Refresh" => Some("Table && !CommandPalette"),
        // `k8s_shell::RefreshView` and `k8s_shell::OpenServiceAccount` are
        // deliberately absent here. Both are in `keymap::UNBOUND_ACTIONS`, so no
        // preset binds them anywhere, and a row naming the table for them would
        // advertise a chord that cannot fire in any of them.
        // Every Inspector key belongs to the panel: it is the only surface that
        // can act on it.
        "k8s_inspector::ReloadActiveTab"
        | "k8s_inspector::RetryMetrics"
        | "k8s_inspector::MetricsRange1m"
        | "k8s_inspector::MetricsRange15m"
        | "k8s_inspector::MetricsRange1h"
        | "k8s_inspector::MetricsRange6h"
        | "k8s_inspector::MetricsRange24h"
        | "k8s_inspector::MetricsRange7d"
        | "k8s_inspector::ConfirmApply"
        | "k8s_inspector::CancelApplyReview"
        | "k8s_inspector::RevertYaml"
        | "k8s_inspector::CopyYaml"
        | "k8s_inspector::ToggleValueExpansion"
        | "k8s_inspector::CopyValue"
        | "k8s_inspector::NextProblem" => Some(INSPECTOR_CONTEXT),
        "k8s_shell::SwitchCluster" => Some("Shell && !CommandPalette"),
        _ => None,
    }
}

fn binding_matches_context(binding: &KeyBinding, context: Option<&str>) -> bool {
    match context {
        None => binding.predicate().is_none(),
        Some(context) => keymap::binding_context(binding).as_deref() == Some(context),
    }
}

fn current_binding_for_action(
    action: &dyn Action,
    context: Option<&str>,
    cx: &App,
) -> Option<KeyBinding> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings_for_action(action)
        .rfind(|binding| binding_matches_context(binding, context))
        .cloned()
}

fn command_context(action_name: &str, binding: Option<&KeyBinding>) -> Option<String> {
    binding
        .and_then(keymap::binding_context)
        .or_else(|| canonical_context(action_name).map(str::to_owned))
}

fn action_for_command(command: &KeyboardCommand, cx: &App) -> Result<Box<dyn Action>, String> {
    let action_input = command
        .action_input
        .as_deref()
        .map(|input| {
            serde_json::from_str(input).map_err(|_| {
                format!(
                    "Shortcut data for {} is invalid. Reload the keymap, then try again.",
                    command.label
                )
            })
        })
        .transpose()?;
    cx.build_action(&command.action_name, action_input)
        .map_err(|_| {
            format!(
                "The {} action is unavailable. Update K8s Studio or use another command.",
                command.label
            )
        })
}

fn keyboard_command_label(action_name: &str, label: &str) -> String {
    match action_name {
        "k8s_shell::PortForwardSelection" => "Start port forward".to_owned(),
        _ => label.to_owned(),
    }
}

fn keyboard_context_description(context: &str) -> &str {
    match context {
        "Shell && !CommandPalette" => "the main workbench outside the Command Palette",
        "Shell && !CommandPalette && !Terminal" => {
            "the main workbench outside the Command Palette and a terminal"
        }
        "Table && !CommandPalette" => "a resource table outside the Command Palette",
        INSPECTOR_CONTEXT => "the resource Inspector outside the Command Palette",
        "!CommandPalette" => "outside the Command Palette",
        _ => context,
    }
}

/// The one sentence a reference row shows under its label.
///
/// `writing.md` asks a label to describe what it does, and a row is a person
/// deciding whether to rebind a key, so the sentence has to be about this
/// command rather than about the verb "run". The table used to end in
/// `_ => format!("Run {label}.")`, and 33 of 68 rows reached it, so
/// `Scale Selection` described a fail-closed command as something it does.
/// `Command` in `shell/commands.rs` has no `description` field, so there is no
/// registry sentence to reuse and the table below is the whole vocabulary;
/// `every_listed_action_says_what_it_does` fails when a new action lands in the
/// list without one.
fn keyboard_description(action_name: &str, label: &str) -> String {
    match action_name {
        // Tabs.
        "k8s_shell::CloseTab" => "Close the active center tab.".to_owned(),
        "k8s_shell::CloseOtherTabs" => "Close every other center tab.".to_owned(),
        "k8s_shell::CloseAllTabs" => "Close all center tabs.".to_owned(),
        "k8s_shell::NextTab" => "Open the next center tab.".to_owned(),
        "k8s_shell::PreviousTab" => "Open the previous center tab.".to_owned(),
        "k8s_shell::MoveTabLeft" => "Move the active center tab one position left.".to_owned(),
        "k8s_shell::MoveTabRight" => "Move the active center tab one position right.".to_owned(),
        "k8s_shell::TogglePinTab" => "Pin or unpin the active center tab.".to_owned(),
        // The selected resource.
        "k8s_shell::RefreshView" => "Refresh the current resource view.".to_owned(),
        "k8s_ops::Refresh" => "Fetch the selected resource view again.".to_owned(),
        "k8s_shell::DescribeSelection" => "Describe the selected resource.".to_owned(),
        "k8s_shell::OpenServiceAccount" => {
            "Open the Service Account used by the selected Pod.".to_owned()
        }
        "k8s_shell::FocusYaml" => "Open editable YAML for the selected resource.".to_owned(),
        "k8s_shell::ApplyYaml" => "Write the reviewed YAML changes to the cluster.".to_owned(),
        "k8s_shell::ExecSelection" => "Open a terminal in the selected pod.".to_owned(),
        "k8s_shell::RestartSelection" => "Restart the selected pods or workloads.".to_owned(),
        // UI Zoom is fail-closed by contract, so the one row that documents it
        // has to say that rather than describe an action. `DESIGN.md` §3.1
        // requires triggering it to change no font size, spacing, control or
        // window size, only to show why it cannot run.
        "k8s_shell::ScaleSelection" => "UI zoom is not available in this build.".to_owned(),
        "k8s_shell::CopySelectedPodName" => {
            "Copy the selected pod name to the clipboard.".to_owned()
        }
        "k8s_shell::PortForwardSelection" => {
            "Start a port forward for the selected pod.".to_owned()
        }
        "k8s_shell::OpenLogs" => "Show the logs of the selected pod.".to_owned(),
        "k8s_shell::OpenEvents" => "Show the events of the selected pod.".to_owned(),
        "k8s_shell::PauseUpdates" => "Pause live updates for the current resource view.".to_owned(),
        "k8s_shell::ResumeUpdates" => {
            "Resume live updates for the current resource view.".to_owned()
        }
        // The table.
        "k8s_table::OpenRowActions" => "Open the actions for the selected row.".to_owned(),
        "k8s_table::OpenDetails" => "Open the selected resource in the Inspector.".to_owned(),
        "k8s_ops::DeleteSelection" => "Delete the selected resource from the cluster.".to_owned(),
        "k8s_table::SortSelectedColumn" => "Sort the selected column.".to_owned(),
        "k8s_table::ToggleProblemsOnly" => {
            "Show only the rows that need attention in the current table.".to_owned()
        }
        // The standard editing commands. They are palette rows so a person can
        // find out what `Ctrl-C` does here, and each one says which selection it
        // acts on.
        "k8s_shell::Undo" => "Undo the last change.".to_owned(),
        "k8s_shell::Redo" => "Redo the change that was undone.".to_owned(),
        "k8s_shell::Cut" => "Cut the selection to the clipboard.".to_owned(),
        "k8s_shell::Copy" => "Copy the selection to the clipboard.".to_owned(),
        "k8s_shell::Paste" => "Paste the clipboard into the focused field.".to_owned(),
        "k8s_shell::SelectAll" => "Select everything in the focused field.".to_owned(),
        // Navigation and panels.
        "k8s_shell::OpenOverview" => "Open the cluster Overview.".to_owned(),
        "k8s_shell::OpenForwards" => "Open the port forward management view.".to_owned(),
        "k8s_shell::OpenContextSwitcher" => {
            "Switch to another configured cluster context.".to_owned()
        }
        "k8s_shell::OpenNamespaceSwitcher" => {
            "Switch to another namespace in the current cluster.".to_owned()
        }
        "k8s_shell::OpenResourceKindSwitcher" => "Open a different resource view.".to_owned(),
        "k8s_shell::SearchResources" => {
            "Search resource names across the current cluster.".to_owned()
        }
        "k8s_shell::ToggleCommandPalette" => "Show or hide the command palette.".to_owned(),
        "k8s_shell::ToggleLeftPanel" => "Show or hide the resource sidebar.".to_owned(),
        "k8s_shell::ToggleRightPanel" => "Show or hide the resource inspector.".to_owned(),
        "k8s_shell::ToggleDock" => "Show or hide the bottom dock.".to_owned(),
        "k8s_shell::ToggleNotifications" => "Show or hide recent notifications.".to_owned(),
        "k8s_shell::ReloadKubeconfigs" => "Reload the configured kubeconfig files.".to_owned(),
        "k8s_shell::ReloadKeymap" => {
            "Reload the user keymap and apply the saved shortcuts.".to_owned()
        }
        "k8s_shell::UseKeymapPreset" => "Replace the user keymap with a named preset.".to_owned(),
        // Theme. The three actions live in the `k8s_shell` namespace, so a table
        // keyed on `k8s_app::Use…` matched nothing and both rows fell through to
        // the generic sentence.
        "k8s_shell::ToggleTheme" => "Switch between the light and dark themes.".to_owned(),
        "k8s_shell::UseLightTheme" => "Switch the workbench to the light theme.".to_owned(),
        "k8s_shell::UseDarkTheme" => "Switch the workbench to the dark theme.".to_owned(),
        "k8s_shell::UseSystemTheme" => "Follow the desktop's light or dark setting.".to_owned(),
        // Application.
        "k8s_app::OpenSettings" => "Open K8s Studio Settings.".to_owned(),
        "k8s_shell::OpenShortcutReference" => {
            "Open the list of every command and the key it answers to.".to_owned()
        }
        "k8s_app::CheckForUpdates" => "Check for a newer K8s Studio release.".to_owned(),
        "k8s_app::RestartToUpdate" => "Restart K8s Studio after an update is ready.".to_owned(),
        "k8s_inspector::ReloadActiveTab" => {
            "Reload the data of the active tab in the Inspector.".to_owned()
        }
        "k8s_inspector::RetryMetrics" => "Retry the failed metrics request.".to_owned(),
        "k8s_inspector::MetricsRange1m" => "Show the last minute of metrics.".to_owned(),
        "k8s_inspector::MetricsRange15m" => "Show the last 15 minutes of metrics.".to_owned(),
        "k8s_inspector::MetricsRange1h" => "Show the last hour of metrics.".to_owned(),
        "k8s_inspector::MetricsRange6h" => "Show the last 6 hours of metrics.".to_owned(),
        "k8s_inspector::MetricsRange24h" => "Show the last day of metrics.".to_owned(),
        "k8s_inspector::MetricsRange7d" => "Show the last week of metrics.".to_owned(),
        "k8s_inspector::ConfirmApply" => "Write the reviewed change to the cluster.".to_owned(),
        "k8s_inspector::CancelApplyReview" => {
            "Close the review without writing to the cluster.".to_owned()
        }
        "k8s_inspector::RevertYaml" => "Restore the YAML the cluster last reported.".to_owned(),
        "k8s_inspector::CopyYaml" => "Copy the YAML in the editor.".to_owned(),
        "k8s_inspector::ToggleValueExpansion" => {
            "Show the full value of the selected row, or collapse it again.".to_owned()
        }
        "k8s_inspector::CopyValue" => "Copy the selected value to the clipboard.".to_owned(),
        "k8s_inspector::NextProblem" => "Move to the next YAML problem.".to_owned(),
        _ => format!("Run {label}."),
    }
}

fn append_keyboard_command(
    sections: &mut Vec<KeyboardSection>,
    title: impl Into<String>,
    command: KeyboardCommand,
) {
    let title = title.into();
    if let Some(section) = sections.iter_mut().find(|section| section.title == title) {
        section.commands.push(command);
    } else {
        sections.push(KeyboardSection {
            title,
            commands: vec![command],
        });
    }
}

fn parameterized_label(spec: &ParameterizedKeyboardAction, input: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(input).ok();
    let suffix = value
        .as_ref()
        .and_then(|value| value.get(spec.value_field))
        .and_then(|value| {
            value
                .as_u64()
                .map(|value| value.saturating_add(spec.value_offset).to_string())
                .or_else(|| value.as_str().map(|value| value.to_owned()))
        });
    suffix.map_or_else(
        || spec.label.to_owned(),
        |suffix| format!("{} {}", spec.label, suffix),
    )
}

/// The installed theme names, in the order the menu draws them.
///
/// The registry is a hash map, so the names are sorted here: a menu that
/// reordered itself between two frames would move a row out from under the
/// pointer that is on its way to it.
fn theme_names(cx: &App) -> Vec<String> {
    let mut names = ThemeRegistry::global(cx)
        .themes()
        .keys()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Keeps the view's open-menu slot in step with a popover the trigger owns.
///
/// The page decides which menu is open, so a new query or a new page can close
/// a menu whose trigger just left the screen; the popover reports its own state
/// back through here.
fn popover_open(
    view: WeakEntity<SettingsView>,
    setting: Setting,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    move |open, window, cx| {
        let next = open.then_some(OpenMenu(setting));
        if let Some(view) = view.upgrade() {
            view.update(cx, |view, cx| view.set_open_menu(next, window, cx));
        }
    }
}

fn append_parameterized_keyboard_commands(sections: &mut Vec<KeyboardSection>, cx: &App) {
    let specs = [
        ParameterizedKeyboardAction {
            action_name: "k8s_shell::SwitchTab",
            label: "Switch to Tab",
            group: "Tabs",
            description: "Switch to a specific center tab.",
            value_field: "index",
            value_offset: 1,
        },
        ParameterizedKeyboardAction {
            action_name: "k8s_shell::SwitchCluster",
            label: "Switch to Context",
            // The palette's own block name, so the reference and the palette stay
            // one arrangement. It was "Contexts", which is a plural nothing in
            // this product is grouped under - and the test that pins the reference
            // to the palette's blocks is exactly the thing that catches a block
            // invented on one side only. These chords switch CONTEXT, so Cluster
            // is where a reader looks for them.
            group: "Cluster",
            description: "Switch to a context by its place in the context switcher.",
            value_field: "index",
            value_offset: 1,
        },
    ];
    for spec in &specs {
        let inputs = {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let mut inputs = Vec::new();
            for binding in keymap.bindings() {
                if binding.action().name() != spec.action_name {
                    continue;
                }
                let Some(input) = binding.action_input() else {
                    continue;
                };
                let input = input.to_string();
                if !inputs.contains(&input) {
                    inputs.push(input);
                }
            }
            inputs
        };
        for input in inputs {
            let Ok(input_value) = serde_json::from_str::<serde_json::Value>(&input) else {
                continue;
            };
            let Ok(action) = cx.build_action(spec.action_name, Some(input_value)) else {
                continue;
            };
            let canonical = canonical_context(spec.action_name);
            let current = current_binding_for_action(action.as_ref(), canonical, cx);
            let command = KeyboardCommand {
                action_name: spec.action_name.to_owned(),
                label: parameterized_label(spec, &input),
                description: spec.description.to_owned(),
                editable: false,
                action_input: Some(input),
                context: command_context(spec.action_name, current.as_ref()),
                when: canonical.map(str::to_owned),
            };
            append_keyboard_command(sections, spec.group, command);
        }
    }
}

/// A bound command the command palette does not carry.
///
/// The reference is built from the palette, so a binding the palette does not
/// mention has no row unless it is named here.
struct BoundOutsidePalette {
    action_name: &'static str,
    label: &'static str,
    /// The palette block a person looks in, never the module the command lives
    /// in: somebody who wants to delete the Pod they have selected is looking
    /// under Resources, not under `k8s_ops`.
    group: &'static str,
    /// False where the recorder refuses the command's own key, so the row names
    /// the shortcut and points at the keymap file instead of offering an Edit
    /// that cannot record what the built-in keymap bound.
    editable: bool,
}

/// The bound commands the palette does not carry.
///
/// The palette is a list of *targets* — the cluster, the resource, the tab, the
/// panels — and this list shares those block names, so the reference and the
/// palette are two windows onto one arrangement. Naming these rows after the
/// module that owns them instead is what produced the reference's own `Inspector`
/// block: a heading no reader could ask for, holding one command.
const BOUND_OUTSIDE_THE_PALETTE: &[BoundOutsidePalette] = &[
    BoundOutsidePalette {
        action_name: "k8s_shell::ToggleCommandPalette",
        label: "Toggle command palette",
        group: "Application",
        editable: true,
    },
    // The reference has to name the command that opens it, or `?` is a key a
    // person can press and cannot find, which is the one thing a shortcut
    // reference exists to prevent.
    BoundOutsidePalette {
        action_name: "k8s_shell::OpenShortcutReference",
        label: "Open shortcut reference",
        group: "Application",
        editable: true,
    },
    BoundOutsidePalette {
        action_name: "k8s_app::OpenSettings",
        label: "Open settings",
        group: "Application",
        editable: true,
    },
    // The tab strip's own menu says all three of these in the words the reader
    // is choosing between, which is why the palette has no row for them; the
    // keymap binds all three, so the reference does.
    BoundOutsidePalette {
        action_name: "k8s_shell::CloseOtherTabs",
        label: "Close other tabs",
        group: "Tabs",
        editable: true,
    },
    BoundOutsidePalette {
        action_name: "k8s_shell::CloseAllTabs",
        label: "Close all tabs",
        group: "Tabs",
        editable: true,
    },
    BoundOutsidePalette {
        action_name: "k8s_shell::TogglePinTab",
        label: "Pin or unpin tab",
        group: "Tabs",
        editable: true,
    },
    // Everything below is what you do to the thing you have selected, which is
    // the question the `Resources` block already answers for logs, exec and
    // restart.
    BoundOutsidePalette {
        action_name: "k8s_ops::Refresh",
        label: "Refresh table",
        group: "Resources",
        editable: true,
    },
    BoundOutsidePalette {
        action_name: "k8s_table::OpenDetails",
        label: "Open selected resource",
        group: "Resources",
        // Enter and Space are the table's own keys and the recorder refuses both,
        // so this row names the shortcut and sends the reader to the keymap file.
        editable: false,
    },
    BoundOutsidePalette {
        action_name: "k8s_table::OpenRowActions",
        label: "Open row actions menu",
        group: "Resources",
        editable: true,
    },
    BoundOutsidePalette {
        action_name: "k8s_ops::DeleteSelection",
        label: "Delete selected resource",
        group: "Resources",
        // Delete is a text key, so a shortcut on it is unreachable from a text
        // surface and the recorder will not take it.
        editable: false,
    },
    // The review keeps Escape to itself and the strip's own button reads "Keep
    // editing", so neither the palette nor any menu names this one; Escape is
    // still a binding, and a reference that omits it hides the only way out of
    // a review.
    BoundOutsidePalette {
        action_name: "k8s_inspector::CancelApplyReview",
        label: "Cancel the Apply Review",
        group: "Resources",
        editable: true,
    },
];

fn keyboard_sections(cx: &App, updater_available: bool) -> Vec<KeyboardSection> {
    let mut sections: Vec<KeyboardSection> = Vec::new();
    for command in demo_commands_with_updater(true, updater_available) {
        let CommandRun::Action(make_action) = command.run else {
            continue;
        };
        let action = make_action();
        if !action.name().starts_with("k8s_")
            || !cx
                .all_action_names()
                .iter()
                .any(|registered| *registered == action.name())
        {
            continue;
        }
        let canonical = canonical_context(action.name());
        let current = current_binding_for_action(action.as_ref(), canonical, cx);
        let action_input = current
            .as_ref()
            .and_then(|binding| binding.action_input())
            .map(|input| input.to_string());
        let context = command_context(action.name(), current.as_ref());
        let editable = action_input.is_none() && cx.build_action(action.name(), None).is_ok();
        let keyboard_command = KeyboardCommand {
            action_name: action.name().to_owned(),
            label: keyboard_command_label(action.name(), command.label.as_ref()),
            description: keyboard_description(action.name(), command.label.as_ref()),
            editable,
            action_input,
            context,
            // The row has to say where the shortcut is live. `context` can be
            // empty for a globally bound action, and those rows correctly say
            // nothing; the others name a context here, and a list that named it
            // only on a conflict read as if the rest of the list had no scope at
            // all.
            when: canonical.map(str::to_owned),
        };
        append_keyboard_command(&mut sections, command.group.to_string(), keyboard_command);
    }
    for entry in BOUND_OUTSIDE_THE_PALETTE {
        if sections
            .iter()
            .flat_map(|section| section.commands.iter())
            .any(|command| command.action_name == entry.action_name)
            || !cx.all_action_names().contains(&entry.action_name)
        {
            continue;
        }
        let Ok(action) = cx.build_action(entry.action_name, None) else {
            continue;
        };
        let canonical = canonical_context(entry.action_name);
        let current = current_binding_for_action(action.as_ref(), canonical, cx);
        let action_input = current
            .as_ref()
            .and_then(|binding| binding.action_input())
            .map(|input| input.to_string());
        let context = command_context(entry.action_name, current.as_ref());
        let keyboard_command = KeyboardCommand {
            action_name: action.name().to_owned(),
            label: entry.label.to_owned(),
            description: keyboard_description(action.name(), entry.label),
            editable: entry.editable && action_input.is_none(),
            action_input,
            context,
            when: canonical.map(str::to_owned),
        };
        append_keyboard_command(&mut sections, entry.group, keyboard_command);
    }
    append_parameterized_keyboard_commands(&mut sections, cx);
    sections
}

/// Whether one command answers the reference's query.
///
/// The group a row is in is searchable as well as the row: a person looking for
/// "reload" should find it whether the word is in the command's name or in the
/// group it sits in.
/// Whether a command answers the query.
///
/// The same every-word rule as the settings rows, for the same reason and with
/// the same cost if it is skipped: `logs tab` and `tab logs` both have to reach
/// the command that opens logs in a tab, and a reference is searched harder than
/// a settings page because a reader arrives with a chord in mind and a name they
/// only half remember.
fn command_matches(command: &KeyboardCommand, group: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack =
        format!("{} {} {}", group, command.label, command.description).to_ascii_lowercase();
    query.split_whitespace().all(|word| haystack.contains(word))
}

/// The theme names in two labelled groups: the product themes first, then the
/// imported ones.
///
/// A single column of fourteen names with one rule reads as a flat list to scan.
/// Two groups let a person reach the K8s Studio theme without reading every
/// imported theme name, and an empty group is dropped so a build with only one
/// family does not get a heading with nothing under it.
fn theme_groups(names: Vec<String>) -> Vec<(&'static str, Vec<String>)> {
    let mut groups: Vec<(&'static str, Vec<String>)> =
        vec![("K8s Studio", Vec::new()), ("Zed", Vec::new())];
    for name in names {
        let group = usize::from(!name.starts_with(K8S_STUDIO_THEME_PREFIX));
        groups[group].1.push(name);
    }
    groups.retain(|(_, names)| !names.is_empty());
    groups
}

impl SettingsView {
    /// Opens one of the page's drop-down menus, or closes whichever one is open.
    ///
    /// The page decides which menu is open, because it replaces the rows
    /// underneath the menus: a menu left open across a page change would float
    /// over a row that no longer names it.
    fn set_open_menu(
        &mut self,
        menu: Option<OpenMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_menu == menu {
            return;
        }
        self.open_menu = menu;
        // The menu is built here and kept on the view rather than inside the
        // popover's content callback, which runs every frame: a menu rebuilt
        // each frame would drop the row the keyboard had highlighted, and the
        // focus the popover moved into it would never settle.
        let opened = menu.map(|menu| OpenMenuState {
            menu: self.build_popup_menu(menu.0, window, cx),
        });
        self.menu = opened;
        cx.notify();
    }

    /// The rows of one of the page's drop-down menus, in the order it draws them.
    ///
    /// `PopupMenu` owns the surface and its dismissal, so the app only describes
    /// the rows. The answer is a theme name or one of a fixed set of values, so
    /// each menu is a list with one row marked as the current answer.
    fn build_popup_menu(
        &self,
        setting: Setting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PopupMenu> {
        let view = cx.entity().downgrade();
        let current = if setting == Setting::Theme {
            theme_answer(cx)
        } else {
            self.read(setting, cx, &user_settings(cx)).as_text()
        };
        if setting == Setting::Theme {
            let names = theme_names(cx);
            return build_menu(window, cx, move |menu, _, _| {
                let view = view.clone();
                let current = current.clone();
                theme_menu_rows(menu, &view, &current, names)
            });
        }
        let choices = setting.choices(cx);
        build_menu(window, cx, move |menu, _, _| {
            let view = view.clone();
            let current = current.clone();
            let mut menu = menu;
            for choice in choices {
                let choice_view = view.clone();
                let checked = choice == current;
                menu = menu.item(
                    PopupMenuItem::new(choice.clone())
                        .checked(checked)
                        .on_click(move |_, window, cx| {
                            if let Some(view) = choice_view.upgrade() {
                                view.update(cx, |view, cx| {
                                    view.write_setting(
                                        setting,
                                        Value::Choice(choice.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }),
                );
            }
            menu
        })
    }
}

/// The rows of the theme menu: System first, then the two named groups.
fn theme_menu_rows(
    menu: PopupMenu,
    view: &WeakEntity<SettingsView>,
    current: &str,
    names: Vec<String>,
) -> PopupMenu {
    let system_view = view.clone();
    let menu = menu.item(
        PopupMenuItem::new(ThemeChoice::System.label())
            .checked(current == ThemeChoice::System.label())
            .on_click(move |_, window, cx| {
                if let Some(view) = system_view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.write_setting(
                            Setting::Theme,
                            Value::Choice(ThemeChoice::System.label().to_owned()),
                            window,
                            cx,
                        );
                    });
                }
            }),
    );
    theme_groups(names)
        .into_iter()
        .fold(menu.separator(), |menu, (title, names)| {
            let mut menu = menu.label(title).separator();
            for name in names {
                let choice = ThemeChoice::named(name);
                let choice_view = view.clone();
                menu = menu.item(
                    PopupMenuItem::new(choice.label())
                        .checked(*current == *choice.label())
                        .on_click(move |_, window, cx| {
                            if let Some(view) = choice_view.upgrade() {
                                let label = choice.label().to_owned();
                                view.update(cx, |view, cx| {
                                    view.write_setting(
                                        Setting::Theme,
                                        Value::Choice(label),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        }),
                );
            }
            menu
        })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext, size};

    /// Installs the globals the settings surface reads.
    ///
    /// This is the one `gpui_kit::init` call `main` makes, plus the app's own
    /// settings store. The component layer owns the theme the controls read
    /// their colours and the key bindings the pop-up menus dispatch through, so
    /// a test that skipped it would draw a settings page against no theme.
    fn init_app(cx: &mut App) {
        gpui_kit::init(cx);
        crate::settings::init(cx);
    }

    fn open(cx: &mut TestAppContext, width: f32) -> (Entity<SettingsView>, &mut VisualTestContext) {
        cx.update(init_app);
        mount(cx, width)
    }

    /// A window on an app that is already initialised.
    ///
    /// `gpui_kit::init` installs the theme registry and the settings store, and
    /// `settings::init` empties the remembered settings, so a test that needs
    /// either of them in place has to build the window *after* it put them there
    /// rather than through [`open`].
    fn mount(
        cx: &mut TestAppContext,
        width: f32,
    ) -> (Entity<SettingsView>, &mut VisualTestContext) {
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(size(px(width), px(900.)));
        cx.run_until_parked();
        (view, cx)
    }

    /// `debug_bounds` takes a `&'static str`, and the row selectors are built
    /// from the row's own title so that a renamed row cannot keep a stale test.
    fn bounds(
        cx: &mut VisualTestContext,
        selector: impl Into<String>,
    ) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        cx.debug_bounds(Box::leak(selector.into().into_boxed_str()))
    }

    fn spec_of(setting: Setting) -> &'static SettingSpec {
        setting.spec()
    }

    // -- the information architecture ---------------------------------------

    /// `UI-SPEC` §15.1's hard rule: no page holds more than eight rows.
    ///
    /// This is the one invariant on this screen that fails silently. Nine rows
    /// still render, still look fine, and still get a scrollbar — nothing
    /// complains, and the page quietly becomes the 6,211-line form again one
    /// setting at a time. So the count is asserted rather than trusted.
    #[gpui_kit::test]
    fn every_category_fits_in_eight_rows(_cx: &mut TestAppContext) {
        for category in SettingsCategory::ALL {
            let rows = SPECS
                .iter()
                .filter(|spec| spec.category == category)
                .count();
            assert!(
                rows <= 8,
                "{} holds {rows} rows; §15.1 says a ninth one means the information \
                 architecture is wrong, not that the page needs scrolling",
                category.label()
            );
            assert!(rows > 0, "{} has no rows at all", category.label());
        }
        // And the six categories of §15.1 are the six the rail draws, in order.
        assert_eq!(
            SettingsCategory::ALL.map(SettingsCategory::label),
            [
                "Appearance",
                "Keyboard",
                "Cluster",
                "Editor",
                "Data & privacy",
                "Updates"
            ]
        );
        // Every row belongs to exactly one page, and the list has no duplicates:
        // `Setting::spec` finds the first match, so a second entry for one
        // setting would leave half the file describing a row that is not drawn.
        let mut seen = std::collections::BTreeSet::new();
        for spec in SPECS {
            assert!(
                seen.insert(spec.setting),
                "{} is listed twice",
                spec.setting.title()
            );
            assert_eq!(spec.setting.category(), spec.category);
        }
        assert_eq!(seen.len(), SPECS.len());
    }

    /// The search box reaches every page.
    ///
    /// §15.2's third rule is that a search box is mandatory, and a search box
    /// that cannot reach a page is a page nobody can find. Naming a page is the
    /// cheapest query that has to work, so it is the one that is tested.
    #[gpui_kit::test]
    fn a_search_reaches_every_category(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for category in SettingsCategory::ALL {
            cx.update(|_, cx| {
                view.update(cx, |view, cx| view.set_search_query(category.label(), cx))
            });
            cx.run_until_parked();
            let matched = cx.update(|_, cx| view.read_with(cx, |view, cx| view.result_count(cx)));
            assert!(
                matched > 0,
                "searching “{}” found nothing on the {} page",
                category.label(),
                category.label()
            );
            assert!(
                cx.debug_bounds("settings-search-results").is_some(),
                "a query states how many rows it kept"
            );
        }
    }

    /// A reader looking for a word a row does not print still finds the row.
    ///
    /// The words in a row's paragraph are for the reader and the words in
    /// `keywords` are for the search box. A person looking for "vim" is looking
    /// for a row called "Vim mode", and a person looking for "size" is looking
    /// for "Text size" — neither query contains the row's own title, so the
    /// search would find nothing without the extra words.
    #[gpui_kit::test]
    fn a_search_finds_rows_by_the_words_they_do_not_print(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for (query, expected) in [
            ("vim", Setting::VimMode),
            ("size", Setting::TextSize),
            ("secret", Setting::RecordLogContent),
            ("timeout", Setting::ConnectionTimeout),
            ("indent", Setting::TabWidth),
            ("metrics", Setting::ClusterMetrics),
            ("quota", Setting::CacheLimit),
            ("release", Setting::UpdateChannel),
        ] {
            cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query(query, cx)));
            cx.run_until_parked();
            let found = cx.update(|_, cx| view.read_with(cx, |view, cx| view.results(cx)));
            assert!(
                found.iter().any(|(setting, _)| *setting == expected),
                "“{query}” did not find {}: {found:?}",
                expected.title()
            );
        }
    }

    /// Every enum row shows every legal value, and picks its shape from the
    /// number of them.
    ///
    /// `UI-SPEC` §15.3's closing line is that a free-text box for an enum is
    /// worse than no setting, because a reader who types `Comfy` into a field
    /// that silently rejects it learns nothing. The threshold is derived rather
    /// than declared, so a row cannot choose a shape for itself — and a row
    /// whose control is a text field or a number for an enum would be the exact
    /// failure the rule exists to prevent.
    #[gpui_kit::test]
    fn every_enum_row_offers_all_of_its_legal_values(cx: &mut TestAppContext) {
        cx.update(init_app);
        cx.update(|cx| {
            for spec in SPECS {
                let answers = spec.setting.choices(cx);
                if answers.is_empty() {
                    // Not an enum. A namespace and a folder path are identifiers
                    // with no list to read, and a boolean is a switch, so a row
                    // with no answers must not draw an empty choice control.
                    assert_ne!(
                        spec.setting.control(),
                        Control::Choices,
                        "{} draws an empty choice list",
                        spec.setting.title()
                    );
                    continue;
                }
                assert_eq!(
                    spec.setting.control(),
                    Control::Choices,
                    "{} has {} answers and is not answered by a control that shows them",
                    spec.setting.title(),
                    answers.len()
                );
                assert!(
                    answers.len() >= 2,
                    "{} offers one answer, so it is neither a switch nor a choice",
                    spec.setting.title()
                );
                let mut unique = answers.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(
                    unique.len(),
                    answers.len(),
                    "{} offers a duplicate answer",
                    spec.setting.title()
                );
                assert_eq!(
                    Control::segmented(answers.len()),
                    answers.len() <= 4,
                    "{} has {} answers",
                    spec.setting.title(),
                    answers.len()
                );
            }
        });
    }

    /// The security row is first on its page, defaults off, and is findable.
    ///
    /// §15.1 calls it "a security setting that was missing", it defaults off,
    /// and the same sentence requires that the reader be able to find its
    /// current state. Any one of the three is easy and all three together is
    /// the setting.
    #[gpui_kit::test]
    fn log_recording_defaults_off_and_is_findable(_cx: &mut TestAppContext) {
        assert_eq!(Setting::RecordLogContent.control(), Control::Switch);
        assert_eq!(default_value(Setting::RecordLogContent), Value::Bool(false));
        assert_eq!(
            Setting::RecordLogContent.storage_key(),
            Some("recordLogContent"),
            "the choice has to be stored somewhere a future reader of the file can find it"
        );
        let page = SPECS
            .iter()
            .filter(|spec| spec.category == SettingsCategory::DataPrivacy)
            .collect::<Vec<_>>();
        assert_eq!(
            page.first().map(|spec| spec.setting),
            Some(Setting::RecordLogContent),
            "the security row is the first thing on its page"
        );
        for query in [
            "log",
            "privacy",
            "record",
            "content",
            "security",
            "telemetry",
        ] {
            let spec = Setting::RecordLogContent.spec();
            assert!(
                spec.title.to_ascii_lowercase().contains(query)
                    || spec.description.to_ascii_lowercase().contains(query)
                    || spec.keywords.to_ascii_lowercase().contains(query),
                "“{query}” does not reach the log-recording row"
            );
        }
    }

    // -- states ------------------------------------------------------------

    /// A search that keeps nothing is one line, one action, and it says the list
    /// was *filtered* rather than empty.
    ///
    /// §4.13 asks an empty state to be terse and to distinguish "filtered out"
    /// from "genuinely empty". The state does both in a single line — "No
    /// matching settings" — and the query that emptied the list is still in the
    /// field 30px above it, so repeating it back was a second sentence of
    /// explanation for a state one click undoes. The action is the way back, and
    /// it has to work: the empty state used to claim the whole content column
    /// and draw this button on top of the danger zone.
    #[gpui_kit::test]
    fn an_empty_search_is_one_line_and_offers_one_way_out(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("zzz-nothing", cx)));
        cx.run_until_parked();
        let empty = cx
            .debug_bounds("settings-no-matches")
            .expect("a search with no hits reaches an empty state");
        assert!(f32::from(empty.size.height) > 0.0);
        assert_eq!(
            cx.update(|_, cx| view.read_with(cx, |view, cx| view.result_count(cx))),
            0
        );
        // One action, and it is clear of the danger zone: the two used to overlap.
        let action = cx
            .debug_bounds("settings-clear-search")
            .expect("the empty state offers one action");
        let zone = cx
            .debug_bounds("settings-danger-zone-box")
            .expect("the danger zone is on the window");
        assert!(
            f32::from(action.bottom()) <= f32::from(zone.top()) + 1.0,
            "the empty state's action overlaps the danger zone: action {action:?} zone {zone:?}"
        );
        cx.simulate_click(action.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-no-matches").is_none(),
            "clearing the search brings the pages back"
        );
    }

    /// A segmented control is one tab stop, and the arrow keys answer it.
    ///
    /// It used to publish two slots with the first segment on one and every
    /// other segment on the other, so `System / On / Off` could be answered from
    /// the keyboard only by pressing Enter on `System`: the two other answers
    /// were on screen and unreachable. This is the one assertion that would
    /// notice that coming back.
    #[gpui_kit::test]
    fn a_segmented_control_is_one_stop_the_arrows_can_answer(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Appearance, cx)
            })
        });
        cx.run_until_parked();
        let track = cx
            .debug_bounds("settings-segmented-reducemotion")
            .expect("Reduce motion is answered by a segmented control");
        assert!(f32::from(track.size.width) > 0.0);
        // One slot — the row's own — and Tab reaches it.
        focus_slot(&view, cx, Setting::ReduceMotion.tab());
        assert_eq!(
            cx.read(|cx| read_setting(Setting::ReduceMotion, cx, &user_settings(cx))),
            Value::Choice("System".to_owned())
        );
        cx.simulate_keystrokes("right");
        assert_eq!(
            cx.read(|cx| read_setting(Setting::ReduceMotion, cx, &user_settings(cx))),
            Value::Choice("On".to_owned()),
            "Right moves to the next answer"
        );
        cx.simulate_keystrokes("right right");
        assert_eq!(
            cx.read(|cx| read_setting(Setting::ReduceMotion, cx, &user_settings(cx))),
            Value::Choice("System".to_owned()),
            "Right wraps round to the first answer"
        );
        cx.simulate_keystrokes("left");
        assert_eq!(
            cx.read(|cx| read_setting(Setting::ReduceMotion, cx, &user_settings(cx))),
            Value::Choice("Off".to_owned()),
            "Left moves back, and wraps the other way"
        );
    }

    /// The two-column form holds from the width its own columns add up to.
    ///
    /// This is the layout cliff, and it is the one thing on this surface that
    /// looks like a different product either side of a number. The stacked form
    /// is a phone settings screen: every row twice the height, no shared control
    /// edge to scan down, and a page of eight that no longer fits the window. So
    /// the switch has to sit where the arithmetic puts it and not at the window's
    /// declared minimum — a window manager handing out a window nineteen pixels
    /// narrower than the one that was asked for is ordinary, and a form that
    /// changes shape over it is a form that can surprise a reader.
    #[gpui_kit::test]
    fn the_two_column_form_holds_from_its_own_floor_up(cx: &mut TestAppContext) {
        let floor = SETTINGS_LABEL_WIDTH
            + SETTINGS_CONTROL_WIDTH
            + f32::from(space::LG) * 2.
            + f32::from(space::XL);
        // 941 is the width a window manager gave this window while two windows
        // shared the screen; it is nineteen under the declared minimum, and it is
        // the width at which the form used to fall over.
        for width in [640., 941., 960., 1_280., 1_600.] {
            let (view, cx) = open(cx, width);
            cx.update(|_, cx| {
                view.update(cx, |view, cx| {
                    view.select_category(SettingsCategory::Appearance, cx)
                })
            });
            cx.run_until_parked();
            let row = bounds(cx, "settings-row-theme")
                .unwrap_or_else(|| panic!("the Theme row is laid out at {width}px"));
            let control = bounds(cx, "settings-control-theme")
                .unwrap_or_else(|| panic!("the Theme control is laid out at {width}px"));
            let side_by_side = f32::from(control.top()) < f32::from(row.bottom()) - 8.;
            assert_eq!(
                side_by_side,
                width >= floor,
                "at {width}px the Theme row is {}",
                if side_by_side {
                    "two columns"
                } else {
                    "stacked; the floor is {floor}px"
                }
            );
        }
    }

    /// Every row answered by a choice shows an answer that is one of its choices.
    ///
    /// This is the defect a drop-down hides best. A combo box whose value is not
    /// one of its own options looks perfectly fine — the trigger prints a
    /// plausible string — but the moment the reader opens the menu there is no
    /// tick anywhere in thirteen rows, and the only way to find out which theme
    /// is in force is to read the app's colours. The Theme row did exactly that:
    /// the store answers in modes and the menu offered names, so a file naming
    /// `K8s Studio Dark` showed `Dark` and ticked nothing.
    ///
    /// The value and the option list come from different places — one from the
    /// store, one from the row's own definition — so nothing else notices when
    /// they drift apart, and the drift is invisible until the menu opens.
    #[gpui_kit::test]
    fn every_answered_row_shows_an_answer_it_actually_offers(cx: &mut TestAppContext) {
        const INSTALLED: [&str; 4] = [
            "Ayu Dark",
            "K8s Studio Dark",
            "K8s Studio Light",
            "One Dark",
        ];
        cx.update(|cx| {
            init_app(cx);
            cx.set_global(SettingsLayout::default());
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        cx.update(|cx| {
            ThemeRegistry::global_mut(cx)
                .load_themes_from_str(&theme_set_json(&INSTALLED))
                .expect("the fixture theme set parses");
        });
        // Every way the store can answer a theme, not just the one a fresh test
        // app happens to boot into: a file that names a product theme alongside
        // its mode is the ordinary case, and it reads back as the mode.
        for choice in [
            ThemeChoice::System,
            ThemeChoice::Light,
            ThemeChoice::Dark,
            ThemeChoice::named("One Dark"),
        ] {
            cx.update(|cx| settings::set_theme_choice(cx, choice.clone()));
            let answers = cx.read(|cx| Setting::Theme.choices(cx));
            let shown =
                cx.read(|cx| read_setting(Setting::Theme, cx, &user_settings(cx)).as_text());
            assert!(
                answers.contains(&shown),
                "the Theme row shows {shown:?} for {choice:?}, which is not one of its options"
            );
        }
        cx.update(|cx| settings::set_theme_choice(cx, ThemeChoice::System));
        let (view, cx) = open(cx, 1_280.);
        for category in SettingsCategory::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            for spec in SPECS.iter().filter(|spec| spec.category == category) {
                if !matches!(spec.setting.control(), Control::Choices) {
                    continue;
                }
                let answers = cx.read(|cx| spec.setting.choices(cx));
                let shown =
                    cx.read(|cx| read_setting(spec.setting, cx, &user_settings(cx)).as_text());
                assert!(
                    answers.contains(&shown),
                    "the {} row shows {shown:?}, which is not one of its {:?}",
                    spec.setting.title(),
                    answers
                );
            }
        }
    }

    /// A search answers two words in whatever order they were typed.
    ///
    /// §15.2 makes the search box the reason a window with six pages and thirty
    /// rows is usable, so a query that returns nothing for a setting sitting on
    /// the screen is a broken path rather than a missing feature. The substring
    /// rule it replaced needed the words in the order the row printed them, so
    /// `dark theme` — the query a person types who is looking for the theme row
    /// and does not remember what it is called — found nothing.
    ///
    /// The words are the ones a reader would actually type, paired with the row
    /// they must reach, and paired the other way round as well, because an
    /// asymmetric fix that only handles the order that happens to be written
    /// first is not a fix.
    #[gpui_kit::test]
    fn a_search_answers_two_words_in_either_order(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for (query, expected) in [
            ("dark theme", Setting::Theme),
            ("theme dark", Setting::Theme),
            ("cache size", Setting::CacheLimit),
            ("size cache", Setting::CacheLimit),
            ("motion accessibility", Setting::ReduceMotion),
            ("accessibility motion", Setting::ReduceMotion),
            ("record privacy", Setting::RecordLogContent),
            ("privacy record", Setting::RecordLogContent),
        ] {
            cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query(query, cx)));
            cx.run_until_parked();
            let found = cx.update(|_, cx| view.read_with(cx, |view, cx| view.results(cx)));
            assert!(
                found.iter().any(|(setting, _)| *setting == expected),
                "“{query}” did not find {}: {found:?}",
                expected.title()
            );
        }
        // And a word that is nowhere on the row still finds nothing, so the
        // split has not turned the box into a match-anything filter.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("dark heliograph", cx)));
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| view.read_with(cx, |view, cx| view.results(cx)))
                .iter()
                .all(|(setting, _)| *setting != Setting::Theme),
            "a word no row prints must still exclude it"
        );
    }

    /// Walks Tab from the search field until the control holding `slot` has
    /// focus, and fails the test if the walk never arrives.
    ///
    /// The walk starts at the search field for the same reason
    /// [`rendered_tab_indices`] starts there: a walk that begins wherever focus
    /// happened to be left is a walk over part of the cycle.
    fn focus_slot(view: &Entity<SettingsView>, cx: &mut VisualTestContext, slot: isize) {
        let search = view.read_with(cx, |view, _| view.search_focus.clone());
        cx.update(|window, cx| window.focus(&search, cx));
        for _ in 0..64 {
            let next = cx.update(|window, app| {
                window.focus_next(app);
                window.focused(app).map(|handle| handle.tab_index)
            });
            if next == Some(slot) {
                return;
            }
        }
        panic!("Tab never reached slot {slot}");
    }

    /// A file that does not parse is reported, and the report names what the app
    /// fell back to.
    ///
    /// This is the worst outcome on this screen and it used to be silent:
    /// `settings::parse` answers a broken file with the built-in defaults, so a
    /// reader could change a setting, have it land in a file that is still
    /// broken, and see stock values with no sentence anywhere.
    #[gpui_kit::test]
    fn a_settings_file_that_does_not_parse_says_so_and_says_what_it_fell_back_to(
        cx: &mut TestAppContext,
    ) {
        assert!(
            settings_file_failure("{ this is not json").is_some(),
            "a broken file is detected"
        );
        assert_eq!(settings_file_failure("{}"), None);
        assert_eq!(settings_file_failure(r#"{"theme": null}"#), None);
        // The sentence names the file, the fallback, and what to do about it.
        assert!(SETTINGS_FILE_UNREADABLE.contains("could not be read"));
        assert!(SETTINGS_FILE_UNREADABLE.contains("defaults"));
        assert_eq!(SETTINGS_FILE_OPEN_LABEL, "Open settings file");
        // And a store that parses shows no banner.
        let (_view, cx) = open(cx, 960.);
        assert!(cx.debug_bounds("settings-file-unreadable").is_none());
    }

    /// A failed write is reported next to the rows it lost, with a verb.
    ///
    /// `feedback.md` asks for an error as close to the problem as it can be, and
    /// a `w_full` banner with a spacer in the middle put `Retry` 1888px from the
    /// promise on a wide window.
    #[gpui_kit::test]
    fn a_failed_write_marks_its_rows_and_keeps_the_retry_on_one_measure(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 1_600.);
        assert!(
            cx.debug_bounds("settings-row-note-recordlogcontent")
                .is_none()
        );
        cx.update(|_, cx| {
            cx.set_global(settings::SettingsSaveStatus {
                pending: false,
                error: Some("Permission denied".to_owned()),
                epoch: 1,
                rollback: None,
                rejected: None,
            })
        });
        cx.run_until_parked();
        let banner = cx
            .debug_bounds("settings-save-error")
            .expect("the failed write is reported");
        let retry = cx
            .debug_bounds("settings-save-retry")
            .expect("the recovery control is on the banner");
        assert!(
            f32::from(retry.right()) - f32::from(banner.left())
                <= SETTINGS_CONTENT_MAX_WIDTH + f32::from(space::LG) * 2. + 1.0,
            "the retry is past the {}px measure",
            SETTINGS_CONTENT_MAX_WIDTH
        );
        // The rows whose answers live in the file say so, in the label column.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::DataPrivacy, cx)
            })
        });
        cx.run_until_parked();
        let content = cx
            .debug_bounds("settings-content")
            .expect("the content column");
        let label_column = f32::from(content.left()) + f32::from(space::LG);
        let note = cx
            .debug_bounds("settings-row-note-recordlogcontent")
            .expect("the security row says the change did not land");
        assert!(
            f32::from(note.left()) >= label_column - 1.0
                && f32::from(note.right()) <= label_column + SETTINGS_LABEL_WIDTH + 1.0,
            "the note is outside the label column: {note:?}"
        );
        // A row whose answer is not in the file does not claim one.
        assert!(cx.debug_bounds("settings-row-note-version").is_none());
    }

    // -- appearance, danger, no Apply --------------------------------------

    /// An appearance change takes effect, and there is no Apply button.
    ///
    /// §15.2's fourth rule. The rule is only credible if something on the page
    /// would have been an Apply button, so the test looks for one by name as
    /// well as proving the write lands without a second click.
    #[gpui_kit::test]
    fn an_appearance_change_takes_effect_with_no_apply_button(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for forbidden in [
            "settings-apply",
            "settings-apply-changes",
            "settings-save-button",
        ] {
            assert!(
                cx.debug_bounds(forbidden).is_none(),
                "{forbidden} is an Apply button, and §15.2 says there is none"
            );
        }
        // A change writes on the first press: the store moves with no second
        // click anywhere.
        view.update(cx, |view, cx| {
            view.commit(Setting::Contrast, Value::Bool(true), None, cx)
        });
        cx.run_until_parked();
        assert!(cx.read(crate::settings::increase_contrast_enabled));
    }

    /// The two destructive actions are at the bottom of the page, isolated in a
    /// region of their own, and they confirm before they do anything.
    ///
    /// §15.2 puts the danger zone at the bottom of the page and says only these
    /// two confirm. The zone is the last thing in the document rather than a
    /// band pinned to the window floor — `render_danger_zone` has the
    /// measurement that traded for that — and it is isolated by a hairline, a
    /// group head and `danger_word` on both buttons rather than by a red frame
    /// around the region.
    #[gpui_kit::test]
    fn the_danger_zone_is_pinned_isolated_and_confirms(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        let zone = cx
            .debug_bounds("settings-danger-zone")
            .expect("the danger zone is on the window");
        let row =
            bounds(cx, Setting::Contrast.selector()).expect("an appearance row is on the window");
        assert!(
            f32::from(zone.top()) > f32::from(row.top()),
            "the danger zone is below the settings it endangers"
        );
        let clear = cx
            .debug_bounds("settings-danger-clear-cache")
            .expect("Clear cache is in the zone");
        let reset = cx
            .debug_bounds("settings-danger-reset-all")
            .expect("Reset all settings is in the zone");
        assert!(f32::from(clear.top()) >= f32::from(zone.top()));
        assert!(f32::from(reset.top()) >= f32::from(zone.top()));
        // It is never mixed into a normal row: it is a region of its own, at the
        // very bottom of the page, and its boundary is a hairline rather than a
        // stroke around a decorative box.
        assert!(
            cx.debug_bounds("settings-row-clearcache").is_none(),
            "a destructive action is never a settings row"
        );
        // The first press arms; nothing is deleted until the second.
        cx.simulate_click(clear.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.danger_step),
            DangerStep::ClearCache
        );
        assert!(cx.debug_bounds("settings-danger-cancel").is_some());
        let cancel = cx
            .debug_bounds("settings-danger-cancel")
            .expect("the armed step offers Cancel");
        cx.simulate_click(cancel.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.danger_step),
            DangerStep::Idle
        );
    }

    /// Every page and its danger zone fit a 900px window.
    ///
    /// "At most eight rows" is what makes a page fit, and it only works while a
    /// row is two lines. The moment a paragraph comes back into a row, the
    /// Appearance page measured 737px of rows and pushed the danger zone off the
    /// bottom of a 900px window — which is the one control a reader must be able
    /// to see before pressing it. The geometry is asserted here because nothing
    /// else notices: the page still renders, still looks like a settings page,
    /// and the zone is one scroll away.
    #[gpui_kit::test]
    fn every_page_and_its_danger_zone_fit_a_900px_window(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for category in SettingsCategory::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            let zone = bounds(cx, "settings-danger-zone")
                .unwrap_or_else(|| panic!("the {} page has a danger zone", category.label()));
            assert!(
                f32::from(zone.bottom()) <= 900.,
                "the {} page's danger zone ends at {}px, below a 900px window",
                category.label(),
                f32::from(zone.bottom())
            );
        }
    }

    /// Escape unwinds one layer at a time.
    ///
    /// "Esc always has an effect" is a native-feel rule, and an armed
    /// destructive step that ignores it is the worst case on this screen.
    #[gpui_kit::test]
    fn escape_leaves_an_armed_danger_step(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.on_danger(DangerStep::ResetAll, window, cx)
            })
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.danger_step),
            DangerStep::ResetAll
        );
        let escape = KeyDownEvent {
            keystroke: Keystroke::parse("escape").expect("escape parses"),
            is_held: false,
            prefer_character_input: false,
        };
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.on_settings_key_down(&escape, window, cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.danger_step),
            DangerStep::Idle
        );
    }

    // -- the shortcut reference --------------------------------------------

    /// The reference is a destination with its own search, and it is reachable.
    ///
    /// `UI-REDESIGN` L13 item 3 says `?` opens a shortcut reference and that it
    /// is worth more than the settings list it replaces. What is testable here is
    /// the destination: it opens, it has a search of its own, and the action that
    /// `default-linux.json` binds to it is the public toggle, so the key and the
    /// surface cannot drift apart.
    #[gpui_kit::test]
    fn the_shortcut_reference_is_a_searchable_destination(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Keyboard, cx)
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-row-shortcutreference").is_some());
        let open = bounds(cx, Setting::ShortcutReference.control_selector())
            .expect("the Keyboard page offers the reference");
        cx.simulate_click(open.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.shortcut_reference_open()));
        assert_eq!(
            view.read_with(cx, |view, _| view.pane_title()),
            "Settings · Shortcuts"
        );
        assert!(cx.debug_bounds("settings-keycap").is_some());
        // There is one search box, not two: the same field that filtered the
        // settings filters the commands, so a reader never has to work out which
        // of two identical fields applies to the rows in front of them.
        assert!(cx.debug_bounds("settings-search").is_some());
        // A query the reference cannot answer reaches its own empty state.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("zzz-nothing", cx)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-no-shortcuts").is_some());
        // And the public toggle closes it again.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.toggle_shortcut_reference(window, cx))
        });
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, _| view.shortcut_reference_open()));
    }

    // -- layout ------------------------------------------------------------

    /// The tab order is a set of blocks that cannot collide.
    ///
    /// GPUI walks a frame's stops in slot order, so a control's slot is where Tab
    /// reaches it. The constants cannot check themselves, so every page is
    /// walked with `focus_next` and the slots are compared as they were drawn: a
    /// row that borrowed a slot from another block, or a block that started too
    /// low, shows up here as a duplicate or a missing stop.
    #[gpui_kit::test]
    fn tab_order_blocks_do_not_collide(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 960.);

        for category in SettingsCategory::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            let walked = rendered_tab_indices(&view, cx);
            // The strip's own block: the six page tabs, in order.
            let tabs = walked
                .iter()
                .copied()
                .filter(|index| {
                    (tab_order::TABS_FIRST
                        ..tab_order::TABS_FIRST + SettingsCategory::ALL.len() as isize)
                        .contains(index)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                tabs,
                (0..SettingsCategory::ALL.len() as isize)
                    .map(|offset| tab_order::TABS_FIRST + offset)
                    .collect::<Vec<_>>(),
                "the strip owns six consecutive slots on the {} page; walked {walked:?}",
                category.label()
            );
            // This page's rows own the slots derived from their own position,
            // and no two of them share one.
            let first =
                tab_order::ROWS_FIRST + category.index() as isize * tab_order::ROWS_PER_CATEGORY;
            let last = first + 8;
            let rows = walked
                .iter()
                .copied()
                .filter(|index| (first..last).contains(index))
                .collect::<Vec<_>>();
            let mut unique = rows.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                unique.len(),
                rows.len(),
                "two controls on the {} page share a slot: {rows:?}",
                category.label()
            );
            assert!(
                rows.windows(2).all(|pair| pair[1] > pair[0]),
                "the {} page's rows are not in reading order: {rows:?}",
                category.label()
            );
            // A row whose answer nothing reads yet is disabled, and a disabled
            // control is not a tab stop — so the slots registered are the live
            // rows' and no others.
            let live = SPECS
                .iter()
                .filter(|spec| spec.category == category)
                .map(|spec| spec.setting)
                .filter(|setting| spec_of(*setting).pending.is_none() || !setting.sets_an_answer())
                .map(Setting::tab)
                .collect::<Vec<_>>();
            assert_eq!(
                rows.clone(),
                rows.iter()
                    .copied()
                    .filter(|slot| live.contains(slot))
                    .collect::<Vec<_>>(),
                "the {} page registers a slot for a row that is not live: {rows:?}",
                category.label()
            );
        }
        // The Appearance page has live rows, so the derivation is exercised on a
        // page that actually registers them rather than only on a page of
        // disabled controls.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Appearance, cx)
            })
        });
        cx.run_until_parked();
        let walked = rendered_tab_indices(&view, cx);
        for spec in SPECS
            .iter()
            .filter(|spec| spec.category == SettingsCategory::Appearance)
        {
            if spec.pending.is_some() && spec.setting.sets_an_answer() {
                continue;
            }
            assert!(
                walked.contains(&spec.setting.tab()),
                "{} owns slot {} and Tab does not reach it: {walked:?}",
                spec.setting.title(),
                spec.setting.tab()
            );
        }
        // The danger zone's controls sit above every row block, so Tab reaches
        // them after the last row rather than in the middle of one, and the slot
        // between them belongs only to the Cancel that an armed step adds.
        let walked = rendered_tab_indices(&view, cx);
        assert!(walked.contains(&tab_order::DANGER_FIRST));
        let reset_slot = tab_order::DANGER_FIRST + 2;
        assert!(walked.contains(&reset_slot));
        assert!(
            !walked.contains(&(tab_order::DANGER_FIRST + 1)),
            "nothing owns the middle slot while no step is armed: {walked:?}"
        );
        let rows_last = tab_order::ROWS_FIRST
            + SettingsCategory::Updates.index() as isize * tab_order::ROWS_PER_CATEGORY
            + 8;
        let danger_first = walked
            .iter()
            .position(|index| *index == tab_order::DANGER_FIRST)
            .expect("the zone is walked");
        let after = walked[danger_first..]
            .iter()
            .all(|index| *index >= tab_order::DANGER_FIRST || *index < rows_last);
        assert!(
            after,
            "the zone's slots sit above the row blocks: {walked:?}"
        );
    }

    /// The tab slots the surface registered, in the order Tab walks them.
    ///
    /// `focus_next` visits the stops the last frame's paint registered, ordered
    /// by the index each control carries, so every number a test looks at is read
    /// back off a real control rather than compared with another constant.
    ///
    /// The walk starts on the search field, because a walk that begins wherever
    /// focus happened to be left off the part of the cycle it had already
    /// travelled and the numbers before that point would be missing rather than
    /// wrong. It ends when a slot comes round a second time, which is one full
    /// cycle: stopping on the first repeat instead cuts the walk short every time
    /// the first stop is restamped more than once on the opening frames.
    fn rendered_tab_indices(view: &Entity<SettingsView>, cx: &mut VisualTestContext) -> Vec<isize> {
        const WALK_LIMIT: usize = 256;
        let search = view.read_with(cx, |view, _| view.search_focus.clone());
        cx.update(|window, cx| window.focus(&search, cx));
        let mut seen: Vec<isize> = Vec::new();
        let mut previous: Option<isize> = None;
        for _ in 0..WALK_LIMIT {
            let next = cx.update(|window, app| {
                window.focus_next(app);
                window.focused(app).map(|handle| handle.tab_index)
            });
            let Some(index) = next else {
                break;
            };
            // The search field's handle is tracked by two elements, so the map
            // holds a node per element and Tab rests on it once per node before
            // it moves. A repeat of the slot the walk is already on is that, and
            // a repeat of a slot it has already passed is the cycle coming round.
            if previous == Some(index) {
                continue;
            }
            if seen.contains(&index) {
                break;
            }
            seen.push(index);
            previous = Some(index);
        }
        seen
    }

    /// A settings row is read as a pair, so every control shares one column on
    /// the trailing edge.
    ///
    /// The value column used to start at a fixed offset from the leading edge,
    /// which in a 1600px window put a theme dropdown a third of the way across
    /// the page and left the rest of the measure empty.
    #[gpui_kit::test]
    fn the_settings_rows_share_one_control_column(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            cx.set_global(SettingsLayout::default());
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 1_600.);
        let content = cx
            .debug_bounds("settings-content")
            .expect("the content column is laid out");
        assert!(
            f32::from(content.size.width) <= SETTINGS_CONTENT_MAX_WIDTH + 1.0,
            "the content column is {}px wide, past the {}px measure",
            f32::from(content.size.width),
            SETTINGS_CONTENT_MAX_WIDTH
        );
        let trailing_edge = f32::from(content.right()) - f32::from(space::LG);
        let control_column = trailing_edge - SETTINGS_CONTROL_WIDTH;
        assert!(
            control_column > f32::from(content.left()) + SETTINGS_LABEL_WIDTH,
            "the control column at {control_column}px has no room left of it for a label"
        );
        let mut checked = 0;
        for category in SettingsCategory::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            for spec in SPECS.iter().filter(|spec| spec.category == category) {
                // Every control is measured through its own wrapper, which is
                // the box that ends on the measure's trailing edge.
                let selector = spec.setting.control_selector();
                let laid_out = bounds(cx, selector.clone())
                    .unwrap_or_else(|| panic!("{selector} is laid out"));
                assert!(
                    (f32::from(laid_out.right()) - trailing_edge).abs() <= 1.0,
                    "{selector} ends at {}px, not on the {trailing_edge}px trailing edge",
                    f32::from(laid_out.right())
                );
                checked += 1;
            }
        }
        assert_eq!(checked, SPECS.len(), "every row's control was measured");
    }

    /// Every category has a navigation stop on the window, and the window has
    /// no navigation of its own beyond that list.
    ///
    /// The search field owns a focus ring, and an accent slab beside it made the
    /// selection the loudest thing in the window while the ring around the field
    /// that filters it stayed quiet. An accent is the scarcest thing on a
    /// screen: six categories, one of them marked. Which of the two navigation
    /// layouts the reader gets is the window's own width, and at this window's
    /// minimum it is the rail — so the assertion is on the name a test can find
    /// in either of them, not on the one shape that happens to be drawn here.
    #[gpui_kit::test]
    fn the_open_page_is_an_underline_and_a_selection(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        for category in SettingsCategory::ALL {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            let selector = format!("settings-category-{}", category.label());
            let tab = bounds(cx, selector.clone())
                .unwrap_or_else(|| panic!("the {selector} tab is on the window"));
            assert!(f32::from(tab.size.height) > 0.0);
        }
        // The categories are a list and nothing else: a rail that could be
        // collapsed to icons is a second, hidden navigation, and a window the
        // reader has to guess in is the failure this surface is fixing.
        assert!(cx.debug_bounds("settings-sidebar-toggle").is_none());
    }

    /// The window's first tab stop is the search field, and a fresh window
    /// lands on the page the reader left.
    #[gpui_kit::test]
    fn default_focus_targets_search_input(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = open(cx, 960.);
        let search = view.read_with(cx, |view, _| view.search_focus.clone());
        assert!(search.tab_stop);
        assert_eq!(search, view.read_with(cx, |view, _| view.focus_handle()));
        cx.update(|window, cx| window.focus(&search, cx));
        assert!(cx.update(|window, _| search.is_focused(window)));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Editor, cx)
            })
        });
        let reopened = cx.update(|_, cx| cx.new(SettingsView::new));
        cx.run_until_parked();
        assert_eq!(
            reopened.read_with(cx, |view, _| view.category),
            SettingsCategory::Editor
        );
    }

    // -- the theme menu ----------------------------------------------------

    #[gpui_kit::test]
    fn theme_menu_lists_system_k8s_studio_and_zed_themes(cx: &mut TestAppContext) {
        const THEME_NAMES: [&str; 13] = [
            "Ayu Dark",
            "Ayu Light",
            "Ayu Mirage",
            "Gruvbox Dark",
            "Gruvbox Dark Hard",
            "Gruvbox Dark Soft",
            "Gruvbox Light",
            "Gruvbox Light Hard",
            "Gruvbox Light Soft",
            "K8s Studio Dark",
            "K8s Studio Light",
            "One Dark",
            "One Light",
        ];
        cx.update(init_app);
        let (view, cx) = open(cx, 960.);
        cx.update(|_, cx| {
            ThemeRegistry::global_mut(cx)
                .load_themes_from_str(&theme_set_json(&THEME_NAMES))
                .expect("the fixture theme set parses");
        });
        let expected = THEME_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        cx.update(|_, cx| {
            let installed = theme_names(cx);
            for name in &expected {
                assert!(
                    installed.contains(name),
                    "{name} is installed but the menu cannot offer it"
                );
            }
            let mut sorted = installed.clone();
            sorted.sort();
            assert_eq!(installed, sorted, "the menu's order has to be stable");
        });
        // Fourteen answers is more than a segmented control can hold, so the
        // theme is a drop-down and every installed name is on it.
        let choices = cx.read(|cx| Setting::Theme.choices(cx));
        assert!(
            choices.len() > 4,
            "fourteen answers cannot be a segmented control"
        );
        for name in &expected {
            assert!(choices.contains(name), "{name} is not on the drop-down");
        }
        assert_eq!(choices[0], ThemeChoice::System.label());
        let trigger = bounds(cx, Setting::Theme.control_selector())
            .expect("the Theme row offers a pop-up trigger");
        cx.simulate_click(trigger.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.open_menu),
            Some(OpenMenu(Setting::Theme))
        );
        cx.simulate_click(trigger.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.open_menu), None);

        // The menu groups the product themes before the imported ones, so a
        // person does not read thirteen names to reach K8s Studio.
        let groups = theme_groups(expected);
        assert_eq!(
            groups.iter().map(|(title, _)| *title).collect::<Vec<_>>(),
            vec!["K8s Studio", "Zed"]
        );
        assert_eq!(groups[0].1, vec!["K8s Studio Dark", "K8s Studio Light"]);
        assert_eq!(groups[1].1.len(), 11);
    }

    /// A theme set the test can install, built from the names it asserts on.
    fn theme_set_json<const N: usize>(names: &[&str; N]) -> String {
        let themes = names
            .iter()
            .map(|name| format!(r#"{{"name": "{name}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"themes": [{themes}]}}"#)
    }

    // -- the settings themselves -------------------------------------------

    /// The data font size writes a size the surfaces measure with, and clamps a
    /// request outside the offered list.
    #[gpui_kit::test]
    fn the_text_size_control_persists_a_choice_and_clamps_beyond_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 960.);
        assert_eq!(
            data_font_label(crate::settings::PRODUCT_DATA_FONT_SIZE),
            "12 px (default)"
        );
        view.update(cx, |view, cx| {
            view.commit(
                Setting::TextSize,
                Value::Choice("16 px".to_owned()),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |_, cx| data_font_size(cx)),
            16.,
            "the chosen size reaches the value the data surfaces measure with"
        );
        // A size the menu cannot show as selected is not written.
        view.update(cx, |view, cx| {
            view.commit(
                Setting::TextSize,
                Value::Choice("400 px".to_owned()),
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.error.is_some()));
        assert_eq!(
            view.read_with(cx, |_, cx| data_font_size(cx)),
            16.,
            "a rejected size leaves the stored one alone"
        );
        // And every offered size round-trips.
        for size in DATA_FONT_SIZES {
            view.update(cx, |view, cx| {
                view.commit(
                    Setting::TextSize,
                    Value::Choice(data_font_label(*size)),
                    None,
                    cx,
                )
            });
        }
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |_, cx| data_font_size(cx)), 18.);
    }

    /// The store's default settings carry `reduce_motion: "off"`, so a merged
    /// read can never say "nobody chose" and would make the inherited state
    /// unreachable.
    #[gpui_kit::test]
    fn reduce_motion_can_hand_the_choice_back_to_the_app(cx: &mut TestAppContext) {
        cx.update(init_app);
        let stored = |text: &'static str, cx: &mut TestAppContext| {
            cx.update(|cx| {
                SettingsStore::update(cx, |store, cx| {
                    store
                        .set_user_settings(text, cx)
                        .expect("valid user settings");
                });
            });
            cx.read(reduce_motion_choice)
        };
        assert_eq!(stored("{}", cx), ReduceMotionChoice::System);
        assert_eq!(
            stored(r#"{"reduce_motion":"on"}"#, cx),
            ReduceMotionChoice::On
        );
        assert_eq!(
            stored(r#"{"reduce_motion":"off"}"#, cx),
            ReduceMotionChoice::Off
        );
        // `System` is the state that writes nothing, so it is the only one that
        // can be undone.
        assert_eq!(ReduceMotionChoice::System.mode(), None);
        assert_eq!(ReduceMotionChoice::On.mode(), Some(ReduceMotionMode::On));
        assert_eq!(ReduceMotionChoice::Off.mode(), Some(ReduceMotionMode::Off));
        assert_eq!(ReduceMotionChoice::ALL.len(), 3);
        for choice in ReduceMotionChoice::ALL {
            assert!(!choice.label().is_empty());
        }
    }

    /// A one-click, immediately reversible setting is not a completed activity.
    ///
    /// A settings page that raises a notification for every successful switch
    /// spends the health channel on a control that already shows the answer.
    #[gpui_kit::test]
    fn a_settings_switch_does_not_confirm_its_own_success(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            settings::set_test_settings_memory(&crate::settings::UserSettings::default());
        });
        let (view, cx) = open(cx, 960.);
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = notices.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, _, _| sink.borrow_mut().push(message));
        });
        view.update(cx, |view, cx| {
            view.commit(Setting::Contrast, Value::Bool(true), None, cx);
            view.commit(Setting::Cache, Value::Bool(false), None, cx);
            view.commit(Setting::RecordLogContent, Value::Bool(true), None, cx);
        });
        cx.run_until_parked();
        assert!(
            notices.borrow().is_empty(),
            "a switch that reports itself restated the control that already showed it: {:?}",
            notices.borrow()
        );
        assert!(view.read_with(cx, |view, _| view.error.is_none()));
    }

    /// A stored switch reaches the file, so a preference is a preference and not
    /// a label.
    #[gpui_kit::test]
    fn a_stored_switch_lands_in_the_file(cx: &mut TestAppContext) {
        cx.update(init_app);
        let (view, cx) = open(cx, 960.);
        view.update(cx, |view, cx| {
            view.commit(Setting::RecordLogContent, Value::Bool(true), None, cx)
        });
        cx.run_until_parked();
        let raw = cx.read(raw_user_settings).unwrap_or_default();
        assert!(
            raw.contains("recordLogContent"),
            "the security choice is not in the file: {raw}"
        );
        // And a reader who comes back sees it.
        assert_eq!(
            cx.read(|cx| read_setting(Setting::RecordLogContent, cx, &user_settings(cx))),
            Value::Bool(true)
        );
        view.update(cx, |view, cx| {
            view.commit(Setting::RecordLogContent, Value::Bool(false), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            cx.read(|cx| read_setting(Setting::RecordLogContent, cx, &user_settings(cx))),
            Value::Bool(false)
        );
    }

    // -- the keymap --------------------------------------------------------

    /// Every row says what its command does, so the list is read as a person
    /// reads it.
    ///
    /// The table behind [`keyboard_description`] used to end in
    /// `_ => format!("Run {label}.")`, and 33 of 68 rows reached it, so
    /// `Scale Selection` described a command the contract makes fail-closed as
    /// something it does.
    #[gpui_kit::test]
    fn every_listed_action_says_what_it_does(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let sections = keyboard_sections(cx, true);
            let commands = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .collect::<Vec<_>>();
            assert!(
                commands.len() > 40,
                "the list under test is the whole command set, not a fixture: {}",
                commands.len()
            );
            for command in &commands {
                assert_ne!(
                    command.description,
                    format!("Run {}.", command.label),
                    "{} has no sentence of its own and falls through to the generic one",
                    command.action_name
                );
                assert!(
                    !command.description.trim().is_empty(),
                    "{} has no description",
                    command.action_name
                );
                assert!(
                    command.description.ends_with('.'),
                    "{} does not read as a sentence: {}",
                    command.action_name,
                    command.description
                );
                assert!(
                    !command.description.contains("Run "),
                    "{} describes an action with the generic verb: {}",
                    command.action_name,
                    command.description
                );
            }
            // UI Zoom is fail-closed by contract, so the one row that documents
            // it has to say that.
            let scale = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::ScaleSelection")
                .expect("Scale Selection is listed");
            assert_eq!(
                scale.description, "UI zoom is not available in this build.",
                "the one fail-closed command is documented as an unavailable one"
            );
            // The three theme actions live in the `k8s_shell` namespace, so a
            // table keyed on `k8s_app::Use…` arms would match nothing.
            for action in [
                "k8s_shell::UseLightTheme",
                "k8s_shell::UseDarkTheme",
                "k8s_shell::UseSystemTheme",
            ] {
                let command = commands
                    .iter()
                    .find(|command| command.action_name == action)
                    .unwrap_or_else(|| panic!("{action} is listed"));
                assert_ne!(command.description, format!("Run {}.", command.label));
            }
            assert!(names(&commands).contains(&"k8s_app::OpenSettings"));
            assert!(!names(&commands).contains(&"k8s_ops::EditYaml"));
        });
    }

    fn names<'a>(commands: &'a [&'a KeyboardCommand]) -> Vec<&'a str> {
        commands
            .iter()
            .map(|command| command.action_name.as_str())
            .collect()
    }

    /// The action names a chord answers with in a stack of contexts.
    ///
    /// The same read the keymap's own tests use, spelled out here because this is
    /// the one place the reference's own key is checked: `Unbind` shows up in the
    /// list exactly like a command, and a released key is a list holding nothing but
    /// `Unbind`.
    fn answers(cx: &mut TestAppContext, chord: &str, contexts: &[&str]) -> Vec<String> {
        let chord = gpui_kit::Keystroke::parse(chord).expect("the chord parses");
        let contexts = contexts
            .iter()
            .map(|context| gpui_kit::KeyContext::parse(context).expect("the context parses"))
            .collect::<Vec<_>>();
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_input(&[chord], &contexts)
                .0
                .iter()
                .map(|binding| binding.action().name().to_owned())
                .collect()
        })
    }

    /// `?` reaches the reference from a surface, and types on a text surface.
    ///
    /// `UI-REDESIGN` L13 binds `?` to the reference, so the binding is global rather
    /// than scoped to a context, and a global binding on a printable character is
    /// the one kind of keybinding that can take a character away from the person
    /// typing it. Both halves fail silently: a key released nowhere is a `?` that
    /// opens the reference instead of landing in the search box, and a key bound in
    /// no context is a `?` nobody can press. Nothing about either looks broken in a
    /// frame.
    #[gpui_kit::test]
    fn the_reference_key_is_global_and_released_where_text_is_typed(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        // Both spellings, because XKB reports Shift+slash as the `?` keysym with the
        // shift bit set and other toolkits report the `/` key with it. A binding for
        // one of them is a binding that works on one of two platforms.
        for chord in ["?", "shift-?"] {
            assert!(
                answers(cx, chord, &["Shell"])
                    .contains(&"k8s_shell::OpenShortcutReference".to_owned()),
                "{chord} does not open the reference from the shell"
            );
            assert!(
                answers(cx, chord, &["Settings"])
                    .contains(&"k8s_shell::OpenShortcutReference".to_owned()),
                "{chord} does not open the reference from the settings surface"
            );
            for typing in [
                &["Shell", "TextInput"][..],
                &["Editor"][..],
                &["ResourceSearch"][..],
                &["Dialog"][..],
                &["PopupMenu"][..],
                &["CommandPalette", "TextInput"][..],
                &["Shell", "Terminal"][..],
            ] {
                assert!(
                    !answers(cx, chord, typing)
                        .contains(&"k8s_shell::OpenShortcutReference".to_owned()),
                    "{chord} opens the reference from {typing:?}, where the character is owed to the reader"
                );
            }
        }
    }

    /// Every action the built-in keymap binds is reachable from the reference.
    ///
    /// The list is derived from the command palette, so two things can go wrong
    /// for one action: the row can exist with a context the binding does not
    /// use, which leaves the keycap blank, and the row can be missing
    /// altogether, which hides the shortcut.
    #[gpui_kit::test]
    fn every_action_the_built_in_keymap_binds_shows_its_chord(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let mut bound = cx
                .key_bindings()
                .borrow()
                .bindings()
                .map(|binding| binding.action().name().to_owned())
                .filter(|name| name.starts_with("k8s_"))
                .collect::<Vec<_>>();
            bound.sort();
            bound.dedup();
            assert!(bound.len() > 20, "the default keymap has to be installed");
            let sections = keyboard_sections(cx, true);
            let commands = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .collect::<Vec<_>>();
            let listed = names(&commands);
            let accounted_for = |name: &str| {
                listed.contains(&name)
                    || BOUND_ON_KEYS_THE_RECORDER_REFUSES
                        .iter()
                        .any(|(action, _)| *action == name)
                    || BOUND_WITHOUT_A_ROW
                        .iter()
                        .any(|(action, _)| *action == name)
                    || APPLE_MENU_CHORDS.contains(&name)
            };
            for name in &bound {
                let command = KeyboardCommand {
                    action_name: name.clone(),
                    label: name.clone(),
                    description: String::new(),
                    action_input: None,
                    context: canonical_context(name).map(str::to_owned),
                    when: None,
                    editable: true,
                };
                assert!(command_chord(&command, cx).is_some(), "{name} is bound");
                assert!(
                    accounted_for(name),
                    "{name} is bound but the reference has no row for it"
                );
            }
            // A name the keymap stopped binding would leave the tables above
            // claiming a gap that is not there.
            for (name, reason) in BOUND_ON_KEYS_THE_RECORDER_REFUSES
                .iter()
                .chain(BOUND_WITHOUT_A_ROW)
            {
                assert!(bound.contains(&name.to_string()), "{name}: {reason}");
            }
            for name in APPLE_MENU_CHORDS {
                assert!(bound.contains(&name.to_string()), "{name} is bound");
            }
        });
    }

    /// The reference and the keymap agree, in every preset, in both directions.
    ///
    /// This is the one place being wrong is worst, because a reader trusts it
    /// absolutely: a keycap that does not fire is a small lie, and a bound key
    /// with no row is the worse one, because the person cannot find the feature
    /// it does. Both directions failed in a way nothing on screen showed — the
    /// reference kept a block named `Inspector` after the palette dropped it,
    /// and `OpenDetails`, `DeleteSelection`, `OpenRowActions`, `k8s_ops::Refresh`
    /// and the three tab-strip commands had been bound all along with no row
    /// anywhere.
    ///
    /// Every preset is checked because a preset is an overlay: the rows are built
    /// from the live keymap, so an overlay that moves a chord to a context the row
    /// does not name blanks its keycap without any other test noticing.
    #[gpui_kit::test]
    fn the_reference_agrees_with_every_preset(cx: &mut TestAppContext) {
        for preset in [keymap::KeymapPreset::Lens, keymap::KeymapPreset::Vscode] {
            cx.update(|cx| {
                crate::keymap::install_sources_with_preset(
                    cx,
                    crate::keymap::default_keymap_source(),
                    None,
                    preset,
                )
                .expect("the keymap loads");
                let mut bound = cx
                    .key_bindings()
                    .borrow()
                    .bindings()
                    .map(|binding| binding.action().name().to_owned())
                    .filter(|name| name.starts_with("k8s_"))
                    .collect::<Vec<_>>();
                bound.sort();
                bound.dedup();
                let sections = keyboard_sections(cx, true);
                let commands = sections
                    .iter()
                    .flat_map(|section| section.commands.iter())
                    .collect::<Vec<_>>();
                let listed = names(&commands);
                assert!(
                    listed.len() > 40,
                    "{}: the list under test is the whole command set, not a fixture: {}",
                    preset.label(),
                    listed.len()
                );
                for name in &bound {
                    let accounted_for = listed.contains(&name.as_str())
                        || BOUND_ON_KEYS_THE_RECORDER_REFUSES
                            .iter()
                            .any(|(action, _)| action == name)
                        || BOUND_WITHOUT_A_ROW.iter().any(|(action, _)| action == name)
                        || APPLE_MENU_CHORDS.contains(&name.as_str());
                    assert!(
                        accounted_for,
                        "{}: {name} is bound but the reference has no row for it",
                        preset.label()
                    );
                }
                for command in &commands {
                    // A row that says where its shortcut is live has to have one
                    // there. A global row may show nothing, because that is how a
                    // command with no key yet is offered for one.
                    if command.when.is_some() {
                        assert!(
                            command_chord(command, cx).is_some(),
                            "{}: {} names a context and shows no key",
                            preset.label(),
                            command.action_name
                        );
                    }
                }
            });
        }
    }

    /// The reference's blocks are the palette's blocks, in the palette's order.
    ///
    /// The palette is the app's only labelled surface, so where a command sits
    /// there is most of how a person learns the command exists; a second list
    /// organised any other way is a list that has to be learned twice. The
    /// `Inspector` block was organised by the module that owned the command, and
    /// it held exactly one row.
    #[gpui_kit::test]
    fn the_reference_blocks_are_the_palette_blocks_in_the_palette_order(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let sections = keyboard_sections(cx, true);
            let titles = sections
                .iter()
                .map(|section| section.title.clone())
                .collect::<Vec<_>>();
            let palette = demo_commands_with_updater(true, true)
                .iter()
                .map(|command| command.group.to_string())
                .fold(Vec::<String>::new(), |mut order, group| {
                    if !order.contains(&group) {
                        order.push(group);
                    }
                    order
                });
            assert!(
                palette.len() >= 8,
                "the palette is the arrangement the reference shares: {palette:?}"
            );
            let mut cursor = 0usize;
            for title in &titles {
                let found = palette[cursor..]
                    .iter()
                    .position(|group| group == title)
                    .unwrap_or_else(|| panic!("{title} is not a palette block"));
                cursor += found + 1;
            }
            for action in [
                "k8s_ops::DeleteSelection",
                "k8s_table::OpenDetails",
                "k8s_table::OpenRowActions",
                "k8s_ops::Refresh",
            ] {
                let command = sections
                    .iter()
                    .flat_map(|section| section.commands.iter())
                    .find(|command| command.action_name == action)
                    .unwrap_or_else(|| panic!("{action} has a row"));
                assert_eq!(
                    command.when.as_deref(),
                    Some("Table && !CommandPalette"),
                    "{action} is the table's own key"
                );
                assert!(!command.description.trim().is_empty());
            }
            // Enter, Space and Delete are keys the recorder refuses, so a row for
            // them names the shortcut and points at the keymap file rather than
            // offering an Edit that cannot record what the keymap bound.
            for action in ["k8s_ops::DeleteSelection", "k8s_table::OpenDetails"] {
                let command = sections
                    .iter()
                    .flat_map(|section| section.commands.iter())
                    .find(|command| command.action_name == action)
                    .unwrap_or_else(|| panic!("{action} has a row"));
                assert!(
                    !command.editable,
                    "{action} is bound on a key the recorder refuses"
                );
            }
        });
    }

    #[gpui_kit::test]
    fn inspector_rows_are_listed_and_scoped_to_the_inspector(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let sections = keyboard_sections(cx, true);
            let commands = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .collect::<Vec<_>>();
            for action in [
                "k8s_inspector::ReloadActiveTab",
                "k8s_inspector::RetryMetrics",
                "k8s_inspector::MetricsRange1m",
                "k8s_inspector::MetricsRange15m",
                "k8s_inspector::MetricsRange1h",
                "k8s_inspector::ConfirmApply",
                "k8s_inspector::CancelApplyReview",
                "k8s_inspector::RevertYaml",
                "k8s_inspector::CopyYaml",
                "k8s_inspector::ToggleValueExpansion",
                "k8s_inspector::CopyValue",
                "k8s_inspector::NextProblem",
            ] {
                let command = commands
                    .iter()
                    .find(|command| command.action_name == action)
                    .unwrap_or_else(|| panic!("{action} is listed in the reference"));
                // The section is what Edit and Clear write into, so a row that
                // names no context would store a global override for a panel
                // shortcut.
                assert_eq!(command.context.as_deref(), Some(INSPECTOR_CONTEXT));
                assert!(command.editable, "{action} takes a user override");
                assert_eq!(command.when.as_deref(), Some(INSPECTOR_CONTEXT));
                assert_eq!(
                    command_chord(command, cx),
                    bound_chord_in_context(action, INSPECTOR_CONTEXT, cx),
                    "{action} shows the key the Inspector context binds"
                );
                assert!(!command.description.is_empty());
            }
        });
    }

    /// An editable shortcut says which context it is live in.
    #[gpui_kit::test]
    fn every_scoped_command_row_says_where_its_shortcut_is_live(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let sections = keyboard_sections(cx, true);
            let commands = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .collect::<Vec<_>>();
            let mut scoped = 0;
            for command in &commands {
                let Some(context) = canonical_context(&command.action_name) else {
                    // A globally bound command has no context to name, so its row
                    // correctly says nothing. The rendering has to agree.
                    assert!(
                        command.when.is_none(),
                        "{} is global and cannot claim a context",
                        command.action_name
                    );
                    continue;
                };
                assert_eq!(
                    command.when.as_deref(),
                    Some(context),
                    "{} is bound inside a context, so its row has to name it",
                    command.action_name
                );
                // The sentence the row prints has to read in words, not as a
                // predicate.
                let sentence = keyboard_context_description(context);
                assert_ne!(sentence, context);
                assert!(
                    !sentence.contains("&&") && !sentence.contains('!'),
                    "{} prints a predicate instead of a surface: {sentence}",
                    command.action_name
                );
                scoped += 1;
            }
            assert!(
                scoped > 30,
                "most of the list is scoped, or the contract is not being exercised: {scoped}"
            );
            let palette = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::ToggleCommandPalette")
                .expect("ToggleCommandPalette is listed");
            assert!(palette.when.is_none());
            assert!(palette.context.is_none());
        });
    }

    /// A parameterized action gets one row per input, and it is not editable.
    #[gpui_kit::test]
    fn keyboard_sections_list_parameterized_actions_without_edit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let sections = keyboard_sections(cx, true);
            let commands = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .collect::<Vec<_>>();
            let switch_tab = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::SwitchTab")
                .expect("SwitchTab is listed");
            assert!(!switch_tab.editable);
            assert!(switch_tab.action_input.is_some());
            assert!(switch_tab.label.starts_with("Switch to Tab "));
            assert_eq!(
                switch_tab.context.as_deref(),
                Some("Shell && !CommandPalette")
            );
            assert!(commands.iter().any(|command| {
                command.action_name == "k8s_shell::SwitchCluster"
                    && !command.editable
                    && command.action_input.is_some()
                    && command.context.as_deref() == Some("Shell && !CommandPalette")
            }));
        });
    }

    /// A newly bound shortcut records and clears, and the keycap follows.
    #[gpui_kit::test]
    fn a_newly_bound_inspector_shortcut_records_and_clears(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 960.);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.toggle_shortcut_reference(window, cx))
        });
        cx.run_until_parked();

        let action = "k8s_inspector::ReloadActiveTab";
        let sections = cx.read(|cx| keyboard_sections(cx, true));
        let command = sections
            .iter()
            .flat_map(|section| section.commands.iter())
            .find(|command| command.action_name == action)
            .cloned()
            .expect("the Inspector reload row is listed");
        let default_chord = cx.read(|cx| bound_chord_in_context(action, INSPECTOR_CONTEXT, cx));
        assert!(default_chord.is_some(), "the default keymap binds the key");
        assert_eq!(cx.read(|cx| command_chord(&command, cx)), default_chord);

        // Edit: the action has no user file entry, so recording writes the
        // Inspector section and releases the built-in key.
        let recorded = Keystroke::parse("ctrl-alt-f9").expect("test keystroke");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.start_recording(command.clone(), window, cx);
                view.handle_recording_keystroke(&recorded, window, cx);
            })
        });
        assert_eq!(view.read_with(cx, |view, _| view.error.clone()), None);
        let saved = cx.read(|cx| keymap::status(cx).user_source.clone());
        let saved = saved.expect("recording a shortcut writes the user keymap source");
        assert!(saved.contains("ctrl-alt-f9"), "{saved}");
        assert_eq!(
            cx.read(|cx| command_chord(&command, cx)).as_deref(),
            Some("ctrl-alt-f9")
        );

        // Clear: the row stops advertising a key, and the user keymap carries a
        // real unbind for the key the built-in keymap owned.
        view.update(cx, |view, cx| view.clear_binding(&command, cx));
        assert_eq!(view.read_with(cx, |view, _| view.error.clone()), None);
        assert_eq!(cx.read(|cx| command_chord(&command, cx)), None);
        let cleared = cx.read(|cx| keymap::status(cx).user_source.clone());
        let cleared = cleared.expect("clearing a shortcut keeps the user keymap source");
        assert!(cleared.contains("\"unbind\""), "{cleared}");
        assert!(cleared.contains(action), "{cleared}");
        discard_queued_keymap_write();
    }

    /// A key two actions claim is reported on the row that owns it, and a write
    /// that lands on a contested key does not claim plain success.
    #[gpui_kit::test]
    fn conflicting_binding_is_reported_on_its_own_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 960.);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.toggle_shortcut_reference(window, cx))
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-key-conflict-k8s_shell::CloseTab")
                .is_none(),
            "the default keymap has no conflicts"
        );

        let command = KeyboardCommand {
            action_name: "k8s_shell::CloseTab".to_owned(),
            label: "Close Tab".to_owned(),
            description: "Close the active center tab.".to_owned(),
            action_input: None,
            context: Some("Shell && !CommandPalette".to_owned()),
            when: None,
            editable: true,
        };
        cx.update(|_, cx| {
            let mut status = keymap::status(cx);
            status.conflicts = vec![KeyConflict {
                context: "Shell && !CommandPalette".to_owned(),
                keystrokes: "ctrl-shift-t".to_owned(),
                actions: vec![
                    "k8s_shell::CloseTab".to_owned(),
                    "k8s_shell::FocusYaml".to_owned(),
                ],
            }];
            cx.set_global(status);
            let conflict = conflict_for_command(&command, cx).expect("the row owns the conflict");
            assert_eq!(conflict.keystrokes, "ctrl-shift-t");
            assert_eq!(conflict.others, vec!["k8s_shell::FocusYaml".to_owned()]);
            assert!(conflict.label().contains("ctrl-shift-t"));
        });
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-key-conflict-k8s_shell::CloseTab")
                .is_some(),
            "the conflict is reported next to the binding that owns it"
        );

        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = notices.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, _, _| sink.borrow_mut().push(message));
        });
        view.update(cx, |view, cx| {
            view.handle_keybinding_update(
                &command,
                Ok(keymap::UserBindingUpdate::SavedAndApplied),
                "Keyboard shortcut saved.",
                "Saved but inactive",
                "Not saved",
                cx,
            );
        });
        assert!(
            notices.borrow().is_empty(),
            "no success toast over a conflict"
        );
        assert!(view.read_with(cx, |view, _| view.error.is_some()));
    }

    /// The key an action is bound to inside one context, read from the live
    /// keymap, because `secondary` is Ctrl on Linux and Command on macOS.
    fn bound_chord_in_context(action_name: &str, context: &str, cx: &App) -> Option<String> {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        keymap
            .bindings()
            .filter(|binding| binding.action().name() == action_name)
            .find(|binding| keymap::binding_context(binding).as_deref() == Some(context))
            .and_then(|binding| binding.keystrokes().first().map(|key| key.unparse()))
    }

    /// Drops a queued keymap write so the suite never reaches the real user
    /// keymap file.
    fn discard_queued_keymap_write() {
        let Some(path) = keymap::user_keymap_path() else {
            return;
        };
        let queue = k8s_core::atomic_file::path_task_queue();
        while queue.pop(&path).is_some() {
            queue.finish(&path);
        }
    }

    #[gpui_kit::test]
    fn saved_but_not_applied_keymap_result_shows_error_not_success(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = notices.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, _, _| sink.borrow_mut().push(message));
        });
        view.update(cx, |view, cx| {
            view.handle_keymap_update_result(
                Ok(keymap::UserBindingUpdate::SavedAndApplied),
                "Saved",
                "Saved but inactive",
                "Not saved",
                cx,
            );
            view.handle_keymap_update_result(
                Ok(keymap::UserBindingUpdate::SavedNotApplied),
                "Saved",
                "Saved but inactive",
                "Not saved",
                cx,
            );
        });
        assert_eq!(notices.borrow().len(), 1);
        assert_eq!(notices.borrow()[0], "Saved");
        assert_eq!(
            view.read_with(cx, |view, _| view.error.clone()),
            Some("Saved but inactive".to_owned())
        );
    }

    #[gpui_kit::test]
    fn keymap_reload_generation_refreshes_the_reference(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_app(cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = open(cx, 960.);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.toggle_shortcut_reference(window, cx))
        });
        cx.run_until_parked();
        let generation = view.read_with(cx, |view, _| view.keymap_generation);
        assert!(generation > 0);

        // An external edit of keymap.json rebuilds the bindings and bumps the
        // generation.
        cx.update(|_, cx| {
            let mut status = keymap::status(cx);
            status.epoch = status.epoch.wrapping_add(1);
            cx.set_global(status);
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.keymap_generation),
            generation
        );
        cx.executor().advance_clock(KEYMAP_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.keymap_generation),
            generation + 1,
            "the poll re-reads the generation without any navigation"
        );
    }

    // -- updates and the disk cache ----------------------------------------

    #[gpui_kit::test]
    fn update_actions_after_state_preserve_ready_and_downloading(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        let actions = UpdateActions::new(|_| {}, |_| {}, |_| {});
        for phase in [UpdatePhase::Ready, UpdatePhase::Downloading] {
            view.update(cx, |view, cx| {
                view.set_update_actions(None, cx);
                view.set_update_state(UpdateUiState::new(phase), cx);
                view.set_update_actions(Some(actions.clone()), cx);
                assert_eq!(view.update_state.phase, phase);
            });
        }
    }

    #[gpui_kit::test]
    fn missing_update_actions_show_unavailable_feedback(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        assert_eq!(
            view.read_with(cx, |view, _| (
                view.update_state.phase,
                view.update_state.error.clone(),
            )),
            (
                UpdatePhase::Unsupported,
                Some(UPDATER_UNAVAILABLE_REASON.to_owned())
            )
        );
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Updates, cx)
            })
        });
        cx.run_until_parked();
        let action = bounds(cx, Setting::CheckForUpdates.control_selector())
            .expect("update action is laid out");
        cx.simulate_click(action.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.error.clone()),
            Some(UPDATES_UNAVAILABLE_MESSAGE.to_owned())
        );
    }

    /// A downloaded update offers the restart, because a download that never
    /// restarts leaves the reader on the old build with no way forward.
    #[gpui_kit::test]
    fn ready_update_offers_the_restart_action(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        let restarts = Rc::new(RefCell::new(0));
        let counter = restarts.clone();
        let actions = UpdateActions::new(
            |_| {},
            |_| {},
            move |_| {
                *counter.borrow_mut() += 1;
            },
        );
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Updates, cx);
            view.set_update_actions(Some(actions), cx);
            view.set_update_state(UpdateUiState::new(UpdatePhase::Ready), cx);
        });
        cx.run_until_parked();
        let restart = bounds(cx, Setting::CheckForUpdates.control_selector())
            .expect("a ready update offers the restart");
        cx.simulate_click(restart.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(*restarts.borrow(), 1);
    }

    /// The environment's capabilities are reported, not set, and they settle.
    ///
    /// A probe that is still running shows the shared spinner and nothing else:
    /// §2.1 rule 12 says a skeleton over a value nobody is waiting for is worse
    /// than no marker.
    #[gpui_kit::test]
    fn a_cluster_capability_is_reported_and_settles(cx: &mut TestAppContext) {
        let (view, cx) = open(cx, 960.);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.select_category(SettingsCategory::Cluster, cx)
            })
        });
        cx.run_until_parked();
        assert!(bounds(cx, Setting::Helm.selector()).is_some());
        let unavailable = cx.update(|_, cx| {
            view.read_with(cx, |view, cx| {
                view.read(Setting::Helm, cx, &user_settings(cx))
            })
        });
        assert!(
            unavailable.as_text().contains("Checking"),
            "{unavailable:?}"
        );
        view.update(cx, |view, cx| {
            view.set_capabilities(HelmCapability::NotInstalled, Capability::Available, cx)
        });
        cx.run_until_parked();
        let settled = cx.update(|_, cx| {
            view.read_with(cx, |view, cx| {
                view.read(Setting::Helm, cx, &user_settings(cx))
            })
        });
        assert_eq!(settled.as_text(), Capability::Unavailable.label());
        // The reason behind the verdict is a tooltip, not a second line of grey
        // in the control column.
        assert!(view.read_with(cx, |view, _| {
            view.capability_reason(Setting::Helm).is_some()
        }));
        // And the answer is searchable, so "helm" finds the row.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("helm", cx)));
        cx.run_until_parked();
        assert!(bounds(cx, Setting::Helm.selector()).is_some());
    }

    fn init_disk_cache_test(cx: &mut TestAppContext, enabled: bool) {
        cx.update(init_app);
        cx.update(|_cx| {
            settings::set_test_settings_memory(&crate::settings::UserSettings {
                disk_cache: Some(enabled),
                ..Default::default()
            });
        });
    }

    /// The cache switch reads the app's cached state, not a second copy.
    #[gpui_kit::test]
    fn disk_cache_initializes_from_memory_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, false);
        settings::reset_test_load_count();
        let (_view, cx) = mount(cx, 960.);
        cx.update(|_, cx| assert!(!settings::disk_cache_enabled_from_app(cx)));
        // The cached state follows the remembered settings rather than a second
        // copy of them, so a read taken before the view exists and a read taken
        // after it cannot disagree.
        cx.update(|_, cx| settings::set_test_disk_cache(cx, true));
        cx.update(|_, cx| assert!(settings::disk_cache_enabled_from_app(cx)));
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui_kit::test]
    fn disk_cache_toggle_success_updates_cached_state_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, true);
        let (view, cx) = mount(cx, 960.);
        view.update(cx, |view, cx| {
            view.commit(Setting::Cache, Value::Bool(false), None, cx);
            assert!(!settings::disk_cache_enabled_from_app(cx));
            assert!(view.error.is_none());
        });
        settings::reset_test_load_count();
        cx.run_until_parked();
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui_kit::test]
    fn disk_cache_toggle_failure_preserves_cached_state_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, true);
        let (view, cx) = mount(cx, 960.);
        view.update(cx, |view, cx| {
            // The `SettingsStore` global must stay installed. `remove_global`
            // pushes a `NotifyGlobalObservers` effect, and the observer
            // `gpui_kit::init` registered for the theme registry reads a global
            // that is gone, which panics when the effect flushes. A value the
            // store rejects fails the same save synchronously, keeps the cached
            // `DiskCache` global, and touches no file.
            let result = settings::update(cx, |settings| {
                settings.disk_cache = Some(false);
                settings.extra.insert(
                    "buffer_font_size".to_owned(),
                    serde_json::Value::String("bad".to_owned()),
                );
            });
            assert!(result.is_err(), "fixture must fail the save");
            if let Err(reason) = result {
                view.set_error(format!(
                    "The disk cache setting was not saved: {reason}. Check write access, then retry."
                ));
            }
            assert!(
                settings::disk_cache_enabled_from_app(cx),
                "a refused write leaves the cached value alone"
            );
            assert!(view.error.is_some());
        });
        settings::reset_test_load_count();
        cx.run_until_parked();
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui_kit::test]
    fn external_disk_cache_update_refreshes_render_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, false);
        let (view, cx) = mount(cx, 960.);
        cx.update(|_, cx| assert!(!settings::disk_cache_enabled_from_app(cx)));
        settings::reset_test_load_count();
        view.update(cx, |_, cx| {
            settings::set_test_settings_memory(&crate::settings::UserSettings {
                disk_cache: Some(true),
                ..Default::default()
            });
            settings::sync_disk_cache(cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert!(settings::disk_cache_enabled_from_app(cx)));
        assert_eq!(settings::test_load_count(), 0);
    }

    /// A settings window and a settings tab are one store, not two.
    ///
    /// `UI-SPEC` §9.3 makes Settings its own window, and the main window can still
    /// hold a settings tab while that ships, so two views can be on screen at once.
    /// The failure this guards is a value written in one and not seen by the other:
    /// it looks like a lag, and the reader concludes their change was lost. Nothing
    /// else in the surface would report it, because each view is internally
    /// consistent.
    #[gpui_kit::test]
    fn a_second_view_sees_what_the_first_one_wrote(cx: &mut TestAppContext) {
        cx.update(init_app);
        let (first, cx) = mount(cx, 960.);
        let second = cx.new(SettingsView::new);
        cx.run_until_parked();

        first.update(cx, |view, cx| {
            view.commit(Setting::TabWidth, Value::Choice("4".to_owned()), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            second.read_with(cx, |view, cx| view
                .results(cx)
                .into_iter()
                .find(|(setting, _)| *setting == Setting::TabWidth)
                .map(|(_, value)| value.as_text())),
            Some("4".to_owned()),
            "the window-hosted view must read what the other one wrote"
        );
        // And the session facts reach a view that never saw the shell that probed
        // them, which is the whole point of publishing them rather than pushing.
        cx.update(|_, cx| {
            SettingsEnvironment::set_capabilities(
                cx,
                HelmCapability::NotInstalled,
                Capability::Available,
                None,
                None,
            )
        });
        cx.run_until_parked();
        let reported = |view: &Entity<SettingsView>, cx: &mut TestAppContext| {
            cx.read(|cx| {
                view.read_with(cx, |view, cx| {
                    view.results(cx)
                        .into_iter()
                        .find(|(setting, _)| *setting == Setting::Helm)
                        .map(|(_, value)| value.as_text())
                })
            })
        };
        assert_eq!(reported(&second, cx), Some("Not found".to_owned()));
        assert_eq!(
            reported(&first, cx),
            Some("Not found".to_owned()),
            "a probe that lands while both views are open reaches both"
        );
    }
}
