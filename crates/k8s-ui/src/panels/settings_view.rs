use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    Global, Hsla, InteractiveElement, IntoElement, Keystroke, ParentElement, Render, Role,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Toggled,
    Window, div, point, px,
};
use ui::prelude::*;
use ui::{
    ContextMenu, IconPosition, KeyBinding as UiKeyBinding, PopoverMenu, PopoverMenuHandle,
    TintColor, Tooltip,
};

use crate::design::{self, Severity, space};
use crate::keymap::{self, KeyConflict};
use crate::panels::common::spinner;
use crate::panels::helm::HelmCapability;
use crate::settings::{self, DiskCache, ReduceMotionMode, SettingsStore, ThemeChoice};
use crate::shell::commands::{CommandRun, UPDATER_UNAVAILABLE_REASON, demo_commands_with_updater};
use crate::table_view::TextInput;
use crate::update::{UpdateActions, UpdatePhase, UpdateUiState};

/// Settings layout dimensions.
///
/// `DESIGN.md` §3.2 requires every value to sit on the 4px scale or to be declared as its own
/// component-contract role, so each width below names the role it plays instead of being a
/// free-floating number: the sidebar holds a search field plus four category labels, the label
/// column is the readable width for the longest setting name, the value column holds the widest
/// control in the surface, and the content cap keeps a settings pane at a measure a person can scan.
///
/// The label column and the value column are fixed, and the value column is the row's last one, so
/// it ends on the content measure's trailing edge. A settings row is read as a pair: the label
/// measure is what the description wraps to, and one right-hand line of controls is what the eye
/// runs down. Letting the value column start at a fixed offset from the leading edge instead put a
/// pop-up button a third of the way across a 1920px window with the rest of the measure empty.
const SETTINGS_SIDEBAR_WIDTH: f32 = 208.; // 26 * space::SM: search field plus four category rows.
const SETTINGS_LABEL_WIDTH: f32 = 220.; // 55 * space::XS: the label column, one width for every row.
const SETTINGS_CONTENT_MAX_WIDTH: f32 = 720.; // 45 * space::LG: both columns plus the gaps that hold them apart.
const SETTINGS_CONTROL_WIDTH: f32 = 240.; // The widest control is a medium button with a keycap.
/// Rail width for the collapsed category sidebar. Same role as the Hotbar rail in `design::size`.
const SETTINGS_SIDEBAR_RAIL_WIDTH: f32 = 40.;
/// The drawn box inside a boolean control's 28px target.
///
/// Square, because a desktop check box is a square: a pill implied a sliding switch, and the row
/// beside it answers with pop-up buttons.
const CHECKBOX_BOX: gpui::Pixels = px(16.);
/// Narrowest window the shell allows (`window_min_size`). The two-column row cannot fit below it,
/// so this is also the width at which the layout stacks.
const SETTINGS_MIN_WINDOW_WIDTH: f32 = 960.;
/// How often the settings surface re-reads the keymap generation counter.
///
/// The keyboard rows read the live keymap at render time, and an external edit of `keymap.json`
/// rebuilds the bindings without notifying this view, so the list would keep the keycaps it had
/// when it last drew. A cheap counter comparison keeps the list honest without a keystroke.
const KEYMAP_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Tab order inside the settings surface.
///
/// One number per control, grouped by the block the control belongs to, so a sidebar button can
/// never collide with the search field's clear button and a keyboard row cannot land on a General
/// row control. The search field owns 0 and its clear button owns 1 (`TextInput::new`).
///
/// GPUI walks the stops of one frame in slot order (`TabStopMap` sorts by tab index, not by paint
/// order), so within a block the slot a control carries is the position Tab reaches it at. Each
/// block therefore numbers its controls top to bottom, the reading order a person expects from
/// Tab, which is what `hig/focus-and-selection.md` asks of a custom view.
mod tab_order {
    pub const SIDEBAR_FIRST: isize = 2;
    pub const SIDEBAR_TOGGLE: isize = SIDEBAR_FIRST;
    pub const GENERAL_FIRST: isize = 10;
    /// The data font size pop-up follows the four General switches.
    pub const GENERAL_DATA_FONT: isize = GENERAL_FIRST + 4;
    /// The user keymap file block, numbered in the order the block is drawn: the path line with
    /// Copy Path sits above the row of file actions, so Tab reaches it first.
    pub const KEYMAP_FIRST: isize = 20;
    pub const KEYMAP_COPY_PATH: isize = KEYMAP_FIRST;
    pub const KEYMAP_OPEN: isize = KEYMAP_FIRST + 1;
    pub const KEYMAP_RELOAD: isize = KEYMAP_FIRST + 2;
    pub const KEYMAP_RESTORE: isize = KEYMAP_FIRST + 3;
    /// Cancel only exists while the restore step is armed, and it follows the destructive button
    /// it undoes.
    pub const KEYMAP_CANCEL: isize = KEYMAP_FIRST + 4;
    pub const KEYBOARD_FIRST: isize = 30;
    /// The About pane numbers its controls in draw order: the version copy, then the update
    /// check, then the restart a ready update adds.
    pub const ABOUT_FIRST: isize = 80;
}
const THEME_DESCRIPTION: &str =
    "Choose a K8s Studio or Zed theme. The System theme follows the desktop setting.";
const GENERAL_DESCRIPTION: &str = "Configure appearance, motion, accessibility, and disk cache.";
const INCREASE_CONTRAST_DESCRIPTION: &str =
    "Increase text, icon, and border contrast throughout the workbench.";
const DISK_CACHE_DESCRIPTION: &str = "Cache discovery results and snapshots to reduce startup time. Secrets are not stored on disk. Takes effect for new sessions.";
/// The line the disk cache row shows.
///
/// `DESIGN.md` §3.1 gives `metadata` to counts and secondary notes, and a paragraph that carries
/// an assurance nobody asked for is four lines of 11px grey. The assurance answers a question only
/// the person switching the setting on has, so it stays in [`DISK_CACHE_DESCRIPTION`], which the
/// control offers as its description and its tooltip. Nothing is lost; it stops being in everyone's
/// way.
const DISK_CACHE_SUMMARY: &str =
    "Cache discovery results and snapshots to reduce startup time. Takes effect for new sessions.";
const REDUCE_MOTION_DESCRIPTION: &str = "Reduce non-essential animation and motion. System uses the motion setting the app starts with; On and Off override it. Takes effect immediately.";
/// The line the reduce motion row shows, with the per-answer wording behind the control.
///
/// Three answers cannot be said in two words, and the answer a person is reading is the value the
/// pop-up already prints. The meaning of the other two belongs on the control, not in the
/// paragraph every reader of the pane pays for.
const REDUCE_MOTION_SUMMARY: &str =
    "Reduce non-essential animation and motion. Takes effect immediately.";
/// What each of the three answers means, for the control rather than for the pane.
const REDUCE_MOTION_DETAIL: &str =
    "System uses the motion setting the app starts with. On and Off override it.";

/// Text sizes offered for the data surfaces, in pixels.
///
/// The interface cannot scale its chrome without changing every spacing token and hit
/// area at the same time, so the honest size control is the one that already works
/// end to end: the data surfaces read this value for their font, their row height, and
/// their column measurements, so the table and the editor grow together.
///
/// Steps are discrete rather than free. A person choosing a text size wants to compare
/// a few readable options, not compose an arbitrary one, and a discrete list is
/// reachable from the keyboard without a spin button.
const DATA_FONT_SIZES: &[f32] = &[11., 12., 13., 14., 16., 18.];

const DATA_FONT_SIZE_DESCRIPTION: &str = "Set the text size for resource tables, logs, and the YAML editor. Larger sizes also grow the row height so text is never cropped.";

/// The button and menu label for one size, in the words a person would say.
/// The data font size the surfaces currently measure with.
fn data_font_size(cx: &App) -> f32 {
    f32::from(crate::settings::data_typography(cx).size)
}

fn data_font_label(size: f32) -> String {
    if size == settings::PRODUCT_DATA_FONT_SIZE {
        format!("{size:.0} px (default)")
    } else {
        format!("{size:.0} px")
    }
}
const KEYBOARD_DESCRIPTION: &str = "Application shortcuts and the user keymap file.";
/// The Inspector section of the built-in keymap.
///
/// The Inspector is a sibling of the Table, so its keys live in their own context and an edit or a
/// clear has to name this section: an override written without a context is global, and a global
/// Inspector shortcut would also fire outside the panel.
const INSPECTOR_CONTEXT: &str = "Inspector && !CommandPalette";
/// Section title for the Inspector rows in the Keyboard list.
const INSPECTOR_GROUP: &str = "Inspector";
/// The recording rule, in the words the error banner uses, so the hint and the failure agree.
const RECORDING_REJECTED_KEYS: &str = "a letter or number without a modifier, a bare F1, a bare key above F24, or an unmodified Home, End, Page Up, Page Down, arrow, Enter, Space, Tab, Insert, Backspace, or Delete key";
/// Bare keys the recorder refuses, with the name the error banner gives each one.
///
/// The rule and the sentence a person reads after pressing a refused key read this one table,
/// so a key the recorder turns down is always named in the message that explains why. A key
/// the rule refuses and the copy does not mention is the worst of the three outcomes: the
/// person presses it again and learns nothing.
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
const RESTORE_DEFAULTS_LABEL: &str = "Restore Defaults";
const RESTORE_DEFAULTS_DESCRIPTION: &str =
    "Delete the user keymap file and reload the built-in shortcuts.";
const RESTORE_DEFAULTS_CONFIRM_LABEL: &str = "Delete and Reload";
const RESTORE_DEFAULTS_CANCEL_LABEL: &str = "Cancel";
const COPY_PATH_LABEL: &str = "Copy Path";
/// Label for the control that writes the settings file again after a failed write.
const RETRY_LABEL: &str = "Retry";
/// Shown when a background write of the settings file failed after the control already flipped.
const SAVE_FAILED_MESSAGE: &str =
    "The last settings change was not saved. Check write access to the settings file, then retry.";
/// The note a row carries while the write that changed it failed.
///
/// The recovery control is a banner, and `feedback.md` asks for an error as close to the problem as
/// possible. A person who flipped this check box is looking at this row, so the row says the
/// change did not land and the banner keeps the sentence and the retry.
const SAVE_NOT_SAVED_NOTE: &str = "Not saved";
/// The theme family that ships with the product. Everything else in the menu is a Zed theme.
const K8S_STUDIO_THEME_PREFIX: &str = "K8s Studio";
const INTEGRATIONS_DESCRIPTION: &str = "Optional tools used by the workbench.";
const HELM_DESCRIPTION: &str =
    "Install the Helm CLI to inspect and manage releases. k8s-gpui does not include Helm.";
const METRICS_DESCRIPTION: &str =
    "Use metrics-server in the selected cluster to read CPU and memory values.";
const ABOUT_DESCRIPTION: &str = "View version and update information for k8s-gpui.";
/// What the Version row is for.
///
/// A version string is the first line of every bug report, so the row says what the value is for
/// and the control beside it puts it on the clipboard. A bare `Label` cannot be selected, so the
/// number a person needs was on screen and not reachable.
const VERSION_DESCRIPTION: &str = "The installed k8s-gpui build. Copy it into a bug report.";
const COPY_VERSION_LABEL: &str = "Copy";
const UPDATE_DESCRIPTION: &str = "Check for a newer k8s-gpui build for this installation.";
const UPDATES_UNAVAILABLE_MESSAGE: &str =
    "Application updates are unavailable in this build. Use a k8s-gpui build with update support.";

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

/// The modifiers a shortcut may use, named the way the platform names them.
///
/// `Modifiers::modified` is what the recorder asks, and it reports control, alt, shift, and
/// the platform key, so this list is those four and no others. Naming three of them — as an
/// earlier version did — tells a person that a key they are holding does not count when the
/// recorder accepts it.
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
///
/// The rule and the sentence are the same contract, so a person who reads the hint knows
/// which keys to press before the recorder refuses one.
fn recording_hint() -> String {
    format!(
        "Press a key with {modifier}, or press F2–F24 on its own. Press Escape to cancel.",
        modifier = modifier_names()
    )
}

fn keymap_description() -> String {
    format!(
        "Create a JSONC template or show the file path. {} shortcuts use {}. Saved changes reload automatically.",
        platform_name(),
        modifier_names()
    )
}

fn keymap_reload_description() -> String {
    "Reload the user keymap and apply the saved shortcuts.".to_owned()
}

fn unsupported_update_state() -> UpdateUiState {
    UpdateUiState::new(UpdatePhase::Unsupported).with_error(UPDATER_UNAVAILABLE_REASON)
}

/// The three answers to "how much motion", including the one that is not an override.
///
/// `reduce_motion` is stored as an `Option` and `None` is the state the app boots into, so a
/// two-value control cannot show it: the first press on a switch would turn an inherited value
/// into a stored one without ever saying that it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReduceMotionChoice {
    /// No stored choice. The app's own motion setting decides.
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
    /// `System` stores nothing, so the app falls back to the motion setting it starts with,
    /// and this build starts with motion allowed. That is the honest answer to show rather
    /// than a remembered override the row has just given up.
    fn applied(self) -> bool {
        matches!(self, Self::On)
    }
}

/// The reduce-motion choice the settings file asks for, without the built-in default folded in.
///
/// The store's default settings carry `reduce_motion: "off"`, so the merged value can never
/// say "nobody chose" and would make `System` unreachable. The raw user content can say it,
/// and `None` there is exactly the `System` state.
fn stored_reduce_motion(cx: &App) -> Option<ReduceMotionMode> {
    cx.try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings())
        .and_then(|content| content.content.reduce_motion)
}

fn reduce_motion_choice(cx: &App) -> ReduceMotionChoice {
    match stored_reduce_motion(cx) {
        Some(ReduceMotionMode::On) => ReduceMotionChoice::On,
        Some(ReduceMotionMode::Off) => ReduceMotionChoice::Off,
        None => ReduceMotionChoice::System,
    }
}

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
    /// `DESIGN.md` §4 gives the hollow ring to `info / syncing` and the dash to "no verdict", so a
    /// probe still running is `Info` and a tool that is not installed is `Muted`. The shape comes
    /// from [`design::health_icon`], which is why the two cannot collapse into one another.
    fn severity(self) -> Severity {
        match self {
            Self::Checking => Severity::Info,
            Self::Available => Severity::Success,
            Self::Unavailable => Severity::Muted,
            Self::Error => Severity::Error,
        }
    }
}

type ThemeHandler = Rc<dyn Fn(ThemeChoice, &mut Window, &mut App) -> Result<(), String>>;
type KeymapHandler = Rc<dyn Fn(&mut Window, &mut App)>;
type NoticeHandler = Rc<dyn Fn(String, Severity, &mut App)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsCategory {
    General,
    Keyboard,
    Integrations,
    About,
}

impl SettingsCategory {
    const ALL: [Self; 4] = [
        Self::General,
        Self::Keyboard,
        Self::Integrations,
        Self::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Keyboard => "Keyboard",
            Self::Integrations => "Integrations",
            Self::About => "About",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Keyboard => IconName::Keyboard,
            Self::Integrations => IconName::Server,
            Self::About => IconName::Info,
        }
    }

    /// The settings this pane owns, in draw order, each with the description its row shows.
    ///
    /// The Keyboard pane has no entry here because its rows are the keymap's own commands. Every
    /// other pane names its rows in one list, so the filter the pane draws with and the count the
    /// toolbar reports read the same rows instead of two hand-written copies of them.
    fn settings(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::General => &[
                ("Theme", THEME_DESCRIPTION),
                ("Increase contrast", INCREASE_CONTRAST_DESCRIPTION),
                ("Disk cache", DISK_CACHE_DESCRIPTION),
                ("Reduce motion", REDUCE_MOTION_DESCRIPTION),
                // The data font size is a row the pane draws, so it is a row the search counts
                // and filters. Leaving it out made a query about text size find no settings at
                // all: the count said zero, the pane was dropped, and the control the person was
                // looking for was never mounted.
                ("Data font size", DATA_FONT_SIZE_DESCRIPTION),
            ],
            Self::Integrations => &[
                ("Helm", HELM_DESCRIPTION),
                ("Cluster metrics", METRICS_DESCRIPTION),
            ],
            Self::About => &[
                ("Version", VERSION_DESCRIPTION),
                ("Application updates", UPDATE_DESCRIPTION),
            ],
            Self::Keyboard => &[],
        }
    }
}

/// The settings layout a person left behind.
///
/// A settings tab is a place a person returns to, so the category and the sidebar state outlive
/// the view that drew them: closing the tab and opening it again lands on the same pane instead
/// of resetting to General.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SettingsLayout {
    category: Option<SettingsCategory>,
    sidebar_collapsed: bool,
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

/// Which pane owns the single error slot.
///
/// The banner is a fixed strip under the toolbar, so it stays put while the content scrolls, and
/// it only shows while the pane that produced it is on screen: a failed write in About must not
/// appear over the Keyboard list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ErrorScope {
    /// Not category specific: a settings write failed, so every pane shows it.
    #[default]
    Any,
    Category(SettingsCategory),
}

/// A keybinding conflict that belongs to one keyboard row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BindingConflict {
    keystrokes: String,
    /// The other actions bound to the same key in the same context, without this row's action.
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

/// The two steps of the Restore Defaults confirmation.
///
/// Deleting the user keymap file cannot be undone, so `KEYMAP.md` promises a confirmation before
/// the delete. `Destructive` is the armed step: the button has changed to the destructive one, the
/// row says what it deletes, and Escape or Cancel returns to `Idle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RestoreStep {
    /// Nothing armed. The button reads Restore Defaults and only arms.
    #[default]
    Idle,
    /// The button reads Delete and Reload, and the next press deletes the file and reloads.
    Destructive,
}

pub struct SettingsView {
    theme_focus: FocusHandle,
    theme_menu: PopoverMenuHandle<ContextMenu>,
    data_font_focus: FocusHandle,
    data_font_menu: PopoverMenuHandle<ContextMenu>,
    reduce_motion_focus: FocusHandle,
    reduce_motion_menu: PopoverMenuHandle<ContextMenu>,
    recording_focus: FocusHandle,
    search_focus: FocusHandle,
    search_input: Entity<TextInput>,
    search_query: String,
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
    recording: Option<Recording>,
    recording_intercept: Option<Subscription>,
    /// The armed Restore Defaults step. `Destructive` is the second click.
    restore_step: RestoreStep,
    /// Which pane owns `error`, so the banner follows the pane that produced it.
    error_scope: ErrorScope,
    /// Async settings write state. A write failure arrives here, not in the `Result` of the click.
    save_pending: bool,
    save_error: Option<String>,
    /// Keymap generation the rows were last drawn from.
    keymap_generation: u64,
    /// The command list the Keyboard pane draws, kept between frames.
    ///
    /// `keyboard_sections` rebuilds the whole command registry, which allocates a few hundred
    /// strings and boxes an action per command. The pane asked for it up to four times in one
    /// frame — the toolbar count, the sidebar match, the content match, and the rows — so the
    /// answer is memoized against the three inputs it reads: the keymap generation, whether an
    /// updater is wired, and how many actions are registered.
    keyboard_sections: RefCell<Option<KeyboardSections>>,
    sidebar_collapsed: bool,
    _keymap_poll: Task<()>,
    _save_status_observer: Subscription,
    _settings_observer: Subscription,
    _disk_cache_observer: Subscription,
    _search_intercept: Subscription,
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let view = cx.weak_entity();
        let search_input = cx.new(|cx| {
            TextInput::new("Search settings…", cx, move |text, cx| {
                let view = view.clone();
                let text = text.to_owned();
                cx.defer(move |cx| {
                    let _ = view.update(cx, |view, cx| view.set_search_query(&text, cx));
                });
            })
            .with_accessibility(
                "Search settings",
                "Enter a name or description to filter settings. Press Escape to clear the search.",
                "Clear Settings Search",
            )
            .with_width(px(SETTINGS_SIDEBAR_WIDTH - f32::from(space::MD) * 2.))
        });
        let search_focus = search_input.read(cx).focus_handle(cx);
        let search_listener = cx.listener(|view, event: &gpui::KeystrokeEvent, window, cx| {
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
        // The settings file is written on a background task, so the `Result` of a click is not the
        // result of the save. `settings::SettingsSaveStatus` carries the async outcome.
        let save_status_observer = cx.observe_global::<settings::SettingsSaveStatus>(|view, cx| {
            view.apply_save_status(cx);
        });
        let layout = layout_state(cx);
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
            theme_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(tab_order::GENERAL_FIRST),
            theme_menu: PopoverMenuHandle::default(),
            data_font_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(tab_order::GENERAL_FIRST + 4),
            data_font_menu: PopoverMenuHandle::default(),
            reduce_motion_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(tab_order::GENERAL_FIRST + 3),
            reduce_motion_menu: PopoverMenuHandle::default(),
            recording_focus: cx.focus_handle().tab_stop(false),
            search_focus,
            search_input,
            search_query: String::new(),
            category: layout.category.unwrap_or(SettingsCategory::General),
            scroll: ScrollHandle::new(),
            helm: HelmCapability::Checking,
            metrics: Capability::Checking,
            helm_error: None,
            metrics_error: None,
            error: None,
            update_state: unsupported_update_state(),
            update_actions: None,
            on_theme: None,
            on_create_or_show_keymap: None,
            on_reload_keymap: None,
            on_notice: None,
            recording: None,
            recording_intercept: None,
            restore_step: RestoreStep::Idle,
            error_scope: ErrorScope::Any,
            save_pending: save.pending,
            save_error: save.error,
            keymap_generation,
            keyboard_sections: RefCell::new(None),
            sidebar_collapsed: layout.sidebar_collapsed,
            _keymap_poll: keymap_poll,
            _save_status_observer: save_status_observer,
            _settings_observer: settings_observer,
            _disk_cache_observer: disk_cache_observer,
            _search_intercept: search_intercept,
        }
    }

    /// The window title the shell should show while this pane is open.
    ///
    /// The title names the pane, so a person with several tabs open can tell which one is which.
    pub fn pane_title(&self) -> String {
        if self.search_query.trim().is_empty() {
            return format!("Settings · {}", self.category.label());
        }
        "Settings · Search".to_owned()
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

    pub fn focus_handle(&self) -> FocusHandle {
        self.search_focus.clone()
    }

    fn set_search_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.search_query == query {
            return;
        }
        self.search_query = query.to_owned();
        self.reset_pane_state(cx);
        cx.notify();
    }

    /// Clears the query through the field, so the field and the filter cannot disagree.
    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.set_search_query("", cx);
    }

    /// Returns the surface to the state a first visit has.
    ///
    /// A new query or a new category replaces the rows on screen, so everything that belonged to
    /// the rows that are gone goes with them. An armed Restore Defaults step would otherwise
    /// leave a destructive button waiting for a second press on a pane nobody is looking at, an
    /// open pop-up would float with no trigger left under it, and the list would keep the scroll
    /// offset of a longer pane and open halfway down a shorter one.
    fn reset_pane_state(&mut self, cx: &mut App) {
        self.restore_step = RestoreStep::Idle;
        self.theme_menu.hide(cx);
        self.data_font_menu.hide(cx);
        self.reduce_motion_menu.hide(cx);
        self.scroll.set_offset(point(px(0.), px(0.)));
    }

    fn matches_query(&self, value: &str) -> bool {
        let query = self.search_query.trim().to_ascii_lowercase();
        query.is_empty() || value.to_ascii_lowercase().contains(&query)
    }

    fn matches_setting(&self, title: &str, description: &str) -> bool {
        self.matches_query(title) || self.matches_query(description)
    }

    fn keyboard_search_is_broad(&self) -> bool {
        let query = self.search_query.trim().to_ascii_lowercase();
        ["keyboard", "shortcut", "keymap", "keybinding"]
            .iter()
            .any(|term| query.contains(term))
    }

    fn keyboard_search_matches_command(
        &self,
        section: &KeyboardSection,
        command: &KeyboardCommand,
    ) -> bool {
        self.search_query.trim().is_empty()
            || self.keyboard_search_is_broad()
            || self.matches_query(&section.title)
            || self.matches_query(&command.label)
            || self.matches_query(&command.description)
            || self.matches_query(&command.action_name)
            || command
                .when
                .as_deref()
                .is_some_and(|when| self.matches_query(when))
    }

    fn keyboard_search_matches_section(&self, section: &KeyboardSection) -> bool {
        self.search_query.trim().is_empty()
            || self.keyboard_search_is_broad()
            || section
                .commands
                .iter()
                .any(|command| self.keyboard_search_matches_command(section, command))
    }

    fn keyboard_search_has_header_match(&self) -> bool {
        self.search_query.trim().is_empty()
            || self.keyboard_search_is_broad()
            || self.matches_query(KEYBOARD_DESCRIPTION)
            || self.matches_query("path")
            || self.matches_query("file")
            || self.matches_query(&keymap_description())
    }

    /// The command list the Keyboard pane draws, rebuilt only when an input it reads has moved.
    ///
    /// The list depends on the keymap, on whether an updater is wired, and on the registered
    /// actions. The keymap is the only one that changes while a person is reading the pane, and it
    /// publishes a generation for exactly this, so the pane is redrawn from a fresh list on a
    /// reload instead of rebuilding one four times per frame.
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

    fn keyboard_search_has_match(&self, cx: &App) -> bool {
        self.keyboard_search_has_header_match()
            || self
                .keyboard_sections(cx)
                .iter()
                .any(|section| self.keyboard_search_matches_section(section))
    }

    /// The categories the search keeps on screen.
    ///
    /// An empty query shows one pane. A query filters the sidebar too, so a result can live in a
    /// pane the person never opened, and the list has to be the panes that still have something to
    /// show: `render_category` drops the rest, so a longer list here would name a pane nothing on
    /// screen refers to.
    fn visible_categories(&self, cx: &App) -> Vec<SettingsCategory> {
        if self.search_query.trim().is_empty() {
            return vec![self.category];
        }
        SettingsCategory::ALL
            .into_iter()
            .filter(|category| self.category_has_match(*category, cx))
            .collect()
    }

    /// The category the sidebar highlights: the one on screen, or every category while a search
    /// filters the list.
    fn highlighted_category(&self) -> Option<SettingsCategory> {
        (self.search_query.trim().is_empty()).then_some(self.category)
    }

    /// How many settings the current query matched, for the toolbar count.
    ///
    /// A search result with no count gives a person no way to tell a filtered list from an empty
    /// one, so the toolbar states how many settings survived the filter. Every term comes from
    /// [`Self::matched_settings`] or the keyboard row filter, which is also what the panes draw
    /// with, so the count can never promise rows the surface does not show.
    fn search_result_count(&self, cx: &App) -> usize {
        self.visible_categories(cx)
            .into_iter()
            .map(|category| match category {
                SettingsCategory::Keyboard => {
                    usize::from(self.keyboard_search_has_header_match())
                        + self
                            .keyboard_sections(cx)
                            .iter()
                            .map(|section| {
                                section
                                    .commands
                                    .iter()
                                    .filter(|command| {
                                        self.keyboard_search_matches_command(section, command)
                                    })
                                    .count()
                            })
                            .sum::<usize>()
                }
                category => self.matched_settings(category),
            })
            .sum()
    }

    /// Whether the query names the pane itself, which is the one answer that keeps every row.
    ///
    /// A category is found by its own name. The section description is header prose that lists
    /// what the pane is for, so it cannot widen a row filter: `disk cache` is a word in General's
    /// description, and matching on it turned one setting into a pane-wide hit that the count then
    /// had to report as four.
    fn pane_show_all(&self, category: SettingsCategory) -> bool {
        self.matches_query(category.label())
    }

    /// Whether one row of a pane survives the query, by the rule the pane draws it with.
    fn setting_matches(&self, category: SettingsCategory, title: &str, description: &str) -> bool {
        self.pane_show_all(category) || self.matches_setting(title, description)
    }

    /// The settings of one category that survive the query, using the same rule the pane renders.
    fn matched_settings(&self, category: SettingsCategory) -> usize {
        category
            .settings()
            .iter()
            .filter(|(title, description)| self.setting_matches(category, title, description))
            .count()
    }

    /// Opens a category. The query stays: a person who searched and then picked a sidebar category
    /// asked to see that category, not to lose the search. Escape, the field's clear button, and
    /// the Search field all remove it.
    fn select_category(&mut self, category: SettingsCategory, cx: &mut Context<Self>) {
        self.category = category;
        self.reset_pane_state(cx);
        self.remember_layout(cx);
        cx.notify();
    }

    fn remember_layout(&mut self, cx: &mut Context<Self>) {
        remember_layout(
            cx,
            SettingsLayout {
                category: Some(self.category),
                sidebar_collapsed: self.sidebar_collapsed,
            },
        );
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.remember_layout(cx);
        cx.notify();
    }

    /// Escape leaves the armed Restore Defaults step.
    ///
    /// The check sits on the surface root, so the key only reaches it while this pane owns the
    /// focus. A keystroke interceptor would be global and would swallow an Escape meant for the
    /// table behind the tab.
    fn on_settings_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "escape" || event.keystroke.modifiers.modified() {
            return;
        }
        if self.restore_step == RestoreStep::Idle {
            return;
        }
        cx.stop_propagation();
        self.restore_step = RestoreStep::Idle;
        cx.notify();
    }

    fn handle_search_escape(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keystroke.key != "escape"
            || keystroke.modifiers.modified()
            || !self.search_focus.is_focused(window)
        {
            return;
        }
        if !self.search_query.trim().is_empty() {
            cx.stop_propagation();
            self.clear_search(cx);
            return;
        }
        // Escape unwinds one layer at a time: the armed confirmation before the tab itself.
        if self.restore_step != RestoreStep::Idle {
            cx.stop_propagation();
            self.restore_step = RestoreStep::Idle;
            cx.notify();
            return;
        }
        window.dispatch_action(Box::new(crate::shell::CloseTab), cx);
    }

    fn notice(&self, message: String, severity: Severity, cx: &mut App) {
        if let Some(handler) = &self.on_notice {
            handler(message, severity, cx);
        }
    }

    fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.error_scope = ErrorScope::Any;
    }

    fn clear_error(&mut self) {
        self.error = None;
        self.error_scope = ErrorScope::Any;
    }

    /// Records an error that belongs to one pane, so the fixed banner only shows it there.
    fn set_pane_error(&mut self, category: SettingsCategory, error: String) {
        self.error = Some(error);
        self.error_scope = ErrorScope::Category(category);
    }

    /// The banner, if the pane that produced it is on screen.
    ///
    /// A search draws every pane at once, so then any error has a home. Without a search only the
    /// selected pane is on screen, and a failure in About must not appear over the Keyboard list.
    fn visible_error(&self) -> Option<String> {
        let error = self.error.clone()?;
        match self.error_scope {
            ErrorScope::Any => Some(error),
            ErrorScope::Category(category) => {
                (category == self.category || !self.search_query.trim().is_empty()).then_some(error)
            }
        }
    }

    /// Reads the async outcome of the last settings write.
    ///
    /// `settings::update` returns before the file is written, so an optimistic toggle is
    /// rolled back by a failure that only the background task sees. `SettingsSaveStatus` is the
    /// record of that task, and it also carries the pending flag, which is the only honest answer
    /// to "did my change stick yet". The rows read the same failure through
    /// [`Self::save_failure_note`], so the sentence on the row and the banner cannot disagree.
    fn apply_save_status(&mut self, cx: &mut Context<Self>) {
        let status = settings::save_status(cx);
        self.save_pending = status.pending;
        self.save_error = status.error;
        cx.notify();
    }

    /// Writes the settings file again after a write failed.
    ///
    /// The failure message tells the person to check write access and retry, so the retry has
    /// to be somewhere to press. `settings::retry_rejected_save` goes through the same queue
    /// and the same status the first attempt used, so a file that is writable again is fixed
    /// without a restart and without a second code path for the writer to disagree with.
    ///
    /// It writes the value the file refused, not the value the store holds: a failed write
    /// puts the store back, so re-saving the store would write what the reader is trying to
    /// change away from and report success. The row beside the button says "Not saved", and
    /// that is what a retry is about.
    ///
    /// The failure is not cleared here. `settings::update` returns before the file is touched, so
    /// clearing now would take the sentence and the retry off screen and put them back a moment
    /// later if the write failed again. The write's own outcome arrives through
    /// [`Self::apply_save_status`], which is the one place that decides whether the failure stands.
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

    /// Re-reads the keymap generation the rows are drawn from.
    ///
    /// The keyboard rows read the live keymap, and an external edit of `keymap.json` rebuilds the
    /// bindings without touching this view, so the keycaps would keep the values from the last
    /// draw. Comparing the counter is enough: an unchanged generation means nothing to redraw.
    fn refresh_keymap_generation(&mut self, cx: &mut Context<Self>) {
        let generation = keymap::status(cx).epoch;
        if generation == self.keymap_generation {
            return;
        }
        self.keymap_generation = generation;
        cx.notify();
    }

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

    /// Reports the outcome of writing one command's binding.
    ///
    /// `update_user_binding_with_outcome` only inspects keymap errors, so a write that lands
    /// next to an existing binding of the same key still answers "saved and applied".
    /// Telling the person "Keyboard shortcut saved." while the key belongs to two actions is
    /// the contradiction this removes: the row that owns the key says so next to the keycap,
    /// and the success message waits until the key is unambiguous.
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
            self.set_pane_error(
                SettingsCategory::Keyboard,
                format!(
                    "The shortcut was saved, but {label} Change one of them, then record the shortcut again.",
                    label = conflict.label()
                ),
            );
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

    fn apply_disk_cache_result(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            // A one-click, immediately reversible switch is not a completed activity: `feedback.md`
            // says a person only needs to hear when it did not work, and the control already shows
            // the new answer. The failure is the half that is worth a sentence.
            Ok(()) => self.clear_error(),
            Err(_error) => {
                self.set_error(
                    "The disk cache setting was not saved. Check write access to the settings file, then retry."
                        .to_owned(),
                );
            }
        }
        cx.notify();
    }

    fn apply_theme_choice(
        &mut self,
        choice: ThemeChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handler) = &self.on_theme {
            match handler(choice, window, cx) {
                Ok(()) => self.clear_error(),
                Err(_error) => self.set_error(
                    "The theme setting was not saved. Check write access to the settings file, then retry."
                        .to_owned(),
                ),
            }
        }
        cx.notify();
    }

    /// Applies a data font size and repaints every surface that measures text.
    ///
    /// The data surfaces read the size for their font, their row height, and their
    /// column widths, so a change has to reach the windows rather than just this
    /// pane; otherwise the trigger would show a new number while the table behind it
    /// stayed the old size.
    fn apply_data_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let low = *DATA_FONT_SIZES
            .first()
            .unwrap_or(&settings::PRODUCT_DATA_FONT_SIZE);
        let high = *DATA_FONT_SIZES
            .last()
            .unwrap_or(&settings::PRODUCT_DATA_FONT_SIZE);
        let size = size.clamp(low, high);
        match crate::settings::set_data_font_size(cx, size) {
            // The pop-up button reads the chosen size, so a toast that says it was set is a second
            // copy of the control. It was `Severity::Success`, so it also spent the one channel
            // `DESIGN.md` §2 budgets for boldness, the health channel, on a dropdown.
            Ok(()) => self.clear_error(),
            Err(_error) => {
                self.set_error(
                    "The font size was not saved. Check write access to the settings file, then retry."
                        .to_owned(),
                );
            }
        }
        // The data surfaces read the size for their font, their row height, and their column
        // widths, so every window has to repaint. Refreshing only this pane would leave the
        // trigger showing a new number while the table behind it kept the old size.
        cx.refresh_windows();
        cx.notify();
    }

    fn start_recording(
        &mut self,
        command: KeyboardCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(window, cx);
        self.clear_error();
        // The recorder owns the keyboard while it is open, so an armed destructive step must not
        // wait behind it.
        self.restore_step = RestoreStep::Idle;
        let previous_focus = window.focused(cx);
        self.recording = Some(Recording {
            command,
            previous_focus,
        });
        let listener = cx.listener(|view, event: &gpui::KeystrokeEvent, window, cx| {
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
            self.set_pane_error(
                SettingsCategory::Keyboard,
                format!(
                    "Use a modifier with the key, or press F2–F24 on its own. {} cannot be recorded.",
                    RECORDING_REJECTED_KEYS
                ),
            );
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
            "The shortcut was saved, but it is not active. Fix the keymap file, then select Reload Keymap.",
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
            "The shortcut was cleared in the keymap, but it is still active. Fix the keymap file, then select Reload Keymap.",
            "The shortcut was not cleared. Check write access to the keymap file, then select Clear again.",
            cx,
        );
        cx.notify();
    }

    /// Arms the Restore Defaults confirmation. Nothing is deleted yet.
    fn request_restore_defaults(&mut self, cx: &mut Context<Self>) {
        self.restore_step = RestoreStep::Destructive;
        self.clear_error();
        cx.notify();
    }

    fn cancel_restore_defaults(&mut self, cx: &mut Context<Self>) {
        self.restore_step = RestoreStep::Idle;
        cx.notify();
    }

    /// Deletes the user keymap override and reloads the built-in shortcuts.
    ///
    /// This is the second, destructive step. The first press only arms the step, and the button
    /// says what it will delete before it does.
    fn perform_restore_defaults(&mut self, cx: &mut Context<Self>) {
        self.restore_step = RestoreStep::Idle;
        match keymap::restore_defaults(cx) {
            Ok(()) => {
                self.clear_error();
                self.keymap_generation = keymap::status(cx).epoch;
                self.notice(
                    "Default shortcuts restored. The user keymap file was deleted.".to_owned(),
                    Severity::Info,
                    cx,
                );
            }
            Err(_error) => self.set_pane_error(
                SettingsCategory::Keyboard,
                "The default shortcuts were not restored. Check write access to the keymap file, then try again."
                    .to_owned(),
            ),
        }
        cx.notify();
    }

    /// The first press arms the step; the second press deletes the file and reloads.
    fn on_restore_defaults(&mut self, cx: &mut Context<Self>) {
        if self.restore_step == RestoreStep::Destructive {
            self.perform_restore_defaults(cx);
        } else {
            self.request_restore_defaults(cx);
        }
    }

    fn render_recording_status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.recording.as_ref().map(|recording| {
            let label = format!("Recording shortcut for {}", recording.command.label);
            let hint = recording_hint();
            h_flex()
                .id("settings-recording-status")
                .w_full()
                .px(space::LG)
                .py(space::SM)
                .gap(space::SM)
                .items_center()
                .track_focus(&self.recording_focus)
                .role(Role::Status)
                .aria_label(label.clone())
                .aria_description(hint)
                .border_l_2()
                .border_color(cx.theme().colors().border_focused)
                .child(Icon::new(IconName::Keyboard).size(IconSize::XSmall))
                .child(
                    Label::new(label)
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Default),
                )
                .into_any_element()
        })
    }

    /// The fixed status strip under the toolbar.
    ///
    /// It lives outside the scroll container so a failure cannot scroll out of sight, and it shows
    /// the pending write as well as the failed one: the click that started the write returns before
    /// the file is touched, so "Saving…" is the honest state in between.
    fn render_status_strip(&self, cx: &Context<Self>) -> Option<AnyElement> {
        // A background write failed after the control had already flipped, so this is the only
        // place the failure can surface. The copy says what to do, the button does it, and the
        // raw reason is a tooltip.
        if let Some(reason) = self.save_error.clone() {
            let retry = div()
                .flex_none()
                .debug_selector(|| "settings-save-retry".to_owned())
                .child(
                    Button::new("settings-save-retry", RETRY_LABEL)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .aria_label("Write the Settings File Again")
                        .tooltip(Tooltip::text("Save the settings file again."))
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
        if let Some(error) = self.visible_error() {
            return Some(
                self.status_banner("settings-error", error, None, None, cx)
                    .into_any_element(),
            );
        }
        if !self.save_pending {
            return None;
        }
        Some(
            h_flex()
                .id("settings-save-pending")
                .debug_selector(|| "settings-save-pending".to_owned())
                .w_full()
                .flex_none()
                .px(space::LG)
                .py(space::SM)
                .gap(space::SM)
                .items_center()
                .role(Role::Status)
                .aria_label("Saving settings.")
                .child(spinner(
                    IconName::LoadCircle,
                    Color::Muted,
                    IconSize::XSmall,
                    cx,
                ))
                .child(
                    Label::new("Saving settings…")
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }

    /// One error line. The reason is a description and a tooltip, never the visible sentence.
    ///
    /// The sentence and the recovery control share the content measure. The strip is a `w_full`
    /// row on the surface root, so a spacer between the two pushed `Retry` to the far edge of a
    /// 1920px window: 1888px of nothing between the promise and the button that keeps it. The
    /// inner row takes the same 720px cap the content column has, so the eye runs one line.
    fn status_banner(
        &self,
        id: &'static str,
        text: String,
        reason: Option<String>,
        action: Option<AnyElement>,
        _cx: &Context<Self>,
    ) -> AnyElement {
        // The action sits on the trailing side of the capped measure, so the sentence reads first
        // and the recovery follows it instead of competing with it for the eye.
        let mut line = h_flex()
            .w_full()
            .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
            .py(space::SM)
            .gap(space::SM)
            .items_center()
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Error),
            )
            .child(
                Label::new(text.clone())
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Error),
            )
            .child(div().flex_1());
        line = line.when_some(action, |this, action| this.child(action));
        let mut banner = h_flex()
            .id(id)
            .debug_selector(move || id.to_owned())
            .w_full()
            .flex_none()
            .px(space::LG)
            .role(Role::Alert)
            .aria_label(text)
            .child(line);
        if let Some(reason) = reason {
            banner = banner.aria_description(reason.clone());
            banner.interactivity().tooltip(Tooltip::text(reason));
        }
        banner.into_any_element()
    }

    fn render_theme(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let current = settings::theme_choice(cx);
        let selected = current.clone();
        let themes = theme::ThemeRegistry::global(cx)
            .list_names()
            .into_iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        let label = current.label().to_owned();
        let handle = self.theme_menu.clone();
        let expand_handle = handle.clone();
        let collapse_handle = handle.clone();
        let view = cx.entity().downgrade();
        let menu = PopoverMenu::new("settings-theme-menu")
            .menu(move |window, cx| {
                let current = selected.clone();
                let groups = theme_groups(themes.clone());
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let system_view = view.clone();
                    let menu = menu.toggleable_entry(
                        ThemeChoice::System.label(),
                        current == ThemeChoice::System,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            if let Some(view) = system_view.upgrade() {
                                view.update(cx, |view, cx| {
                                    view.apply_theme_choice(ThemeChoice::System, window, cx)
                                });
                            }
                        },
                    );
                    // Fourteen entries in one column with a single rule read as a list of names.
                    // Two named groups let a person recognise the product themes first and skip
                    // the imported ones.
                    groups
                        .into_iter()
                        .fold(menu.separator(), |menu, (title, names)| {
                            let mut menu = menu.header(title).separator();
                            for name in names {
                                let choice = ThemeChoice::named(name);
                                let choice_label = choice.label().to_owned();
                                let choice_view = view.clone();
                                menu = menu.toggleable_entry(
                                    choice_label,
                                    current == choice,
                                    IconPosition::Start,
                                    None,
                                    move |window, cx| {
                                        let choice = choice.clone();
                                        if let Some(view) = choice_view.upgrade() {
                                            view.update(cx, |view, cx| {
                                                view.apply_theme_choice(choice, window, cx)
                                            });
                                        }
                                    },
                                );
                            }
                            menu
                        })
                }))
            })
            .with_handle(handle.clone())
            .trigger(
                Button::new("settings-theme-trigger", label.clone())
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .tab_index(tab_order::GENERAL_FIRST)
                    .track_focus(&self.theme_focus)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(format!("Theme: {label}")))
                    .aria_role(Role::ComboBox)
                    .aria_value(label)
                    .aria_label("Theme")
                    .aria_description(THEME_DESCRIPTION)
                    .aria_expanded(handle.is_deployed())
                    .on_a11y_action(gpui::accesskit::Action::Expand, move |_, window, cx| {
                        expand_handle.show(window, cx)
                    })
                    .on_a11y_action(gpui::accesskit::Action::Collapse, move |_, _, cx| {
                        collapse_handle.hide(cx)
                    }),
            );
        setting_row("Theme", THEME_DESCRIPTION, compact, cx)
            .child(
                setting_control(compact).child(
                    div()
                        .flex_none()
                        .debug_selector(|| "settings-theme-trigger".to_owned())
                        .child(menu),
                ),
            )
            .into_any_element()
    }

    /// Reports the outcome of writing Increase contrast.
    ///
    /// The whole workbench repaints on success, so the person sees the change in the surface around
    /// the control that flipped. A toast on top of that says the same thing from a corner of the
    /// window, and the control is a check box that cannot hold two states at once.
    fn apply_increase_contrast_result(
        &mut self,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.clear_error();
                cx.refresh_windows();
            }
            Err(_error) => {
                self.set_error(
                    "Increase contrast was not saved. Check write access to the settings file, then retry."
                        .to_owned(),
                );
            }
        }
        cx.notify();
    }

    /// The note a row carries while the write that changed it failed.
    ///
    /// `DESIGN.md` §9 promises a recoverable error for a failed write, and the recovery control is
    /// a banner under the toolbar. A person who flipped this control is reading this row, so the
    /// row says the change did not land; the banner keeps the sentence and the `Retry`.
    fn save_failure_note(&self, control_id: &str) -> Option<RowNote> {
        self.save_error.as_ref()?;
        Some(RowNote {
            id: format!("settings-row-note-{control_id}"),
            icon: design::health_icon(Severity::Error),
            text: SAVE_NOT_SAVED_NOTE.to_owned(),
            tone: Severity::Error,
        })
    }

    fn render_increase_contrast(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let enabled = settings::increase_contrast_enabled(cx);
        // The next value is read here, from the same state the box was drawn from, so the
        // click and the drawn state can never disagree.
        let next = !enabled;
        setting_row_with_note(
            "Increase contrast",
            INCREASE_CONTRAST_DESCRIPTION,
            self.save_failure_note("settings-increase-contrast"),
            compact,
            cx,
        )
        .child(
            setting_control(compact).child(
                checkbox_control(
                    "settings-increase-contrast",
                    enabled,
                    tab_order::GENERAL_FIRST + 1,
                    "Increase contrast",
                    INCREASE_CONTRAST_DESCRIPTION,
                    cx,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    let result = settings::set_increase_contrast(cx, next);
                    view.apply_increase_contrast_result(result, cx);
                })),
            ),
        )
        .into_any_element()
    }

    fn render_disk_cache(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let enabled = settings::disk_cache_enabled_from_app(cx);
        let next = !enabled;
        setting_row_with_note(
            "Disk cache",
            DISK_CACHE_SUMMARY,
            self.save_failure_note("settings-disk-cache"),
            compact,
            cx,
        )
        .child(
            setting_control(compact).child(
                checkbox_control(
                    "settings-disk-cache",
                    enabled,
                    tab_order::GENERAL_FIRST + 2,
                    if enabled {
                        "Disable Disk Cache"
                    } else {
                        "Enable Disk Cache"
                    },
                    DISK_CACHE_DESCRIPTION,
                    cx,
                )
                // The assurance sentence stays with the control, so a person can read it without
                // every row in the pane carrying it in grey.
                .tooltip(Tooltip::text(DISK_CACHE_DESCRIPTION))
                .on_click(cx.listener(move |view, _, _, cx| {
                    let result = settings::update(cx, |settings| settings.disk_cache = Some(next));
                    view.apply_disk_cache_result(result, cx);
                })),
            ),
        )
        .into_any_element()
    }

    /// Reduce motion as a pop-up button, because the answer has three values.
    ///
    /// A switch can only say on or off, and "no choice written" is a real third state: it is
    /// what the app boots into, and the first press on a switch would replace it with a stored
    /// answer without telling anyone that it had. The pop-up keeps that state selectable, so a
    /// person can hand the decision back to the app.
    fn render_reduce_motion(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let choice = reduce_motion_choice(cx);
        let label = choice.label().to_owned();
        let handle = self.reduce_motion_menu.clone();
        let expand_handle = handle.clone();
        let collapse_handle = handle.clone();
        let view = cx.entity().downgrade();
        let menu = PopoverMenu::new("settings-reduce-motion-menu")
            .menu(move |window, cx| {
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    ReduceMotionChoice::ALL
                        .into_iter()
                        .fold(menu, |menu, entry| {
                            let view = view.clone();
                            menu.toggleable_entry(
                                entry.label(),
                                entry == choice,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    if let Some(view) = view.upgrade() {
                                        view.update(cx, |view, cx| {
                                            view.apply_reduce_motion(entry, cx)
                                        });
                                    }
                                },
                            )
                        })
                }))
            })
            .with_handle(handle.clone())
            .trigger(
                Button::new("settings-reduce-motion-trigger", label.clone())
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .tab_index(tab_order::GENERAL_FIRST + 3)
                    .track_focus(&self.reduce_motion_focus)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    // The three answers cannot be said in the pop-up button's own label, so the
                    // meaning of the two the person is not choosing sits here instead of in the
                    // paragraph the whole pane shows.
                    .tooltip(Tooltip::text(REDUCE_MOTION_DETAIL))
                    .aria_role(Role::ComboBox)
                    .aria_value(label)
                    .aria_label("Reduce motion")
                    .aria_description(REDUCE_MOTION_DESCRIPTION)
                    .aria_expanded(handle.is_deployed())
                    .on_a11y_action(gpui::accesskit::Action::Expand, move |_, window, cx| {
                        expand_handle.show(window, cx)
                    })
                    .on_a11y_action(gpui::accesskit::Action::Collapse, move |_, _, cx| {
                        collapse_handle.hide(cx)
                    }),
            );
        setting_row("Reduce motion", REDUCE_MOTION_SUMMARY, compact, cx)
            .child(
                setting_control(compact).child(
                    div()
                        .flex_none()
                        .debug_selector(|| "settings-reduce-motion".to_owned())
                        .child(menu),
                ),
            )
            .into_any_element()
    }

    /// Writes the reduce-motion choice and applies it.
    fn apply_reduce_motion(&mut self, choice: ReduceMotionChoice, cx: &mut Context<Self>) {
        let mode = choice.mode();
        let result = settings::update(cx, |settings| settings.reduce_motion = mode);
        match result {
            // The pop-up button already reads the chosen answer, so the success toast repeated it
            // from across the window. Only the failure is worth a sentence.
            Ok(()) => {
                // `System` stores nothing, so there is no value for the settings observer to
                // install and the row has to put the app's own motion setting back itself.
                cx.set_reduce_motion(choice.applied());
                self.clear_error();
            }
            Err(_error) => {
                self.set_error(
                    "Reduce motion was not saved. Check write access to the settings file, then retry."
                        .to_owned(),
                );
            }
        }
        cx.notify();
    }

    /// The user keymap file block.
    ///
    /// This is a block and not a two-column row because it owns four controls, a path, and a
    /// destructive confirmation. The fixed value column cannot hold them without clipping, and a
    /// confirmation that has to explain what it deletes needs the width to do it.
    fn render_keymap_file(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let path = keymap::user_keymap_path();
        let path_label = path.as_ref().map_or_else(
            || {
                "The file path is unavailable. Check access to the user configuration directory."
                    .to_owned()
            },
            |path| path.display().to_string(),
        );
        let armed = self.restore_step == RestoreStep::Destructive;
        // A file path is data, and the reader can set the data font size, so this label reads
        // the configured size rather than the default one. A path set at 12px next to a path
        // set at 16px in the same panel is two designs, and a wider font in a fixed box clips
        // the directory the reader needs to recognise.
        let data_typography = settings::data_typography(cx);
        let mut path_text = h_flex()
            .id("settings-keymap-path")
            .flex_1()
            .min_w(px(0.))
            .child(
                Label::new(path_label.clone())
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        data_typography.size,
                    ))))
                    .color(Color::Muted)
                    .truncate(),
            );
        if path.is_some() {
            path_text = path_text.aria_label(format!("Keymap file path: {path_label}"));
            path_text
                .interactivity()
                .tooltip(Tooltip::text(path_label.clone()));
        }
        let path_line = h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .child(path_text)
            .child(
                div().flex_none().child(
                    Button::new("settings-keymap-copy-path", COPY_PATH_LABEL)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .tab_index(tab_order::KEYMAP_COPY_PATH)
                        .disabled(path.is_none())
                        .aria_label("Copy the Keymap File Path")
                        .tooltip(Tooltip::text("Copy the keymap file path."))
                        .on_click(
                            cx.listener(|view, _: &ClickEvent, _, cx| view.copy_keymap_path(cx)),
                        ),
                ),
            );
        let confirm = self.render_restore_confirmation(compact, cx);
        v_flex()
            .id("settings-keymap-file")
            .w_full()
            .gap(space::SM)
            .px(space::LG)
            .py(space::SM)
            .min_h(design::size::ROW)
            .font_ui(cx)
            .child(setting_row_label(
                "User keymap file",
                keymap_description(),
                compact,
                cx,
            ))
            .child(path_line)
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap(space::SM)
                    .items_center()
                    .child(
                        Button::new("settings-keymap-create-show", "Open Keymap File")
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Medium)
                            .tab_index(tab_order::KEYMAP_OPEN)
                            .aria_label("Open the User Keymap File")
                            .tooltip(Tooltip::text("Create or show the user keymap file."))
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                if let Some(handler) = &view.on_create_or_show_keymap {
                                    handler(window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("settings-keymap-reload", "Reload Keymap")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .tab_index(tab_order::KEYMAP_RELOAD)
                            .aria_label("Reload the User Keymap")
                            .tooltip(Tooltip::text(keymap_reload_description()))
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                if let Some(handler) = &view.on_reload_keymap {
                                    handler(window, cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .debug_selector(|| "settings-keymap-restore-defaults".to_owned())
                            .child(
                                Button::new(
                                    "settings-keymap-restore-defaults",
                                    if armed {
                                        RESTORE_DEFAULTS_CONFIRM_LABEL
                                    } else {
                                        RESTORE_DEFAULTS_LABEL
                                    },
                                )
                                .style(if armed {
                                    ButtonStyle::Tinted(TintColor::Error)
                                } else {
                                    ButtonStyle::Subtle
                                })
                                .size(ButtonSize::Medium)
                                .tab_index(tab_order::KEYMAP_RESTORE)
                                .aria_label(RESTORE_DEFAULTS_LABEL)
                                .aria_description(RESTORE_DEFAULTS_DESCRIPTION)
                                .tooltip(Tooltip::text(RESTORE_DEFAULTS_DESCRIPTION))
                                .on_click(cx.listener(
                                    |view, _: &ClickEvent, _, cx| view.on_restore_defaults(cx),
                                )),
                            ),
                    )
                    .when(armed, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .debug_selector(|| "settings-keymap-restore-cancel".to_owned())
                                .child(
                                    Button::new(
                                        "settings-keymap-restore-cancel",
                                        RESTORE_DEFAULTS_CANCEL_LABEL,
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Medium)
                                    .tab_index(tab_order::KEYMAP_CANCEL)
                                    .aria_label("Cancel Restore Defaults")
                                    .tooltip(Tooltip::text(
                                        "Keep the user keymap file and the shortcuts in it.",
                                    ))
                                    .on_click(cx.listener(
                                        |view, _: &ClickEvent, _, cx| {
                                            view.cancel_restore_defaults(cx)
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .when_some(confirm, |this, confirm| this.child(confirm))
            .into_any_element()
    }

    /// Puts the keymap path on the clipboard, which is the only way to paste it into a file
    /// manager, an editor, or a bug report.
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

    /// The second step of the Restore Defaults confirmation.
    ///
    /// `alerts.md` › Buttons asks a destructive alert to say what it will do and to offer Cancel on
    /// the leading side. Inline, that becomes the warning line beside the destructive button, and
    /// Escape leaves the step.
    fn render_restore_confirmation(&self, compact: bool, cx: &Context<Self>) -> Option<AnyElement> {
        if self.restore_step == RestoreStep::Idle {
            return None;
        }
        let target = keymap::user_keymap_path().map_or_else(
            || "the user keymap file".to_owned(),
            |path| path.display().to_string(),
        );
        let text = restore_confirmation_text(&target);
        Some(
            h_flex()
                .id("settings-restore-confirm")
                .debug_selector(|| "settings-restore-confirm".to_owned())
                .w_full()
                .flex_wrap()
                .gap(space::SM)
                .items_center()
                .role(Role::Alert)
                .aria_label(format!("{text} This cannot be undone. Escape cancels."))
                .border_l_2()
                .border_color(cx.theme().colors().border_variant)
                .when(!compact, |this| this.ml(space::LG))
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            Label::new(text.clone())
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Error),
                        )
                        .child(
                            Label::new("This cannot be undone. Escape cancels.")
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_capability(
        &self,
        compact: bool,
        title: &'static str,
        description: &'static str,
        capability: Capability,
        error: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let label = if title == "Helm" {
            self.helm.label()
        } else {
            self.metrics.label()
        }
        .to_owned();
        let mut status = h_flex()
            .id(title)
            .min_h(design::size::CONTROL)
            .flex_none()
            .gap(space::XS)
            .items_center()
            .role(Role::Status)
            .aria_label(format!("{title}: {label}"))
            .child(capability_glyph(capability, cx))
            .child(
                Label::new(label)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .truncate(),
            );
        if let Some(reason) = error {
            status = status.aria_description(reason);
            status
                .interactivity()
                .tooltip(Tooltip::text(reason.to_owned()));
        }
        setting_row(title, description, compact, cx)
            .child(setting_control(compact).child(status))
            .into_any_element()
    }

    fn render_update_status(&self, cx: &Context<Self>) -> AnyElement {
        let state = self.update_state.clone();
        let unavailable = self.update_actions.is_none();
        let phase = if unavailable {
            UpdatePhase::Unsupported
        } else {
            state.phase
        };
        let status = if phase == UpdatePhase::Unsupported {
            UPDATES_UNAVAILABLE_MESSAGE.to_owned()
        } else {
            state.status_text()
        };
        let icon = match phase {
            UpdatePhase::Idle => IconName::Info,
            UpdatePhase::UpToDate => IconName::Check,
            UpdatePhase::Checking | UpdatePhase::Downloading => IconName::LoadCircle,
            UpdatePhase::Ready => IconName::Check,
            UpdatePhase::Restarting => IconName::RotateCw,
            UpdatePhase::Failed => IconName::Warning,
            UpdatePhase::Unsupported => IconName::Dash,
        };
        let severity = match phase {
            UpdatePhase::Ready | UpdatePhase::UpToDate => Severity::Success,
            UpdatePhase::Failed => Severity::Error,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting => {
                Severity::Info
            }
            UpdatePhase::Idle | UpdatePhase::Unsupported => Severity::Muted,
        };
        let icon = if matches!(
            phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Restarting
        ) {
            // The shared spinner, so this row stops rotating when the user asked for less motion
            // like every other one instead of carrying a third private period.
            spinner(
                icon,
                Color::Custom(severity.marker(cx)),
                IconSize::XSmall,
                cx,
            )
        } else {
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(Color::Custom(severity.marker(cx)))
                .into_any_element()
        };
        let role = if phase == UpdatePhase::Failed {
            Role::Alert
        } else {
            Role::Status
        };
        let mut status_element = h_flex()
            .id("settings-update-status")
            .flex_1()
            .min_w(px(0.))
            .gap(space::XS)
            .items_start()
            .role(role)
            .aria_label(status.clone())
            .child(icon)
            .child(
                // The note line owns the full width, so the explanation is read, not truncated
                // into a 240px value column with the detail left to a tooltip.
                Label::new(status)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(if phase == UpdatePhase::Failed {
                        Color::Error
                    } else {
                        Color::Default
                    }),
            );
        if let Some(error) = state.error.filter(|_| !unavailable) {
            status_element = status_element.aria_description(error.clone());
            status_element.interactivity().tooltip(Tooltip::text(error));
        }
        status_element.into_any_element()
    }

    fn render_about(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let keep = |title: &str, description: &str| {
            self.setting_matches(SettingsCategory::About, title, description)
        };
        let mut content = v_flex()
            .w_full()
            .child(section_header("About", ABOUT_DESCRIPTION, cx));
        if keep("Version", VERSION_DESCRIPTION) {
            let version = settings::app_version().to_owned();
            content = content.child(
                setting_row("Version", VERSION_DESCRIPTION, compact, cx).child(
                    setting_control(compact).child(
                        h_flex()
                            .flex_none()
                            .min_h(design::size::CONTROL)
                            .gap(space::SM)
                            .items_center()
                            .child(Label::new(version.clone()).size(LabelSize::Custom(
                                rems_from_px(f32::from(design::text::BODY)),
                            )))
                            // A version number cannot be selected out of a `Label`, and it is the
                            // first line of every bug report, so the value gets the same copy
                            // control the keymap path already has.
                            .child(
                                div()
                                    .flex_none()
                                    .debug_selector(|| "settings-copy-version-action".to_owned())
                                    .child(
                                        Button::new("settings-copy-version", COPY_VERSION_LABEL)
                                            .style(ButtonStyle::Subtle)
                                            .size(ButtonSize::Medium)
                                            .tab_index(tab_order::ABOUT_FIRST)
                                            .aria_label("Copy the Version Number")
                                            .tooltip(Tooltip::text("Copy the version number."))
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    version.to_owned(),
                                                ));
                                            })),
                                    ),
                            ),
                    ),
                ),
            );
        }
        if keep("Application updates", UPDATE_DESCRIPTION) {
            content = content.child(
                setting_row("Application updates", UPDATE_DESCRIPTION, compact, cx).child(
                    setting_control(compact).child(
                        div()
                            .flex_none()
                            .min_h(design::size::CONTROL)
                            .debug_selector(|| "settings-check-updates-action".to_owned())
                            .child(
                                Button::new("settings-check-updates", "Check for Updates")
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Medium)
                                    .tab_index(tab_order::ABOUT_FIRST + 1)
                                    .aria_label("Check for Application Updates")
                                    .tooltip(Tooltip::text("Check for application updates."))
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.check_for_updates(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
                ),
            );
            // A ready update adds a second action, and two medium buttons measure about 308px
            // against a 240px control column. Putting them side by side pushed the first one
            // 68px into the label column, so the restart takes its own line under the row and
            // still ends on the same trailing edge as every other control in the pane.
            if let Some(label) = self.update_restart_label() {
                content =
                    content.child(
                        h_flex()
                            .id("settings-restart-update-line")
                            .w_full()
                            .px(space::LG)
                            .pb(space::SM)
                            .justify_end()
                            .child(
                                // The line is a layout device; the control is what the name refers to,
                                // so the selector stays on the box around the button and not on the
                                // full-width row, whose centre is empty space.
                                div()
                                    .id("settings-restart-update")
                                    .debug_selector(|| "settings-restart-update".to_owned())
                                    .flex_none()
                                    .min_h(design::size::CONTROL)
                                    .child(
                                        Button::new("settings-restart-update-action", label)
                                            .style(ButtonStyle::Tinted(TintColor::Accent))
                                            .size(ButtonSize::Medium)
                                            .tab_index(tab_order::ABOUT_FIRST + 2)
                                            .aria_label("Restart to Apply the Update")
                                            .tooltip(Tooltip::text(
                                                "Restart k8s-gpui and apply the downloaded update.",
                                            ))
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.restart_to_update(cx)
                                            })),
                                    ),
                            ),
                    );
            }
            // The status sits under its own row so the sentence is readable at any window width.
            let status = self.render_update_status(cx);
            content = content.child(
                h_flex()
                    .id("settings-update-note")
                    .w_full()
                    .px(space::LG)
                    .pb(space::SM)
                    .gap(space::SM)
                    .items_start()
                    .child(status),
            );
        }
        content.into_any_element()
    }

    /// The button a Ready update needs, or nothing while the update is not ready.
    ///
    /// A download that finished but never restarts leaves the person on the old build with no way
    /// forward, so the state that can act has to offer the action.
    fn update_restart_label(&self) -> Option<&'static str> {
        (self.update_state.phase == UpdatePhase::Ready && self.update_actions.is_some())
            .then_some("Restart to Update")
    }

    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(actions) = self.update_actions.clone() else {
            self.set_error(UPDATES_UNAVAILABLE_MESSAGE.to_owned());
            cx.notify();
            return;
        };
        actions.run_restart(cx);
    }

    fn render_general(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let keep = |title: &str, description: &str| {
            self.setting_matches(SettingsCategory::General, title, description)
        };
        let mut content =
            v_flex()
                .w_full()
                .child(section_header("General", GENERAL_DESCRIPTION, cx));
        if keep("Theme", THEME_DESCRIPTION) {
            content = content.child(self.render_theme(compact, cx));
        }
        if keep("Increase contrast", INCREASE_CONTRAST_DESCRIPTION) {
            content = content.child(self.render_increase_contrast(compact, cx));
        }
        if keep("Disk cache", DISK_CACHE_DESCRIPTION) {
            content = content.child(self.render_disk_cache(compact, cx));
        }
        if keep("Reduce motion", REDUCE_MOTION_DESCRIPTION) {
            content = content.child(self.render_reduce_motion(compact, cx));
        }
        if keep("Data font size", DATA_FONT_SIZE_DESCRIPTION) {
            content = content.child(self.render_data_font_size(compact, cx));
        }
        content.into_any_element()
    }

    fn render_data_font_size(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let current = data_font_size(cx);
        let label = data_font_label(current);
        let handle = self.data_font_menu.clone();
        let expand_handle = handle.clone();
        let collapse_handle = handle.clone();
        let view = cx.entity().downgrade();
        let menu = PopoverMenu::new("settings-data-font-menu")
            .menu(move |window, cx| {
                let current = current;
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    DATA_FONT_SIZES.iter().fold(menu, |menu, size| {
                        let size = *size;
                        let view = view.clone();
                        menu.toggleable_entry(
                            data_font_label(size),
                            current == size,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |view, cx| view.apply_data_font_size(size, cx));
                                }
                            },
                        )
                    })
                }))
            })
            .with_handle(handle.clone())
            .trigger(
                Button::new("settings-data-font-trigger", label.clone())
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .tab_index(tab_order::GENERAL_DATA_FONT)
                    .track_focus(&self.data_font_focus)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(format!("Data font size: {label}")))
                    .aria_role(Role::ComboBox)
                    .aria_value(label)
                    .aria_label("Data font size")
                    .aria_description(DATA_FONT_SIZE_DESCRIPTION)
                    .aria_expanded(handle.is_deployed())
                    .on_a11y_action(gpui::accesskit::Action::Expand, move |_, window, cx| {
                        expand_handle.show(window, cx)
                    })
                    .on_a11y_action(gpui::accesskit::Action::Collapse, move |_, _, cx| {
                        collapse_handle.hide(cx)
                    }),
            );
        setting_row("Data font size", DATA_FONT_SIZE_DESCRIPTION, compact, cx)
            .child(
                setting_control(compact).child(
                    div()
                        .flex_none()
                        .debug_selector(|| "settings-data-font-trigger".to_owned())
                        .child(menu),
                ),
            )
            .into_any_element()
    }

    fn render_integrations(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let keep = |title: &str, description: &str| {
            self.setting_matches(SettingsCategory::Integrations, title, description)
        };
        let mut content =
            v_flex()
                .w_full()
                .child(section_header("Integrations", INTEGRATIONS_DESCRIPTION, cx));
        if keep("Helm", HELM_DESCRIPTION) {
            let status = match self.helm {
                HelmCapability::Checking => Capability::Checking,
                HelmCapability::Available => Capability::Available,
                HelmCapability::NotInstalled => Capability::Unavailable,
                HelmCapability::Timeout | HelmCapability::Error => Capability::Error,
            };
            content = content.child(self.render_capability(
                compact,
                "Helm",
                HELM_DESCRIPTION,
                status,
                self.helm.reason().or(self.helm_error.as_deref()),
                cx,
            ));
        }
        if keep("Cluster metrics", METRICS_DESCRIPTION) {
            content = content.child(self.render_capability(
                compact,
                "Cluster metrics",
                METRICS_DESCRIPTION,
                self.metrics,
                self.metrics_error.as_deref(),
                cx,
            ));
        }
        content.into_any_element()
    }

    fn render_keyboard_command(
        &self,
        compact: bool,
        command: &KeyboardCommand,
        tab_index: isize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let chord = command_chord(command, cx);
        let is_recording = self
            .recording
            .as_ref()
            .is_some_and(|recording| recording.command.action_name == command.action_name);
        let keycap = keycap_element_from_chord(chord.as_deref(), is_recording);
        // The keycap is the column a person scans, so it is named for a test that can measure it
        // instead of trusting that the group happens to line up.
        let keycap = div()
            .flex_none()
            .debug_selector(|| "settings-keycap".to_owned())
            .child(keycap);
        // The keycap, Edit and Clear share one control column, so the column itself is named for
        // the test that measures the trailing edge. The keycap is its leading child and stops short
        // of that edge by design.
        let mut controls = setting_control(compact)
            .debug_selector(|| "settings-keyboard-controls".to_owned())
            .child(keycap);
        if command.editable {
            let edit_command = command.clone();
            controls = controls.child(
                Button::new(
                    format!("settings-key-edit-{tab_index}"),
                    if is_recording { "Recording…" } else { "Edit" },
                )
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Medium)
                .tab_index(tab_index)
                .disabled(is_recording)
                .aria_label(format!("Edit Shortcut for {}", command.label))
                .tooltip(Tooltip::text("Record a new shortcut for this command."))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.start_recording(edit_command.clone(), window, cx);
                })),
            );
        } else {
            controls = controls.child(
                Label::new("Use the keymap file.")
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted),
            );
        }
        if command.editable && chord.is_some() {
            let clear_command = command.clone();
            controls = controls.child(
                Button::new(format!("settings-key-clear-{tab_index}"), "Clear")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .tab_index(tab_index + 1)
                    .disabled(is_recording)
                    .aria_label(format!("Clear Shortcut for {}", command.label))
                    .tooltip(Tooltip::text("Remove this shortcut from the user keymap."))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.clear_binding(&clear_command, cx);
                    })),
            );
        }
        let description = if let Some(when) = command.when.as_deref() {
            let when = keyboard_context_description(when);
            format!("{} Active when: {when}.", command.description)
        } else {
            command.description.clone()
        };
        // A conflict belongs to the row that owns the key, so it reads next to that keycap rather
        // than in a toast about a different surface.
        let note = conflict_for_command(command, cx).map(|conflict| RowNote {
            id: format!("settings-key-conflict-{}", command.action_name),
            icon: IconName::Warning,
            text: conflict.label(),
            tone: Severity::Error,
        });
        let row = setting_row_with_note(command.label.clone(), description, note, compact, cx);
        let row = if is_recording {
            row.border_l_2()
                .border_color(cx.theme().colors().border_focused)
        } else {
            row
        };
        row.child(controls).into_any_element()
    }

    fn render_keyboard(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let mut content = v_flex().w_full();
        let mut tab_index = tab_order::KEYBOARD_FIRST;
        if self.keyboard_search_has_header_match() {
            content = content.child(section_header("Keyboard", KEYBOARD_DESCRIPTION, cx));
            content = content.child(self.render_keymap_file(compact, cx));
        }
        for section in self.keyboard_sections(cx).iter() {
            let visible_commands = section
                .commands
                .iter()
                .filter(|command| self.keyboard_search_matches_command(section, command))
                .cloned()
                .collect::<Vec<_>>();
            if visible_commands.is_empty() {
                continue;
            }
            content = content.child(section_header(
                &section.title,
                "Application commands available from the keyboard.",
                cx,
            ));
            for command in visible_commands {
                content =
                    content.child(self.render_keyboard_command(compact, &command, tab_index, cx));
                tab_index += 2;
            }
        }
        content.into_any_element()
    }

    /// Whether a pane still has something to show, so a search does not open an empty header.
    ///
    /// The rule is the count's rule: a pane survives when one of its own settings does, so the
    /// sidebar never offers a category whose rows the filter already removed.
    fn category_has_match(&self, category: SettingsCategory, cx: &App) -> bool {
        if self.search_query.trim().is_empty() {
            return true;
        }
        match category {
            SettingsCategory::Keyboard => self.keyboard_search_has_match(cx),
            category => self.matched_settings(category) > 0,
        }
    }

    fn render_category(
        &self,
        compact: bool,
        category: SettingsCategory,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.category_has_match(category, cx) {
            return None;
        }
        Some(match category {
            SettingsCategory::General => self.render_general(compact, cx),
            SettingsCategory::Keyboard => self.render_keyboard(compact, cx),
            SettingsCategory::Integrations => self.render_integrations(compact, cx),
            SettingsCategory::About => self.render_about(compact, cx),
        })
    }

    fn render_content(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let mut content = v_flex().w_full();
        let mut matched = false;
        for category in self.visible_categories(cx) {
            if let Some(section) = self.render_category(compact, category, cx) {
                matched = true;
                content = content.child(section);
            }
        }
        if !matched {
            content = content.child(
                h_flex()
                    .id("settings-no-matches")
                    .debug_selector(|| "settings-no-matches".to_owned())
                    .w_full()
                    .min_h(px(96.))
                    .px(space::LG)
                    .items_center()
                    .role(Role::Status)
                    .child(
                        Label::new(format!(
                            "No settings match “{}”. Try another search.",
                            self.search_query
                        ))
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::BODY,
                        )))),
                    ),
            );
        }
        content.into_any_element()
    }

    /// Whether a category row does anything at all right now.
    ///
    /// A search draws every pane that has a match, so picking a category while one is active
    /// changed nothing on screen: the content is every matching pane whichever one is named, and
    /// the only visible effects were a scroll position reset and three closed pop-ups. A row that
    /// does nothing visible communicates the opposite of what `buttons.md` asks of it, so the
    /// ones the filter emptied are disabled and leave the tab order with them.
    fn category_row_available(&self, category: SettingsCategory, cx: &App) -> bool {
        self.search_query.trim().is_empty() || self.category_has_match(category, cx)
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let highlighted = self.highlighted_category();
        let mut categories = v_flex().gap(space::XS);
        for (index, category) in SettingsCategory::ALL.into_iter().enumerate() {
            let selected = highlighted == Some(category);
            let available = self.category_row_available(category, cx);
            categories = categories.child(category_row(
                selected,
                div().child(
                    Button::new(
                        format!("settings-category-button-{index}"),
                        category.label(),
                    )
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .full_width()
                    .tab_index(tab_order::SIDEBAR_FIRST + index as isize)
                    .start_icon(Icon::new(category.icon()).size(IconSize::XSmall))
                    .toggle_state(selected)
                    // `DESIGN.md` §5 gives a selected button the primary style, and
                    // `ButtonStyle::Tinted(Accent)` is what this app calls primary. Without it
                    // the selected row falls through to Zed's uninspected default, which is not
                    // the accent this panel's selected rows use everywhere else.
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .disabled(!available)
                    .aria_label(format!("Open {} Settings", category.label()))
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.select_category(category, cx)),
                    ),
                ),
                format!("settings-category-{}", category.label()),
                cx,
            ));
        }
        v_flex()
            .w(px(SETTINGS_SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .min_h(px(0.))
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(design::surface::panel(cx).alpha(1.0))
            .child(
                v_flex().px(space::MD).pt(space::MD).gap(space::SM).child(
                    div()
                        .w_full()
                        .track_focus(&self.search_focus)
                        .debug_selector(|| "settings-search".to_owned())
                        .child(self.search_input.clone()),
                ),
            )
            .child(
                v_flex()
                    .px(space::SM)
                    .pt(space::SM)
                    .gap(space::XS)
                    .child(categories),
            )
            .into_any_element()
    }

    /// The collapsed sidebar: an icon rail that keeps every category reachable.
    ///
    /// `DESIGN.md` §6 requires a persistent switch and a way back for every panel, so the rail is
    /// never the only navigation: the toolbar carries the toggle and, while collapsed, the search
    /// field that the rail has no room for.
    fn render_sidebar_rail(&self, cx: &Context<Self>) -> AnyElement {
        let highlighted = self.highlighted_category();
        let mut categories = v_flex().gap(space::XS).items_center();
        for (index, category) in SettingsCategory::ALL.into_iter().enumerate() {
            let selected = highlighted == Some(category);
            // The rail is the collapsed form of the same list, so it answers the same question:
            // a search emptied this category, and the icon does nothing without a match either.
            let available = self.category_row_available(category, cx);
            categories = categories.child(category_row(
                selected,
                div().child(
                    IconButton::new(format!("settings-category-button-{index}"), category.icon())
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .icon_size(IconSize::XSmall)
                        .tab_index(tab_order::SIDEBAR_FIRST + index as isize)
                        .toggle_state(selected)
                        // The collapsed rail is the same list in a narrower frame, so the
                        // selected row takes the same style. See the expanded list above.
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(!available)
                        .aria_label(format!("Open {} Settings", category.label()))
                        .tooltip(Tooltip::text(category.label()))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.select_category(category, cx)),
                        ),
                ),
                format!("settings-category-{}", category.label()),
                cx,
            ));
        }
        v_flex()
            .w(px(SETTINGS_SIDEBAR_RAIL_WIDTH))
            .h_full()
            .flex_none()
            .min_h(px(0.))
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(design::surface::panel(cx).alpha(1.0))
            .child(
                v_flex()
                    .w_full()
                    .px(space::XS)
                    .pt(space::MD)
                    .gap(space::XS)
                    .items_center()
                    .child(categories),
            )
            .into_any_element()
    }

    /// The count the toolbar states while a query is filtering the panes.
    ///
    /// `DESIGN.md` §3.1 does not let a title restate the window title, the tab title and the
    /// content title at once, and all three already name the pane, so the toolbar prints no name
    /// of its own. What is left for it to say is how many rows the query kept: a filtered list and
    /// an empty one look the same without a number, and `metadata` is the role §3.1 gives counts.
    fn search_result_label(&self, cx: &App) -> String {
        format!("Search results · {}", self.search_result_count(cx))
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let searching = !self.search_query.trim().is_empty();
        let icon = if searching {
            IconName::MagnifyingGlass
        } else {
            self.category.icon()
        };
        let mut toolbar = h_flex()
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .min_h(design::size::TOOLBAR)
            .px(space::LG)
            .gap(space::SM)
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted));
        if self.sidebar_collapsed {
            // The rail has no room for the field, so the toolbar keeps it reachable.
            toolbar = toolbar.child(
                div()
                    .flex_none()
                    .track_focus(&self.search_focus)
                    .debug_selector(|| "settings-search".to_owned())
                    .child(self.search_input.clone()),
            );
        }
        toolbar
            .child(div().flex_1())
            .when(searching, |this| {
                this.child(
                    div()
                        .flex_none()
                        .debug_selector(|| "settings-search-results".to_owned())
                        .child(
                            Label::new(self.search_result_label(cx))
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "settings-sidebar-toggle".to_owned())
                    .child(
                        IconButton::new(
                            "settings-sidebar-toggle",
                            if self.sidebar_collapsed {
                                IconName::ThreadsSidebarLeftOpen
                            } else {
                                IconName::ThreadsSidebarLeftClosed
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .icon_size(IconSize::XSmall)
                        .tab_index(tab_order::SIDEBAR_TOGGLE)
                        .aria_label(if self.sidebar_collapsed {
                            "Show the Settings Categories"
                        } else {
                            "Hide the Settings Categories"
                        })
                        .tooltip(Tooltip::text(if self.sidebar_collapsed {
                            "Show the settings categories."
                        } else {
                            "Hide the settings categories."
                        }))
                        .on_click(cx.listener(|view, _, _, cx| view.toggle_sidebar(cx))),
                    ),
            )
            .into_any_element()
    }
}

/// A keycap, the recording hint, or the honest "no shortcut" line.
///
/// An unbound action says so in words rather than in colour alone, and a key the recorder cannot
/// take is never drawn as if it were bound.
fn keycap_element_from_chord(chord: Option<&str>, recording: bool) -> AnyElement {
    if recording {
        Label::new(recording_hint())
            .size(LabelSize::Custom(rems_from_px(f32::from(
                design::text::METADATA,
            ))))
            .color(Color::Muted)
            .into_any_element()
    } else if let Some(keycap) = chord.and_then(keycap_from_chord) {
        keycap
    } else {
        Label::new("No shortcut assigned.")
            .size(LabelSize::Custom(rems_from_px(f32::from(
                design::text::METADATA,
            ))))
            .color(Color::Muted)
            .into_any_element()
    }
}

/// Turns a key chord such as `ctrl-shift-k` into the keycap element, or nothing when the chord
/// cannot be parsed, so a malformed keymap file cannot put a raw string in a keycap.
fn keycap_from_chord(chord: &str) -> Option<AnyElement> {
    let keystroke = gpui::Keystroke::parse(chord).ok()?;
    let key = gpui::KeybindingKeystroke::from_keystroke(keystroke);
    Some(UiKeyBinding::from_keystrokes(vec![key].into(), false).into_any_element())
}

/// The bound actions the Keyboard list does not name because the recorder would refuse their key.
///
/// A row advertises a shortcut the settings page can write, so a binding on a key the recorder
/// rejects has nothing to offer: naming it would put a keycap in the list that the Edit control
/// cannot record and Clear cannot remove.
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
    (
        "k8s_table::OpenDetails",
        "enter and space open the row, and the recorder refuses both",
    ),
    (
        "k8s_ops::DeleteSelection",
        "delete is a text key, so a shortcut on it is unreachable",
    ),
];

/// The bound actions that have no row and a key the recorder accepts.
///
/// The Keyboard list is built from the command palette, so a binding the palette does not mention
/// has no row: the shortcut exists and nobody can find it or change it. Each entry needs a
/// `shell::commands` entry before the list can name it, and the test below fails the moment a new
/// binding lands here unlisted.
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

/// The chord a keyboard row shows, or `None` when the action has no binding in its context.
///
/// `keymap::binding_for_context` is the shared lookup: it honours the context predicate and skips a
/// binding the surface released with an `unbind` row, so a row never advertises a key that will not
/// fire. It reads a focus path rather than a context expression, so the row's own section goes
/// through [`section_focus_path`] first. A parameterized action has one row per input and keeps the
/// instance lookup, because the shared helper matches by action name and would answer with another
/// input's chord.
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

/// The focus path a keymap section answers for, so the shared lookup gets the path it reads.
///
/// A section is a predicate over focus paths (`Shell && !CommandPalette`), and
/// `keymap::binding_for_context` evaluates a predicate against the path a hint sits on
/// (`Shell Table`). Handing it the section names the negated surfaces as if they were focused, so
/// the predicate refuses the very section that names it and every row loses its keycap. The path is
/// the section's own surfaces: the negations stay in the predicate, which is what checks them.
fn section_focus_path(context: &str) -> String {
    let mut path: Vec<&str> = Vec::new();
    let mut negated = false;
    for term in context.split_whitespace() {
        if term == "&&" || term == "||" {
            // A term boundary ends the negation, so `!Terminal && Shell` still keeps the Shell.
            negated = false;
        } else if term.starts_with('!') {
            // A negated name is a condition on the path, not a surface that can hold focus.
            negated = true;
        } else if !negated {
            path.push(term);
        }
    }
    path.join(" ")
}

/// The context of the action's last binding.
///
/// A command whose canonical context is unknown is bound in exactly one place, so its own binding
/// names the surface the shortcut belongs to.
fn binding_context_for_action(action_name: &str, cx: &App) -> Option<String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings()
        .filter(|binding| binding.action().name() == action_name)
        .find_map(|binding| binding.predicate().map(|predicate| predicate.to_string()))
}

/// What the armed Restore Defaults step deletes, in the words of the confirmation.
///
/// A confirmation that only says "restore the defaults" does not say what is lost. The step deletes
/// the user `keymap.json`, so the sentence names that file and the overrides inside it.
fn restore_confirmation_text(target: &str) -> String {
    format!(
        "{RESTORE_DEFAULTS_LABEL} deletes {target}. Your shortcut overrides in that file are lost, and the built-in shortcuts come back."
    )
}

/// The conflict this row owns, if the keymap reports one.
///
/// `feedback.md` › Best practices asks status to sit next to the thing it describes, so a key that
/// two actions claim is reported on the row that shows that key, not only in a toast about a
/// surface the person may not be on.
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

/// A conflict entry is `name` or `name {json}`. Comparing the parsed input keeps whitespace in the
/// keymap file from hiding a conflict on a parameterized row.
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
    // A modifier is what makes a key a shortcut instead of a character, so a modified keystroke
    // always qualifies: `ctrl-f1` spends no text, and the only F1 the app owns is the bare one
    // that `KEYMAP.md` §4.1 gives to the diagnostics frame overlay.
    if keystroke.modifiers.modified() {
        return true;
    }
    // A bare function key types nothing, so F2 through F24 are the one unmodified key that can
    // carry a shortcut on their own.
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

/// The context an action is looked up in, or `None` when the keymap binds it globally.
///
/// A row shows the chord its own surface can reach, and its Edit and Clear controls write into the
/// same section, so an action that the keymap binds inside a context needs an entry here. A key
/// without one falls back to the context of the action's own last binding, which is only the right
/// answer while the action is bound in exactly one place.
fn canonical_context(action_name: &str) -> Option<&'static str> {
    match action_name {
        "k8s_app::OpenSettings"
        | "k8s_shell::SearchResources"
        | "k8s_shell::ToggleNotifications"
        | "k8s_shell::ReloadKubeconfigs"
        | "k8s_shell::ToggleLeftPanel"
        | "k8s_shell::ToggleRightPanel"
        | "k8s_shell::ToggleDock" => Some("!CommandPalette"),
        // The three switchers give way to a focused session, so they carry the extra condition the
        // keymap uses.
        "k8s_shell::OpenContextSwitcher"
        | "k8s_shell::OpenNamespaceSwitcher"
        | "k8s_shell::OpenResourceKindSwitcher" => Some("Shell && !CommandPalette && !Terminal"),
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
        // The commands on the selected row are keyed in the shell block next to Exec and Apply,
        // so a row that looked them up in the table would advertise no key and would write an
        // override into a section the built-in binding does not use.
        | "k8s_shell::DescribeSelection"
        | "k8s_shell::PauseUpdates"
        | "k8s_shell::ResumeUpdates"
        | "k8s_shell::CopySelectedPodName"
        | "k8s_hotbar::ToggleHotbar" => Some("Shell && !CommandPalette"),
        // These two have no built-in key, so the table is where the commands that replaced them
        // fire and where a user override belongs: the F5 refresh and the service account shortcut
        // act on the row a table has focused.
        "k8s_shell::RefreshView" | "k8s_shell::OpenServiceAccount" => {
            Some("Table && !CommandPalette")
        }
        // Every Inspector key belongs to the panel: it is the only surface that can act on it.
        "k8s_inspector::ReloadActiveTab"
        | "k8s_inspector::RetryMetrics"
        | "k8s_inspector::MetricsRange5m"
        | "k8s_inspector::MetricsRange15m"
        | "k8s_inspector::MetricsRange1h"
        | "k8s_inspector::ConfirmApply"
        | "k8s_inspector::CancelApplyReview"
        | "k8s_inspector::RevertYaml"
        | "k8s_inspector::CopyYaml"
        | "k8s_inspector::ToggleValueExpansion"
        | "k8s_inspector::CopyValue"
        | "k8s_inspector::NextProblem" => Some(INSPECTOR_CONTEXT),
        "k8s_hotbar::SwitchCluster" | "k8s_hotbar::SwitchBank" => Some("Hotbar"),
        _ => None,
    }
}

fn binding_matches_context(binding: &gpui::KeyBinding, context: Option<&str>) -> bool {
    match context {
        None => binding.predicate().is_none(),
        Some(context) => keymap::binding_context(binding).as_deref() == Some(context),
    }
}

fn current_binding_for_action(
    action: &dyn Action,
    context: Option<&str>,
    cx: &App,
) -> Option<gpui::KeyBinding> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings_for_action(action)
        .rfind(|binding| binding_matches_context(binding, context))
        .cloned()
}

fn command_context(action_name: &str, binding: Option<&gpui::KeyBinding>) -> Option<String> {
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
                "The {} action is unavailable. Update k8s-gpui or use another command.",
                command.label
            )
        })
}

fn keyboard_command_label(action_name: &str, label: &str) -> String {
    match action_name {
        "k8s_shell::PortForwardSelection" => "Start Port Forward".to_owned(),
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
        "Hotbar" => "the Hotbar",
        "!CommandPalette" => "outside the Command Palette",
        _ => context,
    }
}

/// The one sentence a keyboard row shows under its label.
///
/// `writing.md` asks a label to describe what it does, and a row is a person deciding whether to
/// rebind a key, so the sentence has to be about this command rather than about the verb "run".
/// The table used to end in `_ => format!("Run {label}.")`, and 33 of 68 rows reached it, so
/// `Scale Selection` described a fail-closed command as something it does. `Command` in
/// `shell/commands.rs` has no `description` field, so there is no registry sentence to reuse and
/// the table below is the whole vocabulary; `every_listed_action_says_what_it_does` fails when a
/// new action lands in the list without one.
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
        // UI Zoom is fail-closed by contract, so the one row that documents it has to say that
        // rather than describe an action. `DESIGN.md` §3.1 requires triggering it to change no
        // font size, spacing, control or window size, only to show why it cannot run.
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
        "k8s_table::SortSelectedColumn" => "Sort the selected column.".to_owned(),
        // The standard editing commands. They are palette rows so a person can find out what
        // Ctrl-C does here, and each one says which selection it acts on.
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
        "k8s_hotbar::ToggleHotbar" => "Show or hide the Hotbar rail.".to_owned(),
        "k8s_shell::ReloadKubeconfigs" => "Reload the configured kubeconfig files.".to_owned(),
        "k8s_shell::ReloadKeymap" => {
            "Reload the user keymap and apply the saved shortcuts.".to_owned()
        }
        "k8s_shell::UseKeymapPreset" => "Replace the user keymap with a named preset.".to_owned(),
        // Theme. The three actions live in the `k8s_shell` namespace, so a table keyed on
        // `k8s_app::Use…` matched nothing and both rows fell through to the generic sentence.
        "k8s_shell::ToggleTheme" => "Switch between the light and dark themes.".to_owned(),
        "k8s_shell::UseLightTheme" => "Switch the workbench to the light theme.".to_owned(),
        "k8s_shell::UseDarkTheme" => "Switch the workbench to the dark theme.".to_owned(),
        "k8s_shell::UseSystemTheme" => "Follow the desktop's light or dark setting.".to_owned(),
        // Application.
        "k8s_app::OpenSettings" => "Open k8s-gpui Settings.".to_owned(),
        "k8s_app::CheckForUpdates" => "Check for a newer k8s-gpui release.".to_owned(),
        "k8s_app::RestartToUpdate" => "Restart k8s-gpui after an update is ready.".to_owned(),
        "k8s_inspector::ReloadActiveTab" => {
            "Reload the data of the active tab in the Inspector.".to_owned()
        }
        "k8s_inspector::RetryMetrics" => "Retry the failed metrics request.".to_owned(),
        "k8s_inspector::MetricsRange5m" => "Show the last 5 minutes of metrics.".to_owned(),
        "k8s_inspector::MetricsRange15m" => "Show the last 15 minutes of metrics.".to_owned(),
        "k8s_inspector::MetricsRange1h" => "Show the last hour of metrics.".to_owned(),
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

/// The theme names in two labelled groups: the product themes first, then the imported ones.
///
/// A single column of fourteen names with one rule reads as a flat list to scan. Two groups let a
/// person reach the K8s Studio theme without reading every Zed theme name, and an empty group is
/// dropped so a build with only one family does not get a heading with nothing under it.
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
            action_name: "k8s_hotbar::SwitchCluster",
            label: "Switch to Context",
            group: "Hotbar",
            description: "Switch to a saved context in the active bank.",
            value_field: "slot",
            value_offset: 1,
        },
        ParameterizedKeyboardAction {
            action_name: "k8s_hotbar::SwitchBank",
            label: "Switch to Bank",
            group: "Hotbar",
            description: "Switch to a saved hotbar bank.",
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
            // The row has to say where the shortcut is live. `context` can be empty for a
            // globally bound action, and those rows correctly say nothing; the other 68 name a
            // context here, and a list that named it only on a conflict read as if the rest of
            // the list had no scope at all.
            when: canonical.map(str::to_owned),
        };
        append_keyboard_command(&mut sections, command.group.to_string(), keyboard_command);
    }
    for (action_name, label, group) in [
        (
            "k8s_shell::SearchResources",
            "Search Cluster Resources",
            "View",
        ),
        (
            "k8s_shell::ToggleCommandPalette",
            "Toggle Command Palette",
            "Application",
        ),
        ("k8s_app::OpenSettings", "Open Settings", "Application"),
        // The Inspector toolbar owns these, and its keys are bound in the Inspector context, so
        // the list names them here: a bound action that no row mentions is a shortcut a person
        // cannot find or change.
        (
            "k8s_inspector::ReloadActiveTab",
            "Reload Active Tab",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::RetryMetrics",
            "Retry Metrics",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::MetricsRange5m",
            "Metrics: Last 5 Minutes",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::MetricsRange15m",
            "Metrics: Last 15 Minutes",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::MetricsRange1h",
            "Metrics: Last Hour",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::ConfirmApply",
            "Confirm and Apply Changes",
            INSPECTOR_GROUP,
        ),
        (
            "k8s_inspector::CancelApplyReview",
            "Cancel the Apply Review",
            INSPECTOR_GROUP,
        ),
        ("k8s_inspector::RevertYaml", "Revert YAML", INSPECTOR_GROUP),
        ("k8s_inspector::CopyYaml", "Copy YAML", INSPECTOR_GROUP),
        (
            "k8s_inspector::ToggleValueExpansion",
            "Expand or Collapse Value",
            INSPECTOR_GROUP,
        ),
        ("k8s_inspector::CopyValue", "Copy Value", INSPECTOR_GROUP),
        (
            "k8s_inspector::NextProblem",
            "Next YAML Problem",
            INSPECTOR_GROUP,
        ),
    ] {
        if sections
            .iter()
            .flat_map(|section| section.commands.iter())
            .any(|command| command.action_name == action_name)
            || !cx.all_action_names().contains(&action_name)
        {
            continue;
        }
        let Ok(action) = cx.build_action(action_name, None) else {
            continue;
        };
        let canonical = canonical_context(action_name);
        let current = current_binding_for_action(action.as_ref(), canonical, cx);
        let action_input = current
            .as_ref()
            .and_then(|binding| binding.action_input())
            .map(|input| input.to_string());
        let context = command_context(action_name, current.as_ref());
        let keyboard_command = KeyboardCommand {
            action_name: action.name().to_owned(),
            label: label.to_owned(),
            description: keyboard_description(action.name(), label),
            editable: action_input.is_none(),
            action_input,
            context,
            when: canonical.map(str::to_owned),
        };
        append_keyboard_command(&mut sections, group, keyboard_command);
    }
    append_parameterized_keyboard_commands(&mut sections, cx);
    sections
}

fn section_header(title: &str, description: &str, cx: &Context<SettingsView>) -> AnyElement {
    let heading = format!("settings-heading-{title}");
    h_flex()
        .id(format!("settings-section-{title}"))
        .w_full()
        .min_h(design::size::ROW)
        .px(space::LG)
        .pt(space::LG)
        .pb(space::SM)
        .gap(space::MD)
        .items_center()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            v_flex()
                .id(heading.clone())
                // The header is the only place the pane is named, so it is the element a test
                // looks for: `debug_bounds` reads selectors, never element ids.
                .debug_selector(move || heading)
                .flex_1()
                .min_w(px(0.))
                .gap(space::XS)
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(title.to_owned())
                .child(
                    Label::new(title).size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::SECTION,
                    )))),
                )
                .child(
                    Label::new(description)
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
                ),
        )
        .into_any_element()
}

/// The fill a category row paints.
///
/// A test reads this instead of a screenshot, so the contract the selection has to keep is a value
/// and not a picture.
fn category_row_background(selected: bool, cx: &App) -> Hsla {
    if selected {
        design::row_selected_bg(cx)
    } else {
        design::surface::panel(cx).alpha(1.0)
    }
}

/// The width of the position cue a selected category row carries.
fn category_row_rail_width() -> f32 {
    f32::from(design::border::FOCUS_RAIL)
}

/// The colour of that cue.
///
/// The rail is a graphic, so it is solved against the fill it sits on rather than against the
/// surface the row was measured from: the selected fill is an accent wash over the sidebar, and an
/// accent rail on top of it can lose the interactive contrast the rule asks for.
fn category_row_rail_color(cx: &App) -> Hsla {
    design::graphic_on(
        category_row_background(true, cx),
        cx.theme().colors().text_accent,
    )
}

/// A category row: the selection is a surface change on the row, not a slab on the control.
///
/// `focus-and-selection.md > Best practices` asks for a highlight in a list and a ring only on a
/// text or search field, so the selected row takes the shared `design::row_selected_bg` and a
/// position rail. The control inside it stays subtle, or the search field's focus ring would be
/// the quietest thing on the panel while a saturated accent block shouted next to it.
///
/// The rail is painted after the control so a pointer over the selected row cannot cover the one
/// mark that says which row is open.
fn category_row(
    selected: bool,
    control: gpui::Div,
    selector: String,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(selector.clone())
        .debug_selector(move || selector)
        .w_full()
        .relative()
        .rounded_sm()
        .hover(|style| style.bg(design::row_hover_bg(cx)))
        .when(selected, |style| {
            style.bg(category_row_background(selected, cx))
        })
        .child(control)
        .when(selected, |style| {
            style.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(category_row_rail_width()))
                    .bg(category_row_rail_color(cx)),
            )
        })
}

/// A second line inside a row's label column, for a status that belongs to that setting.
///
/// `feedback.md` › Best practices asks status to sit next to the thing it describes, and the fixed
/// value column has no room for a sentence.
struct RowNote {
    id: String,
    icon: IconName,
    text: String,
    tone: Severity,
}

fn setting_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    compact: bool,
    cx: &Context<SettingsView>,
) -> gpui::Div {
    setting_row_with_note(title, description, None, compact, cx)
}

fn setting_row_with_note(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    note: Option<RowNote>,
    compact: bool,
    cx: &Context<SettingsView>,
) -> gpui::Div {
    let label =
        setting_row_label(title, description, compact, cx).when_some(note, |label, note| {
            // A warning is carried by its icon and its position, and its sentence keeps the body
            // text colour: a warning hue at 11px would not clear the body contrast threshold, and
            // a risk note muted to grey is a risk note nobody reads.
            let (icon_color, text_color) = match note.tone {
                Severity::Error => (Color::Error, Color::Error),
                Severity::Warning => (Color::Custom(note.tone.marker(cx)), Color::Default),
                _ => (Color::Muted, Color::Muted),
            };
            let id = note.id.clone();
            label.child(
                h_flex()
                    .id(note.id)
                    .debug_selector(move || id.clone())
                    .gap(space::XS)
                    .items_start()
                    .role(Role::Status)
                    .aria_label(note.text.clone())
                    .child(
                        Icon::new(note.icon)
                            .size(IconSize::XSmall)
                            .color(icon_color),
                    )
                    .child(
                        Label::new(note.text)
                            .size(LabelSize::Custom(rems_from_px(f32::from(
                                design::text::METADATA,
                            ))))
                            .color(text_color),
                    ),
            )
        });
    let row = if compact {
        v_flex().w_full().gap(space::SM).items_start().child(label)
    } else {
        h_flex()
            .items_center()
            // The label keeps its fixed measure so no description re-wraps, the value column keeps
            // its fixed width so the widest control still has room, and the spacer between them
            // takes what is left. The value column therefore ends on the measure's trailing edge
            // instead of floating a third of the way across a wide window. `layout.md` › Visual
            // hierarchy asks for the alignment that makes a form scannable and for the trailing side
            // to be part of reading order; one right-hand column of controls is both.
            .child(label.w(px(SETTINGS_LABEL_WIDTH)).flex_none())
            .child(div().flex_1().min_w(px(f32::from(space::XL))))
    };
    row.w_full()
        // The row rhythm is the shared `row` token; the two lines of text make a row taller than
        // the floor, which is the intended exception in `DESIGN.md` §3.3.
        .min_h(design::size::ROW)
        .font_ui(cx)
        .px(space::LG)
        .py(space::SM)
}

/// The title, description, and optional note stack of one settings row.
///
/// It is flexible so the keymap block, which owns a path and four controls instead of one, can use
/// the same stack at the pane's full width. The two-column rows narrow it to the shared label
/// column instead.
fn setting_row_label(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    compact: bool,
    _cx: &Context<SettingsView>,
) -> gpui::Div {
    let title = title.into();
    let description = description.into();
    let label = v_flex()
        .flex_1()
        .min_w(px(0.))
        .gap(space::XS)
        .child(
            Label::new(title).size(LabelSize::Custom(rems_from_px(f32::from(
                design::text::BODY,
            )))),
        )
        .child(
            Label::new(description)
                .size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .color(Color::Muted),
        );
    if compact { label.w_full() } else { label }
}

/// The value column of a settings row.
///
/// The column holds the measure's trailing edge: the row gives it a fixed width and the leftover
/// space to a spacer, so every control ends on the same line, the way a macOS settings pane reads.
/// Its children are sized by their content, so a pop-up button and a keymap row both sit flush
/// against that line instead of leaving a ragged gap before it.
fn setting_control(compact: bool) -> gpui::Div {
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

/// The box a boolean setting is answered with.
///
/// A pill switch is the iOS control for a touch surface, and a Linux desktop sets its booleans with
/// a check box in a fixed control column, the same answer `Theme` and `Reduce motion` already give
/// with a pop-up button. `ui::Checkbox` is the shared component, but in this revision it carries
/// no tab index and no ARIA state, so a check box built from it would be a control a keyboard
/// cannot reach and a screen reader cannot read. Drawing it here keeps the keyboard, the accessible
/// name, and the off state under this project's control.
///
/// The empty state is a filled box inside a visible 1px border solved to the graphic threshold: the
/// pill's off state was a dark track with a half-opacity dot, which is the "almost invisible" the
/// review recorded. The target keeps its border in both states and only changes its colour on
/// focus, because a border that appears on focus is taken out of the content box and moves what is
/// inside it.
fn checkbox_control(
    id: &'static str,
    checked: bool,
    tab_index: isize,
    label: &'static str,
    description: &'static str,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors();
    let surface = design::surface::canvas(cx);
    let mut box_element = div()
        .flex_none()
        .size(CHECKBOX_BOX)
        .flex()
        .items_center()
        .justify_center()
        .rounded_xs()
        .border_1()
        .border_color(design::graphic_on(surface, colors.border))
        .bg(if checked {
            colors.element_background
        } else {
            colors.ghost_element_background
        });
    if checked {
        box_element = box_element.child(
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Accent),
        );
    }
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex_none()
        .size(design::size::CONTROL)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded_sm()
        .border_1()
        .border_color(colors.border_transparent)
        .focus_visible(|style| style.border_color(design::focus::border(cx)))
        .tab_index(tab_index)
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_description(description)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .child(box_element)
}

/// The status mark one capability state draws.
///
/// `DESIGN.md` §4 makes `design::health_icon` the app's only shape vocabulary, and the filled
/// check already means "this one is fine" in the status bar. A private dot gave `Available` and
/// `Error` one shape and left the two verdicts to the hue alone, so "a filled point" meant both.
/// Four states, four shapes, and the probe still running is the hollow ring the contract reserves
/// for `info / syncing`.
fn capability_glyph(capability: Capability, cx: &App) -> AnyElement {
    let severity = capability.severity();
    let icon = design::health_icon(severity);
    let color = Color::Custom(severity.marker(cx));
    if capability == Capability::Checking {
        return spinner(icon, color, IconSize::Indicator, cx);
    }
    Icon::new(icon)
        .size(IconSize::Indicator)
        .color(color)
        .into_any_element()
}

/// True when the two-column row cannot hold its label and its fixed value column.
///
/// The shell never opens a window narrower than `SETTINGS_MIN_WINDOW_WIDTH`, so this is the
/// graceful-degradation path rather than a layout a person sees at 672px. The row floor keeps the
/// two conditions honest: whichever is larger wins.
fn compact_settings_layout(viewport_width: f32) -> bool {
    let row_floor = SETTINGS_LABEL_WIDTH
        + SETTINGS_CONTROL_WIDTH
        + f32::from(space::LG) * 2.
        + f32::from(space::XL);
    viewport_width < row_floor.max(SETTINGS_MIN_WINDOW_WIDTH)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = compact_settings_layout(f32::from(window.viewport_size().width));
        let colors = cx.theme().colors();
        v_flex()
            .size_full()
            .min_w(px(0.))
            .bg(design::surface::canvas(cx).alpha(1.0))
            .text_color(colors.text)
            .font_ui(cx)
            .key_context("Settings")
            .tab_group()
            .track_focus(&self.recording_focus)
            .on_key_down(cx.listener(Self::on_settings_key_down))
            .child(self.render_toolbar(cx))
            .when_some(self.render_recording_status(cx), |this, status| {
                this.child(status)
            })
            .when_some(self.render_status_strip(cx), |this, strip| {
                this.child(strip)
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .items_stretch()
                    .child(if self.sidebar_collapsed {
                        self.render_sidebar_rail(cx)
                    } else {
                        self.render_sidebar(cx)
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w(px(0.))
                            .min_h(px(0.))
                            .key_context("SettingsContent")
                            .child(
                                div()
                                    .id("settings-scroll")
                                    .flex_1()
                                    .min_h(px(0.))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .child(
                                        div()
                                            .id("settings-content")
                                            .debug_selector(|| "settings-content".to_owned())
                                            .w_full()
                                            .max_w(px(SETTINGS_CONTENT_MAX_WIDTH))
                                            .child(self.render_content(compact, cx)),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use gpui::TestAppContext;
    use theme::{LoadThemes, ThemeRegistry};

    #[gpui::test]
    fn default_focus_targets_search_input(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let search = view.read_with(cx, |view, _| view.search_focus.clone());
        assert!(search.tab_stop);
        assert_eq!(search, view.read_with(cx, |view, _| view.focus_handle()));
        cx.update(|window, cx| window.focus(&search, cx));
        assert!(cx.update(|window, _| search.is_focused(window)));
    }

    #[gpui::test]
    fn saved_but_not_applied_keymap_result_shows_error_not_success(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
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
            view.read_with(cx, |view, _| view.error.as_deref().map(str::to_owned)),
            Some("Saved but inactive".to_owned())
        );
    }

    #[gpui::test]
    fn update_actions_after_state_preserve_ready_and_downloading(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
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

    #[gpui::test]
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

        assert_eq!(
            THEME_DESCRIPTION,
            "Choose a K8s Studio or Zed theme. The System theme follows the desktop setting."
        );
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::All(Box::new(assets::Assets)), cx);
            theme_settings::load_user_theme(
                &ThemeRegistry::global(cx),
                include_bytes!("../../../k8s-app/assets/themes/k8s-studio.json"),
            )
            .unwrap();
        });
        let expected = THEME_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        cx.update(|cx| {
            let names = ThemeRegistry::global(cx)
                .list_names()
                .into_iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>();
            assert_eq!(names, expected);
        });

        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let handle = view.read_with(cx, |view, _| view.theme_menu.clone());
        assert!(!handle.is_deployed());
        cx.update(|window, cx| handle.show(window, cx));
        cx.run_until_parked();
        cx.run_until_parked();
        assert!(handle.is_deployed());
        cx.update(|_, cx| handle.hide(cx));
        cx.run_until_parked();
        assert!(!handle.is_deployed());
        cx.update(|window, cx| handle.show(window, cx));
        cx.run_until_parked();
        cx.run_until_parked();

        for selector in [
            "MENU_ITEM-System",
            "MENU_ITEM-Ayu Dark",
            "MENU_ITEM-Ayu Light",
            "MENU_ITEM-Ayu Mirage",
            "MENU_ITEM-Gruvbox Dark",
            "MENU_ITEM-Gruvbox Dark Hard",
            "MENU_ITEM-Gruvbox Dark Soft",
            "MENU_ITEM-Gruvbox Light",
            "MENU_ITEM-Gruvbox Light Hard",
            "MENU_ITEM-Gruvbox Light Soft",
            "MENU_ITEM-K8s Studio Dark",
            "MENU_ITEM-K8s Studio Light",
            "MENU_ITEM-One Dark",
            "MENU_ITEM-One Light",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing theme menu item: {selector}"
            );
        }
        // The menu groups the product themes before the imported ones, so a person does not read
        // thirteen names to reach K8s Studio.
        let groups = theme_groups(expected.clone());
        assert_eq!(
            groups.iter().map(|(title, _)| *title).collect::<Vec<_>>(),
            vec!["K8s Studio", "Zed"]
        );
        assert_eq!(groups[0].1, vec!["K8s Studio Dark", "K8s Studio Light"]);
        assert_eq!(groups[1].1.len(), 11);
        assert!(
            !groups[1]
                .1
                .iter()
                .any(|name| name.starts_with("K8s Studio"))
        );
    }

    #[test]
    fn capability_labels_are_user_facing() {
        assert_eq!(Capability::Checking.label(), "Checking…");
        assert_eq!(Capability::Available.label(), "Available");
        assert_eq!(Capability::Unavailable.label(), "Not found");
        assert_eq!(Capability::Error.label(), "Error");
        assert_eq!(Capability::Available.severity(), Severity::Success);
        assert_eq!(Capability::Error.severity(), Severity::Error);
        // A probe still running is `info / syncing`, and a tool that is not installed has no
        // verdict. Both used to be `Muted`, so the dash said the same thing twice.
        assert_eq!(Capability::Checking.severity(), Severity::Info);
        assert_eq!(Capability::Unavailable.severity(), Severity::Muted);
    }

    /// The four states are four shapes, from the one shape vocabulary the app has.
    ///
    /// A private 8px dot drew `Available` and `Error` with the same mark and left the two verdicts
    /// to the hue, while the same filled point meant "healthy" in the status bar. `color.md ›
    /// Inclusive color` asks the shape not to carry two meanings, so the test reads the shared
    /// table rather than a private one.
    #[test]
    fn capability_states_use_the_health_shape_vocabulary() {
        let icon = |capability: Capability| design::health_icon(capability.severity());
        assert_eq!(icon(Capability::Available), IconName::Check);
        assert_eq!(
            icon(Capability::Error),
            design::health_icon(Severity::Error)
        );
        assert_eq!(
            icon(Capability::Unavailable),
            design::health_icon(Severity::Muted)
        );
        assert_eq!(
            icon(Capability::Checking),
            design::health_icon(Severity::Info)
        );
        let shapes = [
            icon(Capability::Checking),
            icon(Capability::Available),
            icon(Capability::Unavailable),
            icon(Capability::Error),
        ];
        for (index, shape) in shapes.iter().enumerate() {
            assert!(
                !shapes[index + 1..].contains(shape),
                "{shape:?} is drawn for two capability states: {shapes:?}"
            );
        }
    }

    #[test]
    fn theme_choice_labels_are_stable() {
        assert_eq!(ThemeChoice::System.label(), "System");
        assert_eq!(ThemeChoice::Light.label(), "Light");
        assert_eq!(ThemeChoice::Dark.label(), "Dark");
    }

    #[test]
    fn settings_layout_stacks_at_compact_widths() {
        // The shell never opens a window narrower than the minimum, so the stacked layout is the
        // graceful-degradation path and starts exactly at that width.
        assert!(!compact_settings_layout(960.));
        assert!(!compact_settings_layout(1440.));
        assert!(compact_settings_layout(640.));
    }

    /// The tab indices the rendered surface registered, in the order Tab walks them.
    ///
    /// `focus_next` visits the tab stops the last frame's paint registered, ordered by the index
    /// each control carries, so every number a test below looks at is read back off a real control
    /// rather than compared with another constant. A walk continues from whatever already has
    /// focus, so the stop it starts on is read first and the walk ends when focus returns to it.
    fn rendered_tab_indices(cx: &mut gpui::VisualTestContext) -> Vec<isize> {
        // The Keyboard pane holds the longest list. This only keeps a frame that never moves focus
        // from turning the walk into a hang.
        const WALK_LIMIT: usize = 512;
        let focused = cx.update(|window, app| window.focused(app).map(|handle| handle.tab_index));
        let mut indices: Vec<isize> = focused.into_iter().collect();
        for _ in 0..WALK_LIMIT {
            let next = cx.update(|window, app| {
                window.focus_next(app);
                window.focused(app).map(|handle| handle.tab_index)
            });
            let Some(index) = next else {
                break;
            };
            if indices.first() == Some(&index) {
                break;
            }
            indices.push(index);
        }
        indices
    }

    /// Splits a walked tab order into the shell's own stops and the pane's own stops.
    fn tab_blocks(indices: &[isize], first_pane_stop: isize) -> (Vec<isize>, Vec<isize>) {
        indices
            .iter()
            .copied()
            .partition(|index| *index < first_pane_stop)
    }

    /// The stops the shell shows on every pane: the search field, its clear button, the sidebar.
    ///
    /// The search field owns 0 and its clear button owns 1 (`TextInput::new`), and the toolbar
    /// toggle shares the first category's slot inside the sidebar block. A control that reached
    /// one of the two search slots would drop the clear button in the middle of the sidebar, and a
    /// category that left the block would collide with the pane beside it.
    fn assert_shell_tab_block(shell: &[isize]) {
        assert_eq!(
            shell.first(),
            Some(&0),
            "the search field is the first tab stop: {shell:?}"
        );
        assert!(
            shell
                .iter()
                .filter(|index| **index < tab_order::SIDEBAR_FIRST)
                .all(|index| *index <= 1),
            "only the search field and its clear button sit below the sidebar: {shell:?}"
        );
        assert!(
            shell
                .iter()
                .all(|index| *index <= tab_order::SIDEBAR_FIRST + 3),
            "the sidebar block stays inside its own four slots: {shell:?}"
        );
        for category in 0..SettingsCategory::ALL.len() as isize {
            assert!(
                shell.contains(&(tab_order::SIDEBAR_FIRST + category)),
                "category {category} is missing from the tab order: {shell:?}"
            );
        }
    }

    /// The blocks in `tab_order` only matter if the controls really carry them.
    ///
    /// The constants cannot check themselves, so every pane is walked with `focus_next` and the
    /// blocks are compared as they were drawn: a row that borrowed a slot from another block, or a
    /// block that started too low, would show up here as a duplicate or a missing stop.
    #[gpui::test]
    fn tab_order_blocks_do_not_collide(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        // General: the sidebar shell, then every control in the pane in the block it owns.
        let (shell, rows) = tab_blocks(&rendered_tab_indices(cx), tab_order::GENERAL_FIRST);
        assert_shell_tab_block(&shell);
        assert_eq!(
            rows,
            vec![
                tab_order::GENERAL_FIRST,
                tab_order::GENERAL_FIRST + 1,
                tab_order::GENERAL_FIRST + 2,
                tab_order::GENERAL_FIRST + 3,
                tab_order::GENERAL_DATA_FONT,
            ],
            "the General pane owns five consecutive slots, ending with the font size pop-up"
        );
        assert!(
            shell.iter().max() < rows.iter().min(),
            "the sidebar and the General rows share no slot: {shell:?} {rows:?}"
        );

        // Keyboard: the keymap file block first, then the command rows below it.
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.run_until_parked();
        let (shell, pane) = tab_blocks(&rendered_tab_indices(cx), tab_order::KEYMAP_FIRST);
        assert_shell_tab_block(&shell);
        let (file, commands): (Vec<isize>, Vec<isize>) = pane
            .iter()
            .copied()
            .partition(|index| *index < tab_order::KEYBOARD_FIRST);
        // Copy Path is the one control the block can lose: it is disabled where the platform has no
        // config directory to name. Cancel only exists while the restore step is armed, so the block
        // owns four consecutive slots and the walk visits them in slot order.
        let expected_file = (0..4)
            .map(|offset| tab_order::KEYMAP_FIRST + offset)
            .filter(|slot| {
                *slot != tab_order::KEYMAP_COPY_PATH || keymap::user_keymap_path().is_some()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            file, expected_file,
            "the keymap file block owns the four slots under KEYMAP_FIRST, in reading order"
        );
        assert!(
            file.windows(2).all(|pair| pair[1] > pair[0]),
            "two controls in the keymap file block share a slot: {file:?}"
        );
        assert!(
            rows.iter().max() < file.iter().min(),
            "the keymap file block starts above the General rows: {rows:?} {file:?}"
        );
        assert!(
            !commands.is_empty(),
            "the Keyboard pane lists the installed shortcuts: {commands:?}"
        );
        assert!(
            commands.windows(2).all(|pair| pair[1] > pair[0]),
            "two command rows share no slot: {commands:?}"
        );
        assert!(
            file.iter().max() < commands.iter().min(),
            "the keymap file block and the command rows share no slot: {file:?} {commands:?}"
        );

        // About: the pane owns the block under ABOUT_FIRST, in the order the rows are drawn.
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::About, cx)
        });
        cx.run_until_parked();
        let (shell, about) = tab_blocks(&rendered_tab_indices(cx), tab_order::ABOUT_FIRST);
        assert_shell_tab_block(&shell);
        assert_eq!(
            about,
            vec![tab_order::ABOUT_FIRST, tab_order::ABOUT_FIRST + 1],
            "the About pane owns the version copy and the update check"
        );

        // A ready update adds the restart after the check rather than on another block.
        view.update(cx, |view, cx| {
            view.set_update_actions(Some(UpdateActions::new(|_| {}, |_| {}, |_| {})), cx);
            view.set_update_state(UpdateUiState::new(UpdatePhase::Ready), cx);
        });
        cx.run_until_parked();
        let (shell, about) = tab_blocks(&rendered_tab_indices(cx), tab_order::ABOUT_FIRST);
        assert_shell_tab_block(&shell);
        assert_eq!(
            about,
            vec![
                tab_order::ABOUT_FIRST,
                tab_order::ABOUT_FIRST + 1,
                tab_order::ABOUT_FIRST + 2,
            ],
            "the About block holds the version copy, Check for Updates, and the restart: {about:?}"
        );
        // The command list grows by one pair of slots per row and the About pane is never drawn
        // with it, so the About block only has to clear the fixed blocks and the start of the list.
        assert!(
            shell.iter().max() < about.iter().min() && file.iter().max() < about.iter().min(),
            "the About block starts above the sidebar and the keymap file: {shell:?} {file:?} {about:?}"
        );
        assert!(
            about.iter().min() > commands.iter().min(),
            "the About block starts above the first command row: {about:?} {commands:?}"
        );
    }

    #[test]
    fn recording_accepts_function_keys_and_modified_shortcuts() {
        for source in [
            "f2",
            "f24",
            "ctrl-a",
            "shift-enter",
            "alt-delete",
            "ctrl-f12",
            "ctrl-f1",
        ] {
            let keystroke = gpui::Keystroke::parse(source).expect("valid test keystroke");
            assert!(recording_keystroke_allowed(&keystroke), "{source}");
        }
    }

    #[test]
    fn recording_rejects_unmodified_text_and_navigation_keys() {
        for source in [
            "a",
            "1",
            "space",
            "enter",
            "tab",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "insert",
            "escape",
            "backspace",
            "delete",
            "f0",
            "f1",
            "f25",
        ] {
            let keystroke = gpui::Keystroke::parse(source).expect("valid test keystroke");
            assert!(!recording_keystroke_allowed(&keystroke), "{source}");
        }
    }

    #[test]
    fn recording_hint_and_rejection_copy_agree() {
        let hint = recording_hint();
        assert!(hint.contains("F2–F24"));
        assert!(!hint.contains("F1–F24"));
        // The hint names the modifiers the rule accepts, so a person is not left guessing
        // whether the key they are holding counts. `Modifiers::modified` reports control, alt,
        // shift, and the platform key, so the list has to name all four under their own names.
        assert!(
            hint.contains(modifier_names()),
            "the hint must name the accepted modifiers: {hint}"
        );
        #[cfg(target_os = "macos")]
        for modifier in ["Control", "Command", "Option", "Shift"] {
            assert!(
                modifier_names().contains(modifier),
                "{modifier} is accepted by the rule and missing from the hint"
            );
        }
        #[cfg(not(target_os = "macos"))]
        for modifier in ["Ctrl", "Alt", "Shift", "Super"] {
            assert!(
                modifier_names().contains(modifier),
                "{modifier} is accepted by the rule and missing from the hint"
            );
        }
        // The banner has to name what it refuses, or a person presses the same key again.
        for key in ["letter", "Home", "End", "Page Up", "Page Down", "F1"] {
            assert!(
                RECORDING_REJECTED_KEYS.contains(key),
                "rejection copy omits {key}"
            );
        }
    }

    /// The refused-key table and the sentence that explains the refusal are one contract.
    ///
    /// The rule reads the table, so a key added to the table without a name in the copy would
    /// be refused with a message that does not mention it, and the person presses it again.
    #[test]
    fn the_rejection_copy_names_every_key_the_recorder_refuses() {
        for (refused, name) in RECORDING_REFUSED_BARE_KEYS {
            let keystroke = gpui::Keystroke::parse(refused).expect("valid test keystroke");
            assert!(
                !recording_keystroke_allowed(&keystroke),
                "{refused} is in the refused table, so the rule has to refuse it"
            );
            assert!(
                RECORDING_REJECTED_KEYS.contains(name),
                "rejection copy omits {name}, which the rule refuses as {refused:?}"
            );
        }
    }

    #[test]
    fn canonical_contexts_keep_shell_and_hotbar_bindings_separate() {
        assert_eq!(
            canonical_context("k8s_shell::CloseTab"),
            Some("Shell && !CommandPalette")
        );
        assert_eq!(canonical_context("k8s_hotbar::SwitchBank"), Some("Hotbar"));
        assert_eq!(canonical_context("k8s_shell::ToggleCommandPalette"), None);
    }

    /// A keymap section is a predicate over focus paths, and the shared lookup reads a path.
    ///
    /// The sections a row names are the ones the assets use, so the path each one answers for is
    /// fixed: a section that names a negated surface is asked about with that surface left out,
    /// because a path lists what holds focus and the predicate is what checks the negations.
    #[test]
    fn a_section_answers_for_its_own_surfaces_without_the_negations() {
        assert_eq!(section_focus_path("Shell && !CommandPalette"), "Shell");
        assert_eq!(
            section_focus_path("Shell && !CommandPalette && !Terminal"),
            "Shell"
        );
        assert_eq!(section_focus_path("Table && !CommandPalette"), "Table");
        assert_eq!(section_focus_path(INSPECTOR_CONTEXT), "Inspector");
        assert_eq!(section_focus_path("Hotbar"), "Hotbar");
        // A section that only rules surfaces out is the workbench with no surface of its own.
        assert_eq!(section_focus_path("!CommandPalette"), "");
    }

    #[test]
    fn settings_controls_keep_the_desktop_hit_target() {
        assert_eq!(f32::from(design::size::CONTROL), 28.0);
    }

    #[test]
    fn settings_keymap_copy_uses_platform_modifier_names() {
        let description = keymap_description();
        assert!(description.contains(platform_name()));
        assert!(description.contains(modifier_names()));
        assert!(description.ends_with("Saved changes reload automatically."));
        assert_eq!(
            keymap_reload_description(),
            "Reload the user keymap and apply the saved shortcuts."
        );
    }

    #[gpui::test]
    fn settings_exposes_search_and_four_categories(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-search").is_some());
        assert!(cx.debug_bounds("shared-text-input").is_some());
        let first_category = cx
            .debug_bounds("settings-category-General")
            .expect("General category");
        let first_row = cx
            .debug_bounds("settings-theme-trigger")
            .expect("Theme setting");
        assert!(
            f32::from(first_category.top()) < 200.0,
            "category top was {}",
            f32::from(first_category.top())
        );
        assert!(
            f32::from(first_row.top()) < 200.0,
            "row top was {}; category bounds: {first_category:?}",
            f32::from(first_row.top())
        );
        assert!(f32::from(first_category.right()) <= f32::from(first_row.left()) + 1.0);
        for category in ["General", "Keyboard", "Integrations", "About"] {
            let selector = match category {
                "General" => "settings-category-General",
                "Keyboard" => "settings-category-Keyboard",
                "Integrations" => "settings-category-Integrations",
                "About" => "settings-category-About",
                _ => unreachable!(),
            };
            assert!(cx.debug_bounds(selector).is_some());
        }

        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let wide_row = cx
            .debug_bounds("settings-theme-trigger")
            .expect("Theme setting at wide width");
        assert!(
            f32::from(wide_row.right())
                <= SETTINGS_SIDEBAR_WIDTH + SETTINGS_CONTENT_MAX_WIDTH + 1.0
        );
    }

    #[gpui::test]
    fn settings_search_broad_query_and_category_navigation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.search_query = "shortcut".to_owned();
                assert!(view.keyboard_search_is_broad());
                // Picking a category answers "show me this one", not "forget what I searched for".
                view.select_category(SettingsCategory::About, cx);
                assert_eq!(view.search_query, "shortcut");
                assert_eq!(view.category, SettingsCategory::About);
                // A filtered list shows the panes that still have a match, so the one the person
                // named while typing is not on screen and nothing is highlighted as "the open
                // pane". Before, the list held all four while the content pane held one, and the
                // sidebar row for the other three did nothing but reset the scroll offset.
                assert_eq!(
                    view.visible_categories(cx),
                    vec![SettingsCategory::Keyboard],
                    "a broad keyboard query keeps the Keyboard pane and nothing else"
                );
                assert_eq!(view.highlighted_category(), None);
                assert!(
                    !view.category_row_available(SettingsCategory::About, cx),
                    "a category the filter emptied is not available to pick"
                );
                assert!(
                    view.category_row_available(SettingsCategory::Keyboard, cx),
                    "a category the filter kept is available to pick"
                );
            });
        });
        cx.run_until_parked();
        // A row the filter emptied leaves the tab order with it, so Tab cannot land on a control
        // that does nothing.
        let walked = rendered_tab_indices(cx);
        assert!(
            walked.contains(&(tab_order::SIDEBAR_FIRST + 1)),
            "the Keyboard row the query kept is still reachable: {walked:?}"
        );
        // The General row shares the first sidebar slot with the toolbar's toggle, which no query
        // empties, so one visit to that slot is the proof the row itself left it: two controls on
        // one slot are walked twice.
        assert_eq!(
            walked
                .iter()
                .filter(|index| **index == tab_order::SIDEBAR_FIRST)
                .count(),
            1,
            "the General row the query emptied left the toggle's slot: {walked:?}"
        );
        for emptied in [2, 3] {
            assert!(
                !walked.contains(&(tab_order::SIDEBAR_FIRST + emptied)),
                "the row the query emptied is still a tab stop: {walked:?}"
            );
        }

        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_search_query("", cx);
                assert_eq!(view.highlighted_category(), Some(SettingsCategory::About));
                assert_eq!(view.visible_categories(cx), vec![SettingsCategory::About]);
            });
        });
    }

    /// The toolbar number is the only way a person tells a narrow search from an empty one, so it
    /// has to count the rows that are actually on screen. Every query below is therefore read back
    /// twice: once as the number the toolbar shows, once as the rows the surface drew, so a count
    /// that drifts from the render fails here instead of in a person's face.
    #[gpui::test]
    fn search_result_count_follows_the_filtered_panes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        // One row answers the query, so the count is one and the row that does not answer it is
        // gone from the pane.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_search_query("increase contrast", cx)
            })
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_count(cx)),
            1,
            "only the Increase contrast setting answers the query"
        );
        assert!(
            cx.debug_bounds("settings-increase-contrast").is_some(),
            "the row the count promises is on screen"
        );
        assert!(
            cx.debug_bounds("settings-theme-trigger").is_none(),
            "a row that does not answer the query is not drawn beside it"
        );

        // A category is found by its own name, so naming the pane keeps every row it owns, and
        // the number is the number of rows the pane draws.
        let general_rows = SettingsCategory::General.settings().len();
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("General", cx)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_count(cx)),
            general_rows,
            "the category name keeps the whole pane"
        );
        assert!(cx.debug_bounds("settings-theme-trigger").is_some());
        assert!(
            cx.debug_bounds("settings-data-font-trigger").is_some(),
            "the data font size row is one of the rows the count promises"
        );

        // A row the pane draws has to be a row the search can find, or a query about it drops
        // the pane and the control never mounts.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("text size", cx)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_count(cx)),
            1,
            "a query about the data font size finds that setting"
        );
        assert!(cx.debug_bounds("settings-data-font-trigger").is_some());

        // The section description is header prose, not a row label. A word only it carries matches
        // no setting, so the surface says so instead of opening a pane of rows that do not answer
        // the query.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("appearance", cx)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.search_query.clone()),
            "appearance",
            "the query the field reports is the one under test"
        );
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_count(cx)),
            0,
            "a word from the header description alone matches no setting"
        );
        assert!(cx.debug_bounds("settings-no-matches").is_some());
        assert!(cx.debug_bounds("settings-theme-trigger").is_none());

        // Clearing the query brings the pane back whole.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("", cx)));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_count(cx)),
            general_rows
        );
        assert!(cx.debug_bounds("settings-no-matches").is_none());
    }

    /// The pane name belongs to the section header, and a query's count to the toolbar.
    ///
    /// The window title, the tab title, the sidebar's selected row and the section header already
    /// name the pane. The toolbar used to print a fifth copy at `panel_title`, 14px, above a
    /// section header at 15px: 38.5px apart with the smaller one on top, so the duplicate read as
    /// the heading. `DESIGN.md` §3.1 does not allow a title to restate all three at once, and §2
    /// says a duplicate title should be deleted.
    #[gpui::test]
    fn the_section_header_names_the_pane_and_the_toolbar_owns_the_result_count(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-heading-Keyboard").is_some(),
            "the open pane names itself in its own section header"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.pane_title()),
            "Settings · Keyboard",
            "the window and tab titles keep naming the pane"
        );
        assert!(
            cx.debug_bounds("settings-search-results").is_none(),
            "with no query there is no count to state"
        );
        assert_eq!(
            cx.update(|_, cx| layout_state(cx).category),
            Some(SettingsCategory::Keyboard)
        );

        // While a query is active the count is the one thing the toolbar has left to say, and it
        // has to be the number the pane actually drew.
        cx.update(|_, cx| view.update(cx, |view, cx| view.set_search_query("shortcut", cx)));
        cx.run_until_parked();
        let expected = view.read_with(cx, |view, cx| view.search_result_count(cx));
        assert!(expected > 0, "a broad query matches the command list");
        assert_eq!(
            view.read_with(cx, |view, cx| view.search_result_label(cx)),
            format!("Search results · {expected}")
        );
        assert!(cx.debug_bounds("settings-search-results").is_some());

        // A reopened tab is a new view: it has to land on the same pane.
        let reopened = cx.update(|_, cx| cx.new(SettingsView::new));
        cx.run_until_parked();
        assert_eq!(
            reopened.read_with(cx, |view, _| view.category),
            SettingsCategory::Keyboard
        );
    }

    #[gpui::test]
    fn sidebar_collapses_to_a_rail_that_keeps_the_search(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let toggle = cx
            .debug_bounds("settings-sidebar-toggle")
            .expect("sidebar toggle is laid out");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.sidebar_collapsed));
        // The rail keeps every category reachable and the toolbar keeps the search field.
        for category in ["General", "Keyboard", "Integrations", "About"] {
            let selector = match category {
                "General" => "settings-category-General",
                "Keyboard" => "settings-category-Keyboard",
                "Integrations" => "settings-category-Integrations",
                _ => "settings-category-About",
            };
            assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
        }
        assert!(cx.debug_bounds("settings-search").is_some());
        let rail = cx
            .debug_bounds("settings-category-General")
            .expect("rail button");
        assert!(f32::from(rail.right()) <= SETTINGS_SIDEBAR_RAIL_WIDTH + 1.0);

        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, _| view.sidebar_collapsed));
    }

    /// The view zoom chords were removed because the interface cannot scale its chrome. This
    /// control is the replacement: a text size that actually reaches the data surfaces.
    #[gpui::test]
    fn the_data_font_size_control_offers_choices_and_persists_one(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let trigger = cx
            .debug_bounds("settings-data-font-trigger")
            .expect("the General pane offers a data font size control");
        // The trigger announces itself as a combo box with the current value, so the size is
        // readable without opening the menu.
        assert_eq!(
            data_font_label(crate::settings::PRODUCT_DATA_FONT_SIZE),
            "12 px (default)"
        );

        view.update(cx, |view, cx| view.apply_data_font_size(16., cx));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |_, cx| data_font_size(cx)),
            16.,
            "the chosen size reaches the value the data surfaces measure themselves with"
        );

        // Out-of-range requests clamp to the offered list rather than writing a size the menu
        // cannot show as selected.
        view.update(cx, |view, cx| view.apply_data_font_size(400., cx));
        cx.run_until_parked();
        let highest = *DATA_FONT_SIZES.last().unwrap();
        assert_eq!(view.read_with(cx, |_, cx| data_font_size(cx)), highest);

        view.update(cx, |view, cx| view.apply_data_font_size(1., cx));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |_, cx| data_font_size(cx)),
            *DATA_FONT_SIZES.first().unwrap()
        );
        assert!(
            trigger.size.width > px(0.),
            "the trigger keeps its hit area"
        );
    }

    #[gpui::test]
    fn restore_defaults_confirms_before_deleting_the_user_keymap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let before = cx.update(|_, cx| keymap::status(cx).epoch);
        let button = cx
            .debug_bounds("settings-keymap-restore-defaults")
            .expect("Restore Defaults is laid out");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        // The first press only arms: the step says what it deletes and nothing changed yet.
        assert_eq!(
            view.read_with(cx, |view, _| view.restore_step),
            RestoreStep::Destructive
        );
        assert!(cx.debug_bounds("settings-restore-confirm").is_some());
        assert!(cx.debug_bounds("settings-keymap-restore-cancel").is_some());
        assert_eq!(cx.update(|_, cx| keymap::status(cx).epoch), before);

        // Escape leaves the step without deleting.
        let escape = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("escape").expect("escape parses"),
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
            view.read_with(cx, |view, _| view.restore_step),
            RestoreStep::Idle
        );
        assert_eq!(cx.update(|_, cx| keymap::status(cx).epoch), before);

        // The second press on the armed button is the destructive one. It removes the real user
        // keymap file, so the click only runs where there is nothing to remove; a machine that has
        // one keeps it and still proves both confirmation steps.
        if keymap::user_keymap_path().is_some_and(|path| path.is_file()) {
            return;
        }
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let armed = cx
            .debug_bounds("settings-keymap-restore-defaults")
            .expect("armed Restore Defaults is laid out");
        cx.simulate_click(armed.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.restore_step),
            RestoreStep::Idle
        );
        assert!(
            cx.update(|_, cx| keymap::status(cx).epoch) > before,
            "the second press reloads the keymap"
        );
    }

    #[gpui::test]
    fn conflicting_binding_is_reported_on_its_own_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
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

        // A write that lands on a contested key must not claim plain success.
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
        assert_eq!(
            view.read_with(cx, |view, _| view.error.clone()),
            Some(
                "The shortcut was saved, but ctrl-shift-t is also bound to k8s_shell::FocusYaml in the main workbench outside the Command Palette. The last binding wins. Change one of them, then record the shortcut again."
                    .to_owned()
            )
        );
    }

    #[gpui::test]
    fn async_settings_write_failure_reaches_the_banner(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-save-pending").is_none());

        cx.update(|_, cx| {
            cx.set_global(settings::SettingsSaveStatus {
                pending: true,
                error: None,
                epoch: 1,
                rollback: None,
                rejected: None,
            })
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.save_pending));
        assert!(cx.debug_bounds("settings-save-pending").is_some());

        // The write fails on the background task, after the toggle already flipped.
        cx.update(|_, cx| {
            cx.set_global(settings::SettingsSaveStatus {
                pending: false,
                error: Some("Permission denied".to_owned()),
                epoch: 2,
                rollback: None,
                rejected: None,
            })
        });
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, _| view.save_pending));
        assert_eq!(
            view.read_with(cx, |view, _| view.save_error.clone()),
            Some("Permission denied".to_owned())
        );
        let banner = cx
            .debug_bounds("settings-save-error")
            .expect("the failed write is reported");
        assert!(f32::from(banner.size.height) > 0.0);
        // The message tells the person to retry, so the retry has to be on the banner.
        assert!(
            cx.debug_bounds("settings-save-retry").is_some(),
            "the failed write offers the retry its own message promises"
        );
    }

    /// The sentence and the recovery control are one line, and the failure is on the row it
    /// belongs to.
    ///
    /// The banner is a `w_full` row on the surface root with a spacer in the middle, so in a wide
    /// window the sentence sat at x≈16 and `Retry` at x≈1904: 1888px of nothing between the
    /// promise and the button. `feedback.md` asks for an error as close to the problem as it can
    /// be, and the rows the failed write touched are where the problem is.
    #[gpui::test]
    fn a_failed_write_marks_its_rows_and_keeps_the_retry_on_one_measure(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(1_600.), px(900.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-row-note-settings-disk-cache")
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
            "the retry is {:.0}px from the sentence, past the {}px measure",
            f32::from(retry.right()) - f32::from(banner.left()),
            SETTINGS_CONTENT_MAX_WIDTH
        );

        // Both rows whose settings live in that file now say the change did not land, in the label
        // column of the row that has to be undone.
        let content = cx
            .debug_bounds("settings-content")
            .expect("the content column is laid out");
        let label_column = f32::from(content.left()) + f32::from(space::LG);
        for selector in [
            "settings-row-note-settings-increase-contrast",
            "settings-row-note-settings-disk-cache",
        ] {
            let note = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is on its row"));
            assert!(
                f32::from(note.left()) >= label_column - 1.0
                    && f32::from(note.right()) <= label_column + SETTINGS_LABEL_WIDTH + 1.0,
                "{selector} is at {:?}, outside the {}px label column at {label_column}px",
                note,
                SETTINGS_LABEL_WIDTH
            );
        }
        assert_eq!(
            view.read_with(cx, |view, _| view
                .save_failure_note("settings-disk-cache")
                .map(|note| note.text)),
            Some(SAVE_NOT_SAVED_NOTE.to_owned())
        );

        // The write's own outcome is what clears it, not the retry click. A retry used to take the
        // sentence and the button off screen before the file was touched, so a second failure put
        // them back with nothing having changed in between.
        cx.update(|_, cx| {
            cx.set_global(settings::SettingsSaveStatus {
                pending: true,
                error: None,
                epoch: 2,
                rollback: None,
                rejected: None,
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-save-error").is_none());
        assert!(cx.debug_bounds("settings-save-pending").is_some());
        assert!(
            cx.debug_bounds("settings-row-note-settings-disk-cache")
                .is_none()
        );

        cx.update(|_, cx| {
            cx.set_global(settings::SettingsSaveStatus {
                pending: false,
                error: Some("Permission denied".to_owned()),
                epoch: 3,
                rollback: None,
                rejected: None,
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-save-error").is_some());
        assert!(
            cx.debug_bounds("settings-row-note-settings-disk-cache")
                .is_some()
        );
    }

    /// A retry writes the change the file refused, not the value the store fell back to.
    ///
    /// A failed write puts the store back, so the store and the reader's intent are now
    /// different values. Pressing `Retry` next to a row that says "Not saved" and having the
    /// old value written again would report success for a change that never happened.
    ///
    /// This covers the view's half: what the panel shows while the failure stands, and what
    /// `Retry` offers afterwards. The store actually moving -- and the write actually
    /// failing -- belongs to `settings.rs`'s own test, which drives a real unwritable path;
    /// setting a `SettingsSaveStatus` here would skip the rollback, because that happens
    /// inside the write task rather than in the status.
    #[gpui::test]
    fn a_retry_after_a_failed_write_offers_the_refused_change_again(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        // The reader turned Increase contrast on, the file refused the write, and the store went
        // back to the value the file still holds. The two snapshots have opposite jobs and
        // swapping them is the bug this test exists for: `rollback` is what the disk holds and
        // what the store must be restored *to*, `rejected` is what the reader asked for and what a
        // retry must write *again*.
        let on_disk = crate::settings::UserSettings {
            increase_contrast: Some(false),
            ..crate::settings::UserSettings::default()
        };
        let refused = crate::settings::UserSettings {
            increase_contrast: Some(true),
            ..crate::settings::UserSettings::default()
        };
        cx.update(|_, cx| {
            crate::settings::set_test_increase_contrast(cx, true);
            crate::settings::set_test_settings_memory(&refused.clone());
            cx.set_global(crate::settings::SettingsSaveStatus {
                pending: false,
                error: Some("Cannot write settings file: Permission denied".to_owned()),
                epoch: 1,
                rollback: Some(on_disk.clone()),
                rejected: Some(refused.clone()),
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-save-error").is_some());
        assert!(
            cx.debug_bounds("settings-row-note-settings-increase-contrast")
                .is_some()
        );

        view.update(cx, |view, cx| view.retry_settings_save(cx));
        cx.run_until_parked();
        // The write finished, not merely started: a scheduled write clears the banner too, so
        // the status is what says the file took it.
        let status = cx.read(crate::settings::save_status);
        assert!(
            cx.read(crate::settings::increase_contrast_enabled),
            "the retry has to write the change the reader asked for, not the value the store \
             fell back to when the first attempt failed"
        );
        assert!(!status.pending, "the retried write has not finished");
        assert!(
            status.rejected.is_none(),
            "a written change is nothing left to retry"
        );
        assert_eq!(
            status.error, None,
            "a written retry has no failure left to report"
        );
        assert!(view.read_with(cx, |view, _| view.save_error.is_none()));
        assert!(cx.debug_bounds("settings-save-error").is_none());
        assert!(
            cx.debug_bounds("settings-row-note-settings-increase-contrast")
                .is_none()
        );
    }

    /// A new query or a new category replaces the rows on screen, so the state that belonged to
    /// the rows that are gone has to go with them.
    ///
    /// An armed Restore Defaults step left behind is the sharp end: it leaves a destructive button
    /// waiting for a second press on a pane nobody is looking at. The offset is the quieter half —
    /// a new pane opens halfway down because the old pane was longer.
    #[gpui::test]
    fn a_new_query_or_category_returns_the_pane_to_its_first_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.request_restore_defaults(cx);
                view.scroll.set_offset(point(px(0.), px(-240.)));
                assert_eq!(view.restore_step, RestoreStep::Destructive);
                assert_eq!(view.scroll.offset().y, px(-240.));

                view.set_search_query("theme", cx);
                assert_eq!(
                    view.restore_step,
                    RestoreStep::Idle,
                    "a query cannot leave the previous pane's destructive step armed"
                );
                assert_eq!(view.scroll.offset().y, px(0.));
                // A filtered list draws the panes that have a match, so the sidebar cannot still
                // claim the one the person was on when they started typing, and it cannot offer
                // the ones the query emptied either.
                assert_eq!(view.highlighted_category(), None);
                assert_eq!(
                    view.visible_categories(cx),
                    vec![SettingsCategory::General, SettingsCategory::Keyboard],
                    "`theme` answers in the General settings and in the theme commands"
                );

                view.clear_search(cx);
                assert_eq!(view.highlighted_category(), Some(SettingsCategory::General));

                view.request_restore_defaults(cx);
                view.scroll.set_offset(point(px(0.), px(-240.)));
                view.select_category(SettingsCategory::About, cx);
                assert_eq!(view.restore_step, RestoreStep::Idle);
                assert_eq!(
                    view.scroll.offset().y,
                    px(0.),
                    "a new category starts at its own first row"
                );
                assert_eq!(view.highlighted_category(), Some(SettingsCategory::About));
            })
        });
    }

    /// Every boolean control in the General pane takes the desktop target.
    ///
    /// The control is a square in a square 28px target, so a pointer and the focus ring are the
    /// same size as the Medium buttons beside them. The pill switch this replaced drew a 20x14
    /// track, which made the clickable control a third shorter than the row it sat in.
    #[gpui::test]
    fn every_general_boolean_takes_the_desktop_hit_target(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        for selector in ["settings-increase-contrast", "settings-disk-cache"] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is laid out"));
            assert!(
                f32::from(bounds.size.height) >= f32::from(design::size::CONTROL),
                "{selector} is {}px tall, below the {}px control target",
                f32::from(bounds.size.height),
                f32::from(design::size::CONTROL)
            );
            assert!(
                f32::from(bounds.size.width) >= f32::from(design::size::CONTROL),
                "{selector} is {}px wide, below the {}px control target",
                f32::from(bounds.size.width),
                f32::from(design::size::CONTROL)
            );
        }
    }

    /// A settings row is read as a pair, so every control shares one column on the trailing edge.
    ///
    /// The value column used to start at a fixed offset from the leading edge, which in a 1600px
    /// window put a theme dropdown a third of the way across the pane and left the rest of the
    /// measure empty. It is now the row's last fixed column, so the controls line up with each
    /// other and end where the content measure ends.
    ///
    /// The walk covers all three panes, not just General's five rows. The Keyboard row's control is
    /// a keycap plus Edit plus Clear, and About's is a version string with a copy control or an
    /// update check beside a restart; two medium buttons measure about 308px against the 240px
    /// column, so the restart had to move to a line of its own rather than overflow into the label.
    #[gpui::test]
    fn the_settings_rows_share_one_control_column(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(1_600.), px(900.)));
        cx.run_until_parked();
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
        for category in [
            SettingsCategory::General,
            SettingsCategory::Keyboard,
            SettingsCategory::About,
        ] {
            cx.update(|_, cx| view.update(cx, |view, cx| view.select_category(category, cx)));
            cx.run_until_parked();
            // Every entry is a control that has to end on the measure's trailing edge, so a row
            // with three of them is measured through its column rather than through its keycap.
            for selector in match category {
                SettingsCategory::General => vec![
                    "settings-theme-trigger",
                    "settings-increase-contrast",
                    "settings-disk-cache",
                    "settings-reduce-motion",
                    "settings-data-font-trigger",
                ],
                SettingsCategory::Keyboard => vec!["settings-keyboard-controls"],
                _ => vec![
                    "settings-copy-version-action",
                    "settings-check-updates-action",
                ],
            } {
                let bounds = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} is laid out"));
                assert!(
                    (f32::from(bounds.right()) - trailing_edge).abs() <= 1.0,
                    "{selector} ends at {}px, not on the {trailing_edge}px trailing edge",
                    f32::from(bounds.right())
                );
                checked += 1;
            }
            if category == SettingsCategory::Keyboard {
                // The keycap leads the column, so it is checked against the column rather than
                // against the trailing edge.
                let keycap = cx
                    .debug_bounds("settings-keycap")
                    .expect("the keycap column is laid out");
                assert!(
                    f32::from(keycap.left()) >= control_column - 1.0
                        && f32::from(keycap.right()) <= trailing_edge + 1.0,
                    "the keycap is at {keycap:?}, outside the {control_column}px control column"
                );
            }
        }
        assert_eq!(checked, 8, "every row's control was measured");
    }

    /// The Keyboard pane is redrawn from one command list, and a keymap reload redraws it from a
    /// fresh one.
    ///
    /// The list costs a few hundred allocations to build, and the pane used to build it up to four
    /// times per frame. The memo is keyed on the keymap generation, the updater, and the registered
    /// action count, so a reload cannot leave a stale shortcut on screen.
    #[gpui::test]
    fn the_keyboard_command_list_is_built_once_per_keymap_generation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(1_600.), px(900.)));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.run_until_parked();

        let first = view.read_with(cx, |view, cx| view.keyboard_sections(cx));
        let second = view.read_with(cx, |view, cx| view.keyboard_sections(cx));
        assert!(
            Rc::ptr_eq(&first, &second),
            "two reads in the same keymap generation must share one list"
        );
        assert!(
            !first.is_empty(),
            "the Keyboard pane has commands to draw, so the list is not empty"
        );

        cx.update(|_, cx| crate::keymap::reload(cx).expect("the keymap reloads"));
        let reloaded = view.read_with(cx, |view, cx| view.keyboard_sections(cx));
        assert!(
            !Rc::ptr_eq(&first, &reloaded),
            "a keymap reload has to rebuild the list"
        );
    }

    /// A category row marks the open pane with a surface and a position, not a saturated block.
    ///
    /// The search field owns a focus ring, and an accent slab beside it made the selection the
    /// loudest thing on the panel while the ring around the field that filters it stayed quiet.
    #[gpui::test]
    fn a_selected_category_row_is_a_surface_and_a_rail(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            let panel = design::surface::panel(cx);
            let selected = category_row_background(true, cx);
            let resting = category_row_background(false, cx);
            assert_eq!(selected, design::row_selected_bg(cx));
            assert_ne!(
                selected, panel,
                "the selected row has to be visible on the sidebar"
            );
            assert_ne!(selected, resting, "selection has to change the row");
            assert_eq!(
                category_row_rail_width(),
                f32::from(design::border::FOCUS_RAIL),
                "the rail is a position cue, so it is a rail's width and not a slab"
            );
            assert_eq!(f32::from(design::border::FOCUS_RAIL), 2.0);
            // The rail is a graphic a reader has to find, so it clears the interactive
            // threshold against the fill it is painted on.
            let rail = category_row_rail_color(cx);
            assert!(
                ui::utils::calculate_contrast_ratio(rail, selected.alpha(1.0))
                    >= design::border::INTERACTIVE_MIN_CONTRAST,
                "the rail is below the interactive threshold on the selected fill"
            );
        });
    }

    /// The store's default settings carry `reduce_motion: "off"`, so a merged read can never say
    /// "nobody chose" and would make the inherited state unreachable.
    #[gpui::test]
    fn reduce_motion_can_hand_the_choice_back_to_the_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let stored = |text: &'static str, cx: &TestAppContext| {
            cx.update(|cx| {
                SettingsStore::update(cx, |store, cx| {
                    store
                        .set_user_settings(text, cx)
                        .result()
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

        // `System` is the state that writes nothing, so it is the only one that can be undone.
        assert_eq!(ReduceMotionChoice::System.mode(), None);
        assert_eq!(ReduceMotionChoice::On.mode(), Some(ReduceMotionMode::On));
        assert_eq!(ReduceMotionChoice::Off.mode(), Some(ReduceMotionMode::Off));
        assert_eq!(ReduceMotionChoice::ALL.len(), 3);
        for choice in ReduceMotionChoice::ALL {
            assert!(!choice.label().is_empty());
        }
    }

    #[gpui::test]
    fn pane_errors_only_appear_on_the_pane_that_caused_them(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        view.update(cx, |view, cx| {
            view.set_pane_error(
                SettingsCategory::About,
                UPDATES_UNAVAILABLE_MESSAGE.to_owned(),
            );
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("settings-error").is_none(),
            "an About failure stays out of the Keyboard list"
        );
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::About, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-error").is_some());
    }

    #[gpui::test]
    fn keymap_reload_generation_refreshes_the_keyboard_list(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let generation = view.read_with(cx, |view, _| view.keymap_generation);
        assert!(generation > 0);

        // An external edit of keymap.json rebuilds the bindings and bumps the generation.
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

    #[gpui::test]
    fn ready_update_offers_the_restart_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            cx.set_global(SettingsLayout::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
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
            view.select_category(SettingsCategory::About, cx);
            view.set_update_actions(Some(actions), cx);
            view.set_update_state(UpdateUiState::new(UpdatePhase::Ready), cx);
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let restart = cx
            .debug_bounds("settings-restart-update")
            .expect("a ready update offers the restart");
        cx.simulate_click(restart.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(*restarts.borrow(), 1);
    }

    #[gpui::test]
    fn increase_contrast_is_a_keyboard_reachable_general_setting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("settings-increase-contrast")
            .expect("increase contrast check box is laid out");
        assert!(f32::from(bounds.size.height) >= 28.0);
    }

    #[gpui::test]
    fn missing_update_actions_show_unavailable_feedback(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
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
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::About, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("settings-check-updates-action")
            .expect("update action is laid out");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.error.clone()),
            Some(UPDATES_UNAVAILABLE_MESSAGE.to_owned())
        );
    }

    /// UI Zoom stays fail-closed, and the sentence that says so is the guard.
    ///
    /// `DESIGN.md` §3.1 and §9 require the Zoom actions to keep changing no font size, spacing,
    /// control or window size until the scale tokens land. The one row that documents the command
    /// has to keep naming that, so this test asserts the property rather than one wording: a
    /// future change that makes the action do something has to rewrite the sentence deliberately,
    /// and a reworded refusal still passes.
    #[test]
    fn the_scale_selection_row_documents_the_fail_closed_command() {
        let description = keyboard_description("k8s_shell::ScaleSelection", "Scale Selection");
        assert!(
            description.contains("not available"),
            "the fail-closed command has to say it is unavailable: {description}"
        );
        assert!(
            description.to_lowercase().contains("zoom"),
            "the refusal has to name what is refused: {description}"
        );
        assert!(
            !description.contains("Run "),
            "the row must not describe an action it does not perform: {description}"
        );
        assert_eq!(
            description, "UI zoom is not available in this build.",
            "the shipped sentence is the contract wording"
        );
    }

    #[gpui::test]
    fn keyboard_sections_use_registered_action_backed_commands(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let sections = keyboard_sections(cx, true);
            let names = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .map(|command| command.action_name.as_str())
                .collect::<Vec<_>>();
            assert!(names.contains(&"k8s_shell::CloseTab"));
            assert!(names.contains(&"k8s_shell::FocusYaml"));
            assert!(names.contains(&"k8s_shell::SearchResources"));
            assert!(names.contains(&"k8s_shell::ToggleCommandPalette"));
            assert!(names.contains(&"k8s_app::OpenSettings"));
            assert!(!names.contains(&"k8s_ops::EditYaml"));
        });
    }

    /// Every row says what its command does, so the list is built here and read as a person
    /// reads it.
    ///
    /// The table behind [`keyboard_description`] used to end in `_ => format!("Run {label}.")`, and
    /// 33 of 68 rows reached it, so `Scale Selection` described a command the contract makes
    /// fail-closed as something it does. Enumerating the list the pane actually draws is what makes
    /// a newly registered action fail here instead of degrading silently: a row with no sentence of
    /// its own is exactly the one the generic sentence produced.
    #[gpui::test]
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
            // UI Zoom is fail-closed by contract, so the one row that documents it has to say
            // that. Before, the table had no entry and printed "Run Scale Selection.".
            let scale = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::ScaleSelection")
                .expect("Scale Selection is listed");
            assert_eq!(
                scale.description, "UI zoom is not available in this build.",
                "the one fail-closed command is documented as an unavailable one"
            );
            // The two theme actions live in the `k8s_shell` namespace, so the table used to carry
            // `k8s_app::Use…` arms that matched nothing.
            for action in [
                "k8s_shell::UseLightTheme",
                "k8s_shell::UseDarkTheme",
                "k8s_shell::UseSystemTheme",
            ] {
                let command = commands
                    .iter()
                    .find(|command| command.action_name == action)
                    .unwrap_or_else(|| panic!("{action} is listed"));
                assert_ne!(
                    command.description,
                    format!("Run {}.", command.label),
                    "{action}"
                );
            }
        });
    }

    /// A one-click, immediately reversible setting is not a completed activity.
    ///
    /// Five General controls pushed a notification on every success, and Data font size used
    /// `Severity::Success`, which spends the health channel `DESIGN.md` §2 budgets for one place on
    /// a drop-down. `feedback.md` says a person typically expects the action to succeed and only
    /// needs to hear when it does not, so the four switches keep the failure and drop the success.
    #[gpui::test]
    fn a_settings_switch_does_not_confirm_its_own_success(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            settings::set_test_settings_memory(&crate::settings::UserSettings::default());
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = notices.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, _, _| sink.borrow_mut().push(message));
        });

        view.update(cx, |view, cx| {
            view.apply_increase_contrast_result(Ok(()), cx);
            view.apply_disk_cache_result(Ok(()), cx);
            view.apply_data_font_size(14., cx);
            view.apply_reduce_motion(ReduceMotionChoice::On, cx);
        });
        cx.run_until_parked();
        assert!(
            notices.borrow().is_empty(),
            "a switch that reports itself restated the control that already showed it: {:?}",
            notices.borrow()
        );
        assert!(
            view.read_with(cx, |view, _| view.error.clone()).is_none(),
            "the quiet half is still the success path"
        );

        // The failure is the half worth a sentence, and it stays in the row's own error banner.
        view.update(cx, |view, cx| {
            view.apply_disk_cache_result(Err("Permission denied".to_owned()), cx);
        });
        assert!(view.read_with(cx, |view, _| view.error.is_some()));
    }

    /// The three theme actions are in the `k8s_shell` namespace, and the description table carried
    /// the `k8s_app` spelling.
    ///
    /// Two unreachable arms in a match are worse than no arm: they read as if the rows are covered
    /// while the rows fall through to the generic sentence. `every_listed_action_says_what_it_does`
    /// catches the symptom; this pins the names so the next table edit cannot reintroduce it.
    #[test]
    fn the_theme_commands_are_named_in_their_own_namespace() {
        assert_eq!(
            keyboard_description("k8s_shell::UseLightTheme", "Use Light Theme"),
            "Switch the workbench to the light theme."
        );
        assert_eq!(
            keyboard_description("k8s_shell::UseDarkTheme", "Use Dark Theme"),
            "Switch the workbench to the dark theme."
        );
        assert_eq!(
            keyboard_description("k8s_shell::UseSystemTheme", "Use System Theme"),
            "Follow the desktop's light or dark setting."
        );
    }

    #[gpui::test]
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
            assert_eq!(switch_tab.when.as_deref(), Some("Shell && !CommandPalette"));
            assert!(commands.iter().any(|command| {
                command.action_name == "k8s_hotbar::SwitchCluster"
                    && !command.editable
                    && command.action_input.is_some()
                    && command.context.as_deref() == Some("Hotbar")
                    && command.when.as_deref() == Some("Hotbar")
            }));
            let palette = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::ToggleCommandPalette")
                .expect("ToggleCommandPalette is listed");
            assert!(palette.editable);
            assert_eq!(palette.context, None);
        });
    }

    /// The key an action is bound to inside one context, read from the live keymap.
    ///
    /// The row is compared against the binding itself rather than against a written-out chord,
    /// because `secondary` is Ctrl on Linux and Command on macOS.
    fn bound_chord_in_context(action_name: &str, context: &str, cx: &App) -> Option<String> {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        keymap
            .bindings()
            .filter(|binding| binding.action().name() == action_name)
            .find(|binding| keymap::binding_context(binding).as_deref() == Some(context))
            .and_then(|binding| binding.keystrokes().first().map(|key| key.unparse()))
    }

    /// Drops a queued keymap write so the suite never reaches the real user keymap file.
    ///
    /// `keymap::update_user_binding_with_outcome` rebinds in memory and enqueues the file write, so
    /// a test that records or clears a shortcut has a write pending. The `gpui::test` teardown runs
    /// the background executor, which would flush it into the developer's own `keymap.json`.
    fn discard_queued_keymap_write() {
        let Some(path) = keymap::user_keymap_path() else {
            return;
        };
        let queue = k8s_core::atomic_file::path_task_queue();
        while queue.pop(&path).is_some() {
            queue.finish(&path);
        }
    }

    #[test]
    fn canonical_context_names_the_inspector_for_every_inspector_action() {
        for action in [
            "k8s_inspector::ReloadActiveTab",
            "k8s_inspector::RetryMetrics",
            "k8s_inspector::MetricsRange5m",
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
            assert_eq!(
                canonical_context(action),
                Some(INSPECTOR_CONTEXT),
                "{action} is bound inside the Inspector, so the table has to say where"
            );
        }
        // The three switchers carry the terminal condition, so a key must not be read in the plain
        // shell context.
        assert_eq!(
            canonical_context("k8s_shell::OpenContextSwitcher"),
            Some("Shell && !CommandPalette && !Terminal")
        );
    }

    /// Every action the built-in keymap binds is reachable from the Keyboard list.
    ///
    /// The list is derived from the command palette, so two things can go wrong for one action: the
    /// row can exist with a context the binding does not use, which leaves the keycap blank, and the
    /// row can be missing altogether, which hides the shortcut. The first loop asks every bound
    /// action for a chord through the same path the row uses; the second asks whether the list names
    /// it, and the two tables above are the audited answer for the ones it does not, so an unlisted
    /// binding fails here rather than reaching a person.
    #[gpui::test]
    fn every_action_the_built_in_keymap_binds_shows_its_chord(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("default keymap loads");
            let mut names = cx
                .key_bindings()
                .borrow()
                .bindings()
                .map(|binding| binding.action().name().to_owned())
                .filter(|name| name.starts_with("k8s_"))
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            assert!(names.len() > 20, "the default keymap has to be installed");
            let sections = keyboard_sections(cx, true);
            let listed = sections
                .iter()
                .flat_map(|section| section.commands.iter())
                .map(|command| command.action_name.as_str())
                .collect::<Vec<_>>();
            let accounted_for = |name: &str| {
                listed.contains(&name)
                    || BOUND_ON_KEYS_THE_RECORDER_REFUSES
                        .iter()
                        .any(|(action, _)| *action == name)
                    || BOUND_WITHOUT_A_ROW
                        .iter()
                        .any(|(action, _)| *action == name)
            };
            for name in &names {
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
                    "{name} is bound but the Keyboard list has no row for it"
                );
            }
            // A name the keymap stopped binding would leave the tables above claiming a gap that is
            // not there, so the audit cannot rot into a list of excuses.
            for (name, reason) in BOUND_ON_KEYS_THE_RECORDER_REFUSES
                .iter()
                .chain(BOUND_WITHOUT_A_ROW)
            {
                assert!(names.contains(&name.to_string()), "{name}: {reason}");
            }
        });
    }

    #[gpui::test]
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
                "k8s_inspector::MetricsRange5m",
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
                    .unwrap_or_else(|| panic!("{action} is listed in the Keyboard list"));
                // The section is what Edit and Clear write into, so a row that names no context
                // would store a global override for a panel shortcut.
                assert_eq!(
                    command.context.as_deref(),
                    Some(INSPECTOR_CONTEXT),
                    "{action}"
                );
                assert!(command.editable, "{action} takes a user override");
                // The section is also what the row prints as "Active when: …". A row that named it
                // only on a conflict read as if the rest of the list had no scope at all.
                assert_eq!(command.when.as_deref(), Some(INSPECTOR_CONTEXT), "{action}");
                assert_eq!(
                    command_chord(command, cx),
                    bound_chord_in_context(action, INSPECTOR_CONTEXT, cx),
                    "{action} shows the key the Inspector context binds"
                );
                assert!(
                    !command.description.is_empty(),
                    "{action} needs a description"
                );
            }
        });
    }

    /// An editable shortcut says which context it is live in.
    ///
    /// `canonical_context` already knows, and the rendering turns `when` into the "Active when: …"
    /// sentence, but only the parameterized rows were built with it: the other two construction
    /// sites passed `None`, so all 68 normal rows — which is every editable row — printed nothing.
    #[gpui::test]
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
                    // A globally bound command has no context to name, so its row correctly says
                    // nothing. The rendering has to agree.
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
                // The sentence the row prints has to read in words, not as a predicate: the raw
                // section is not something a person can act on.
                let sentence = keyboard_context_description(context);
                assert_ne!(
                    sentence, context,
                    "{} prints a predicate instead of a surface",
                    command.action_name
                );
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
            // A globally bound row is the one shape that prints no context, and the Command
            // Palette toggle is it.
            let palette = commands
                .iter()
                .find(|command| command.action_name == "k8s_shell::ToggleCommandPalette")
                .expect("ToggleCommandPalette is listed");
            assert!(palette.when.is_none());
            assert!(palette.context.is_none());
        });
    }

    #[gpui::test]
    fn a_newly_bound_inspector_shortcut_records_and_clears(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("default keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        view.update(cx, |view, cx| {
            view.select_category(SettingsCategory::Keyboard, cx)
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
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

        // Edit: the action has no user file entry, so recording writes the Inspector section and
        // releases the built-in key rather than doing nothing.
        let recorded = gpui::Keystroke::parse("ctrl-alt-f9").expect("test keystroke");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.start_recording(command.clone(), window, cx);
                view.handle_recording_keystroke(&recorded, window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |view, _| view.error.clone()), None);
        let saved = cx.read(|cx| keymap::status(cx).user_source.clone());
        let saved = saved.expect("recording a shortcut writes the user keymap source");
        assert!(saved.contains("ctrl-alt-f9"), "{saved}");
        let recorded_chord = cx.read(|cx| command_chord(&command, cx));
        assert_eq!(recorded_chord.as_deref(), Some("ctrl-alt-f9"));

        // Clear: the row stops advertising a key, and the user keymap carries a real unbind for
        // the key the built-in keymap owned.
        view.update(cx, |view, cx| view.clear_binding(&command, cx));
        assert_eq!(view.read_with(cx, |view, _| view.error.clone()), None);
        assert_eq!(cx.read(|cx| command_chord(&command, cx)), None);
        let cleared = cx.read(|cx| keymap::status(cx).user_source.clone());
        let cleared = cleared.expect("clearing a shortcut keeps the user keymap source");
        assert!(cleared.contains("\"unbind\""), "{cleared}");
        assert!(cleared.contains(action), "{cleared}");
        discard_queued_keymap_write();
    }

    #[test]
    fn restore_defaults_confirmation_names_what_it_deletes() {
        let text = restore_confirmation_text("/home/dev/.config/k8s-gpui/keymap.json");
        assert!(text.contains("Restore Defaults"));
        assert!(
            text.contains("/home/dev/.config/k8s-gpui/keymap.json"),
            "{text}"
        );
        assert!(text.contains("overrides"), "{text}");
        assert!(RESTORE_DEFAULTS_DESCRIPTION.contains("user keymap file"));
    }

    fn init_disk_cache_test(cx: &mut TestAppContext, enabled: bool) {
        cx.update(|cx| {
            ::settings::init(cx);
            settings::set_test_settings_memory(&crate::settings::UserSettings {
                disk_cache: Some(enabled),
                ..Default::default()
            });
            theme_settings::init(LoadThemes::JustBase, cx);
        });
    }

    #[gpui::test]
    fn disk_cache_initializes_from_memory_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, false);
        settings::reset_test_load_count();
        let (_view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        cx.update(|_, cx| assert!(!settings::disk_cache_enabled_from_app(cx)));
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui::test]
    fn disk_cache_toggle_success_updates_cached_state_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, true);
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));

        view.update(cx, |view, cx| {
            settings::set_test_disk_cache(cx, false);
            view.apply_disk_cache_result(Ok(()), cx);
            assert!(!settings::disk_cache_enabled_from_app(cx));
            assert!(view.error.is_none());
        });
        settings::reset_test_load_count();
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui::test]
    fn disk_cache_toggle_failure_preserves_cached_state_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, true);
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));

        view.update(cx, |view, cx| {
            // The `SettingsStore` global must stay installed. `remove_global` pushes a
            // `NotifyGlobalObservers` effect, and the observer that `theme_settings::init`
            // registered calls `ThemeSettings::get_global`, which panics on the missing
            // global when the effect flushes. A value the store rejects fails the same save
            // synchronously, keeps the cached `DiskCache` global, and touches no file.
            let result = settings::update(cx, |settings| {
                settings.disk_cache = Some(false);
                settings.extra.insert(
                    "buffer_font_size".to_owned(),
                    serde_json::Value::String("bad".to_owned()),
                );
            });
            assert!(result.is_err(), "fixture must fail the save");
            view.apply_disk_cache_result(result, cx);
            assert!(settings::disk_cache_enabled_from_app(cx));
            assert!(view.error.is_some());
        });
        settings::reset_test_load_count();
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert_eq!(settings::test_load_count(), 0);
    }

    #[gpui::test]
    fn external_disk_cache_update_refreshes_render_without_file_io(cx: &mut TestAppContext) {
        init_disk_cache_test(cx, false);
        let (view, cx) = cx.add_window_view(|_, cx| SettingsView::new(cx));
        cx.run_until_parked();
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
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        cx.update(|_, cx| assert!(settings::disk_cache_enabled_from_app(cx)));
        assert_eq!(settings::test_load_count(), 0);
    }
}
