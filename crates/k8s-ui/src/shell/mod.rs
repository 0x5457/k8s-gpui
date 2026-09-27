//! Application shell layout, state, actions, dialogs, and command palette.
//!
//! [`Shell`] owns layout and shared state. Panel rendering lives in `panels`, tree data in
//! `tree`, and command definitions in `commands`.
//!
//! The chrome the shell draws itself — the splitter, the panel budget, the modal focus ring —
//! is app state with no gpui-kit equivalent, so it stays here. Everything a component already
//! covers (labels, buttons, icons, tooltips, keycaps, menus, empty states) is called from
//! `gpui_kit::component` rather than reimplemented.

pub mod commands;
mod hotbar;
mod panels;
mod status_bar;
mod tree;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable, Size, ThemeRegistry, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::private::serde as private_serde;
use gpui_kit::{
    Action, AnyElement, AnyView, App, ClipboardItem, Context, CursorStyle, Entity, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, Role, ScrollHandle, SharedString, Styled,
    Subscription, Task, TextRun, WeakEntity, Window, actions, div, px,
};
pub use k8s_actions::{
    CheckForUpdates, Copy, Cut, Paste, Redo, RefreshView, RestartToUpdate, SelectAll, Undo,
};
use k8s_core::cluster::{ClusterId, ClusterRegistry, Health};
use k8s_core::cluster_data::{ClusterDataSource, data_source_for_cluster};
use k8s_core::discovery::{ResourceCatalog, ResourceEntry};
use k8s_core::helm::{Helm, HelmError};
use k8s_core::hotbar::Hotbar;
use k8s_core::latency::LatencyTier;
use k8s_core::machines::{
    ConnectionEffect as CoreConnectionEffect, ConnectionEvent as CoreConnectionEvent,
    ConnectionMachine as CoreConnectionMachine, ConnectionState as CoreConnectionState,
    HotbarEffect, HotbarEvent, HotbarMachine, SearchHit,
};
use statig::blocking::StateMachine;
use statig::prelude::IntoStateMachineExt as _;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use self::{
    commands::{
        Command, CommandRun, ShellHandler, UPDATER_UNAVAILABLE_REASON,
        demo_commands_with_capabilities, filter_commands_for_scope,
    },
    panels::PickerKind,
    tree::{ResourceTree, TreeRow, TreeRowKind},
};
#[cfg(test)]
use crate::table_view::Subscription as ResourceSubscription;
#[cfg(test)]
use crate::table_view::{ResourceSource, SourceEvent};
use crate::{
    design, keymap,
    keymap::KeymapPreset,
    panels::ApplyVerdict,
    panels::forwards::{ForwardsView, NewForwardCallback},
    panels::helm::{HelmCapability, HelmServices, HelmView},
    panels::inspector_data::{ApplyRequest, InspectorSession},
    panels::logs::LogRequest,
    panels::metrics::{MetricsHandle, MetricsProbeState},
    panels::overview::{OverviewHandle, OverviewView},
    panels::search::{SearchExecutor, SearchResultAction, SearchView},
    panels::settings_view::{Capability, SettingsView},
    panels::terminal::{ForwardRequest, TerminalKind, TerminalServices, parse_local_port},
    panels::{
        DockPanel, ForwardSummary, HelmAction, InspectorPanel, InspectorSelection, InspectorTab,
        ObjectRef,
    },
    session::{InspectorBinding, InspectorUpdate, ServiceAccountTarget},
    table_view::{
        CatalogHandle, ClusterCache, ClusterHandle, ClusterSession, DeleteTarget, EditYaml,
        ExecTarget, ObjectOps, OpenDetails, PodsView, PortForwardTarget, ResourceSpec, Row,
        ScaleTarget, SourceFactory, TextInput,
    },
    update::{StartupNotice, UpdateActions, UpdatePhase, UpdateUiState},
};
#[cfg(test)]
use k8s_core::controller::{StoreEvent, StoreOp};
use kube_core::{ApiResource, DynamicObject, GroupVersionKind};

actions!(
    k8s_hotbar,
    [
        /// Toggle the Hotbar rail.
        ToggleHotbar
    ]
);

actions!(
    k8s_shell,
    [
        ToggleCommandPalette,
        OpenContextSwitcher,
        OpenNamespaceSwitcher,
        OpenResourceKindSwitcher,
        OpenOverview,
        OpenForwards,
        ApplyYaml,
        OpenLogs,
        OpenEvents,
        ExecSelection,
        PortForwardSelection,
        RestartSelection,
        ScaleSelection,
        ReloadKeymap,
        ToggleLeftPanel,
        ToggleRightPanel,
        ToggleDock,
        Dismiss,
        CloseTab,
        CloseOtherTabs,
        CloseAllTabs,
        TogglePinTab,
        MoveTabLeft,
        MoveTabRight,
        FocusYaml,
        NextTab,
        PreviousTab,
        FocusNext,
        FocusPrevious,
        DescribeSelection,
        PauseUpdates,
        ResumeUpdates,
        CopySelectedPodName,
        ToggleTheme,
        UseLightTheme,
        UseDarkTheme,
        UseSystemTheme,
        ToggleNotifications,
        SearchResources,
        ReloadKubeconfigs,
        OpenServiceAccount
    ]
);

#[derive(Clone, Copy)]
enum UpdateActionKind {
    Check,
    Retry,
    Restart,
}

/// Select a tab by index.
#[derive(
    Clone,
    PartialEq,
    private_serde::Deserialize,
    gpui_kit::private::schemars::JsonSchema,
    gpui_kit::Action,
)]
#[action(namespace = k8s_shell)]
#[serde(crate = "gpui_kit::private::serde", deny_unknown_fields)]
#[schemars(crate = "gpui_kit::private::schemars")]
pub struct SwitchTab {
    pub index: usize,
}

#[derive(
    Clone,
    PartialEq,
    private_serde::Deserialize,
    gpui_kit::private::schemars::JsonSchema,
    gpui_kit::Action,
)]
#[action(namespace = k8s_shell)]
#[serde(crate = "gpui_kit::private::serde", deny_unknown_fields)]
#[schemars(crate = "gpui_kit::private::schemars")]
pub struct UseTheme {
    pub name: String,
}

#[derive(
    Clone,
    PartialEq,
    private_serde::Deserialize,
    gpui_kit::private::schemars::JsonSchema,
    gpui_kit::Action,
)]
#[action(namespace = k8s_shell)]
#[serde(crate = "gpui_kit::private::serde", deny_unknown_fields)]
#[schemars(crate = "gpui_kit::private::schemars")]
pub struct UseKeymapPreset {
    pub preset: String,
}

/// Switch to a cluster in the active Hotbar bank slot.
#[derive(
    Clone,
    PartialEq,
    private_serde::Deserialize,
    gpui_kit::private::schemars::JsonSchema,
    gpui_kit::Action,
)]
#[action(namespace = k8s_hotbar)]
#[serde(crate = "gpui_kit::private::serde", deny_unknown_fields)]
#[schemars(crate = "gpui_kit::private::schemars")]
pub struct SwitchCluster {
    pub slot: usize,
}

/// Switch the active Hotbar bank.
#[derive(
    Clone,
    PartialEq,
    private_serde::Deserialize,
    gpui_kit::private::schemars::JsonSchema,
    gpui_kit::Action,
)]
#[action(namespace = k8s_hotbar)]
#[serde(crate = "gpui_kit::private::serde", deny_unknown_fields)]
#[schemars(crate = "gpui_kit::private::schemars")]
pub struct SwitchBank {
    pub index: usize,
}

#[cfg(test)]
mod tests;

/// `#[gpui_kit::test]` brings the app up with the component layer already initialized, so
/// a test only has to add the keymap and the design bridge the shell reads.
#[cfg(test)]
fn init_app(cx: &mut gpui_kit::TestAppContext) {
    crate::init_ui(cx);
    cx.update(|cx| design::set_appearance(cx, design::Appearance::Dark));
}

/// Label for the namespace scope that includes every namespace.
///
/// This string is both the scope's identity and the label a reader sees, so it
/// has to be the sentence-case form the search panel and the pickers use, and the
/// comparisons against it have to move with it. The scope leaves the shell as
/// `None` before it reaches a cluster call, so this label is never a wire value.
const ALL_NAMESPACES: &str = "All namespaces";

/// How long a toast that reports something that went well stays on screen.
///
/// `DESIGN.md` §3.2 asks a non-token value to be justified in a component contract, and the
/// justification is the reason it is not a motion token: this is a *disappearance*, so it must
/// be long enough to read and short enough that a stream of confirmations does not stack up.
/// `SLOW` and `FAST` are for a transition the reader watches, which this is not.
///
/// An error toast never expires. `alerts.md` treats a failure as something the reader has to
/// deal with, and a message that removes itself before it can be acted on is a message that
/// cannot be acted on.
const TOAST_DURATION: Duration = Duration::from_millis(3_000);

/// Stable reason used while the startup session loads.
pub const STARTUP_LOADING_REASON: &str = "Loading kubeconfig…";

/// The one way out of a toast, when the toast knows one.
///
/// `alerts.md` asks an alert for "essential information **and useful actions**", and
/// `feedback.md` asks for a reason the reader can act on. A toast that can only be dismissed is
/// a notification, so the shell hands the recovery to the message that needs it instead of
/// making the reader find the control that performs it.
#[derive(Clone)]
pub(super) struct ToastAction {
    /// Button label. A verb, because the button does the thing: `Retry`, `Undo`, `View`.
    pub label: &'static str,
    pub run: ShellHandler,
}

impl std::fmt::Debug for ToastAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The handler is a closure, so only its label is printable.
        f.debug_struct("ToastAction")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// Current toast message.
#[derive(Clone, Debug)]
pub(super) struct Toast {
    pub message: SharedString,
    pub severity: design::Severity,
    /// The recovery, when the caller knows one.
    pub action: Option<ToastAction>,
}

/// Persistent toast history entry.
#[derive(Clone, Debug)]
pub(super) struct Notification {
    pub id: u64,
    pub message: SharedString,
    pub severity: design::Severity,
    pub detail: Option<String>,
    pub at: std::time::Instant,
    pub expanded: bool,
}

/// Maximum number of retained notifications.
const NOTIFICATION_CAPACITY: usize = 100;
/// Tab order of the notification header action, before the rows.
const NOTIFICATION_CLEAR_TAB_INDEX: isize = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StatusPanel {
    #[default]
    None,
    Notifications,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StatusSummary {
    pub operations: usize,
    pub sessions: usize,
    pub errors: usize,
    pub notifications: usize,
    /// The notifications that report something the reader has to act on.
    ///
    /// The raw total counts every entry, and the app raises an Info confirmation for every
    /// successful Settings switch, so the total is mostly the reader's own actions echoed back.
    /// This is the same rule the notification centre sorts with (`status_bar`), so the two places
    /// that answer "is anything wrong" answer it the same way. The centre counts its own folded
    /// rows, so the two differ by the duplicates it collapses; the status bar owns
    /// `collapsed_notifications` and the top bar's number should read through it once that is
    /// reachable from here.
    pub active_notifications: usize,
    pub port_forwards: ForwardSummary,
    /// Live, paused, or reconnecting log stream. The Dock is not rendered while it is collapsed.
    pub log_status: Option<&'static str>,
}

fn error_notification_count(notifications: &[Notification]) -> usize {
    notifications
        .iter()
        .filter(|notification| notification.severity == design::Severity::Error)
        .count()
}

/// Whether a notification reports something the reader still has to deal with.
///
/// `status_bar::notification_is_active_incident` is the same rule, and the notification centre
/// orders and counts itself with it. That copy is module-private, so the two cannot be compared
/// from a test; the top bar and the status bar read the count this returns, so they cannot drift
/// from each other, and the shared rule is the one thing left to lift out of `status_bar`.
fn notification_is_active_incident(notification: &Notification) -> bool {
    notification.detail.is_some()
        || matches!(
            notification.severity,
            design::Severity::Error | design::Severity::Warning
        )
}

fn active_notification_count(notifications: &[Notification]) -> usize {
    notifications
        .iter()
        .filter(|notification| notification_is_active_incident(notification))
        .count()
}

/// Input rule for the replica count field, shared by the hint and every error.
const REPLICA_COUNT_RULE: &str = "Replica count must be a whole number of 0 or more.";

/// Recovery sentence shown when a port forward does not start.
const PORT_FORWARD_RECOVERY: &str =
    "The port forward did not start. Check the container port, then try again.";

/// Dialog focus indexes of the port-forward dialog: the remote field, the local field, Cancel, and
/// Start Port Forward. Every other dialog keeps the input at 0, Cancel at 1, and confirm at 2.
const PORT_FORWARD_LOCAL_FOCUS: usize = 1;
const PORT_FORWARD_CANCEL_FOCUS: usize = 2;
const PORT_FORWARD_CONFIRM_FOCUS: usize = 3;

/// Recovery sentence shown when the resource catalog cannot be refreshed.
const CATALOG_REFRESH_RECOVERY: &str = "The app did not refresh resources. Try Refresh View again.";

/// Modal confirmation and input dialogs.
#[derive(Clone, Copy)]
enum DialogInputKind {
    Scale,
    PortForward,
    PortForwardLocal,
    BankName,
    HelmChartReference,
}

impl DialogInputKind {
    fn spec(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        usize,
    ) {
        match self {
            Self::Scale => (
                "Replica Count",
                "Replica Count",
                "Enter a whole number of 0 or more.",
                "Clear Replica Count",
                9,
            ),
            Self::PortForward => (
                "80",
                "Remote Port",
                "Enter a container port from 1 through 65535.",
                "Clear Remote Port",
                5,
            ),
            Self::PortForwardLocal => (
                "8080",
                "Local Port",
                "Leave empty to assign a free local port.",
                "Clear Local Port",
                5,
            ),
            Self::BankName => (
                "Bank Name",
                "Bank Name",
                "Name this group of contexts, such as production or staging.",
                "Clear Bank Name",
                32,
            ),
            Self::HelmChartReference => (
                "repo/chart or /path/to/chart",
                "Chart Reference",
                "Enter repo/chart or a local chart path. Helm resolves the chart version. The current release values stay the same.",
                "Clear Chart Reference",
                512,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabCloseRequest {
    One(usize),
    Others(usize),
    All,
}

enum Dialog {
    ConfirmDelete {
        target: DeleteTarget,
        /// Every object the confirmation covers, so a multi-row delete names its real count.
        objects: Vec<ObjectRef>,
        title: SharedString,
        detail: SharedString,
        /// Number of selected objects.
        count: usize,
        view: Entity<PodsView>,
    },
    Scale {
        target: ScaleTarget,
        view: Entity<PodsView>,
        input: Entity<TextInput>,
        error: Option<&'static str>,
    },
    /// Select a container for a multi-container Pod.
    Exec {
        target: ExecTarget,
        selected: usize,
    },
    /// Remote and local ports for a port-forward request.
    PortForward {
        target: PortForwardTarget,
        /// Remote container port.
        input: Entity<TextInput>,
        /// Local port to ask for. Empty text leaves the choice to the system.
        local: Entity<TextInput>,
        /// Declared `containerPort` the user picked, when the buttons chose one.
        selected: Option<u16>,
        error: Option<String>,
    },
    HelmConfirm {
        action: HelmAction,
        view: Entity<HelmView>,
        input: Option<Entity<TextInput>>,
        error: Option<String>,
    },
    /// Create or rename a Hotbar bank.
    HotbarBankName {
        index: Option<usize>,
        input: Entity<TextInput>,
        error: Option<String>,
    },
    /// Confirm removal of a Hotbar bank and its slots.
    HotbarRemove {
        index: usize,
        name: SharedString,
    },
    ConfirmTabClose {
        request: TabCloseRequest,
    },
}

/// Parse a non-negative replica count.
fn parse_replicas(input: &str) -> Result<i32, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(REPLICA_COUNT_RULE);
    }
    if !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(REPLICA_COUNT_RULE);
    }
    trimmed
        .parse::<i32>()
        .map_err(|_| "Replica count must be a whole number from 0 to 2147483647.")
}

/// Parse a port from 1 through 65535.
fn parse_port(input: &str) -> Result<u16, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a port number.");
    }
    match trimmed.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err("Port must be a number from 1 through 65535."),
    }
}

fn parse_chart_reference(input: &str) -> Result<String, &'static str> {
    let chart = input.trim();
    if chart.is_empty() {
        return Err("Enter a chart reference.");
    }
    if chart.starts_with('-') {
        return Err("Do not start the chart reference with '-'.");
    }
    if chart.chars().any(char::is_control) {
        return Err("Do not include line breaks in the chart reference.");
    }
    let path = Path::new(chart);
    let explicit_path = path.is_absolute()
        || path.exists()
        || chart.starts_with("./")
        || chart.starts_with("../")
        || chart.contains('\\');
    if !explicit_path && !chart.contains('/') {
        return Err("Enter repo/chart or a local chart path.");
    }
    Ok(chart.to_owned())
}

fn dialog_input_index_at(
    text: &str,
    position: gpui_kit::Point<Pixels>,
    bounds: gpui_kit::Bounds<Pixels>,
    window: &Window,
    cx: &App,
) -> usize {
    if text.is_empty() {
        return 0;
    }
    let font = window.text_style().font();
    let font_size = design::text::BODY;
    let text_left = bounds.left() + design::border::LINE + design::space::SM;
    let mut previous_width = px(0.0);
    for (index, (byte_index, ch)) in text.char_indices().enumerate() {
        let end = byte_index + ch.len_utf8();
        let run = TextRun {
            len: end,
            font: font.clone(),
            color: design::colors(cx).text,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let width = window
            .text_system()
            .shape_line(
                SharedString::from(text[..end].to_owned()),
                font_size,
                &[run],
                None,
            )
            .width();
        if position.x <= text_left + (previous_width + width) / 2.0 {
            return index;
        }
        previous_width = width;
    }
    text.chars().count()
}

/// Default dialog width for short modal tasks.
const DIALOG_WIDTH: f32 = 420.0;

fn dialog_width(viewport_width: f32) -> f32 {
    (viewport_width - f32::from(design::space::XXL))
        .min(DIALOG_WIDTH)
        .max(f32::from(design::size::CONTROL))
}

fn is_text_entry_keystroke(keystroke: &Keystroke) -> bool {
    keystroke
        .key_char
        .as_deref()
        .is_some_and(|text| !text.chars().all(|character| character.is_control()))
        && !keystroke.modifiers.secondary()
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
}

/// The prefix typed since the last jump, and when the last keystroke landed.
///
/// `UI-SPEC` §9.3 asks for type-ahead in the table and the sidebar, and the table has it
/// (`table_view::view::typeahead`). The sidebar did not, which is the worse half to be
/// missing: the sidebar is where a reader starts, and a reader who types `deploy` there and
/// nothing happens has to learn that this one list does not answer to the keyboard.
///
/// This is deliberately the same shape as the table's, because a reader who has learned one
/// has to guess the other:
///
/// * **The same window.** [`TYPE_AHEAD_RESET`] is the table's 900ms, so `c`, pause, `c`
///   starts a new `c` in both lists at the same speed.
/// * **The same match.** Per name segment, not per whole string: a reader types `core` and
///   means `coredns-559f6c778d-hzwzg`, not a row whose first four characters happen to be
///   `core`.
/// * **The same cycle.** The first match after the current row, wrapping, so a second press
///   of the same prefix walks a run of matches instead of parking on the first.
#[derive(Default)]
struct TypeAhead {
    prefix: String,
    last_typed: Option<std::time::Instant>,
}

/// How long a type-ahead prefix survives without a keystroke.
const TYPE_AHEAD_RESET: Duration = Duration::from_millis(900);

impl TypeAhead {
    /// Appends one keystroke's worth of prefix, restarting it when the last one is too old.
    fn push(&mut self, keystroke: &str, now: std::time::Instant) {
        let expired = self
            .last_typed
            .is_none_or(|last| now.duration_since(last) > TYPE_AHEAD_RESET);
        if expired {
            self.prefix.clear();
        }
        self.prefix.push_str(&keystroke.to_lowercase());
        self.last_typed = Some(now);
    }

    fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// Whether a label answers to a typed prefix, per name segment.
///
/// A segment is a `-`, `_` or `.` boundary, because those are the three characters a
/// Kubernetes name is allowed to join its parts with, and they are the three a reader
/// mentally uses to say "the `dns` in `coredns`".
fn label_matches_prefix(label: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let label = label.to_lowercase();
    label.starts_with(prefix)
        || label
            .split(['-', '_', '.'])
            .any(|segment| segment.starts_with(prefix))
}

/// The row a type-ahead prefix lands on, given the row the reader is on.
///
/// The first match after `current`, wrapping: wrapping is what makes a second press of the
/// same prefix step to the next one, and starting after the current row is what makes a
/// table with nothing selected land on its first match rather than on nothing.
fn type_ahead_index(labels: &[&str], prefix: &str, current: usize) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    let first = labels
        .iter()
        .position(|label| label_matches_prefix(label, prefix))?;
    let from = current.saturating_add(1).min(labels.len() - 1);
    labels
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, label)| label_matches_prefix(label, prefix))
        .map(|(index, _)| index)
        .or(Some(first))
}

fn is_text_edit_shortcut(keystroke: &Keystroke) -> bool {
    keystroke.modifiers.secondary()
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
        && !keystroke.modifiers.function
        && matches!(keystroke.key.as_str(), "a" | "c" | "v" | "x" | "z")
}

const DOCK_HEIGHT_DEFAULT: f32 = 220.0;
/// The narrowest window the shell lays out for. `design::size::WINDOW_MIN` is the same number
/// the app binary hands the window manager, so the layout floor and the window floor cannot be
/// two different numbers.
const MIN_LAYOUT_WIDTH: f32 = design::size::WINDOW_MIN.0;

/// The client width below which the window is *compact*: the title bar sheds its secondary
/// clusters and the status bar sheds its optional readouts.
///
/// One number, and it is [`MIN_LAYOUT_WIDTH`] — the same token the sidebar's own gate reads.
///
/// That is not a tautology. `MIN_LAYOUT_WIDTH` is a **window** width: it is what `main.rs` hands the
/// window manager. Every breakpoint the shell owns is a **client** width, because that is what
/// `viewport_size()` reports and therefore what every layout decision is made against. On a
/// decorated window the two differ by the frame — 19px on the machine this was last measured on —
/// so a window resting on its own legal minimum arrives 19px *below* the floor, and the band between
/// the two is a band a real window spends time in. Three unrelated literals would mean three
/// windows in which one region has shed and another has not, which is worse than either having
/// shed; deriving all of them from the floor makes the band one band.
///
/// The sidebar's own gate already reads the same number, and the answer for that band is a title-bar
/// switch to the searchable kind list rather than a glyph rail (`shell/panels.rs`).
pub(super) fn chrome_compact_width() -> f32 {
    MIN_LAYOUT_WIDTH
}

const DIVIDER_KEY_STEP: f32 = 8.0;

/// `UI-SPEC` §11.2, the width rows.
///
/// | window | sidebar | inspector | centre |
/// |---|---|---|---|
/// | ≥ 1440 | 236 | 352 docked | 852+ |
/// | 1200–1439 | 236 | 280 docked | 684+ |
/// | 1000–1199 | 180 | floating | 820+ |
/// | 960–999 | 48 rail | floating | 912+ |
///
/// The numbers the design already states live in `design::size`; the two that do not are here
/// with the row they came from, because a breakpoint nobody can name is a breakpoint that
/// moves the next time someone renumbers a token.
const INSPECTOR_DOCK_NARROW: f32 = 280.0;
/// Below this the Inspector stops taking width from the centre and floats over it.
const INSPECTOR_FLOAT_BELOW: f32 = design::size::INSPECTOR_FLOAT_BELOW;
/// Below this the docked Inspector narrows to [`INSPECTOR_DOCK_NARROW`].
const INSPECTOR_DOCK_NARROW_BELOW: f32 = 1200.0;
/// The window width below which the toolbar starts taking room away from the cluster and
/// namespace names.
///
/// Named for what it used to decide and kept because `shell/panels.rs` still asks it that
/// question. It is no longer the Inspector's breakpoint — those are
/// [`INSPECTOR_FLOAT_BELOW`] and [`INSPECTOR_DOCK_NARROW_BELOW`] — and the name is the last
/// thing about it that is wrong.
const INSPECTOR_LAYOUT_BREAKPOINT: f32 = 1200.0;
const INSPECTOR_WIDTH_HINT: &str = "Widen the window to show the Inspector.";
/// Label for the Inspector toggle when the window is too narrow to show it.
const INSPECTOR_COMPACT_LABEL: &str = "Inspector Unavailable. Widen the window to show it.";
/// How long the window waits after the last change before writing `layout.json`.
const LAYOUT_SAVE_DELAY: Duration = Duration::from_millis(400);

/// The sidebar's resting width, and the width a window with no remembered one opens at.
///
/// `UI-SPEC` §11.1 says 236 in a range of 180–360, and it says it in `design::size` as
/// `SIDEBAR_DEFAULT`. The shell used to carry its own `232`, which is not a number the design
/// contains: §11.1's centre-column arithmetic is computed from 236, and a sidebar four pixels
/// narrower than the budget makes every one of those rows four pixels optimistic.
fn left_width_default() -> f32 {
    f32::from(design::size::SIDEBAR_DEFAULT)
}

/// The docked Inspector's resting width, from the same row. Was 336, for the same reason.
fn right_width_default() -> f32 {
    f32::from(design::size::INSPECTOR_DEFAULT)
}

/// The extra ceiling the docked Inspector takes at this window width, over its own range.
///
/// `None` means its range is the whole story, which is the case at and above
/// [`INSPECTOR_DOCK_NARROW_BELOW`]: a reader who wants a 400-wide Inspector still has 684px
/// of table at 1200 and 1152 at 2000, and `INSPECTOR_MAX` already says that is allowed.
fn inspector_width_ceiling(width: f32) -> Option<f32> {
    (width < INSPECTOR_DOCK_NARROW_BELOW).then_some(INSPECTOR_DOCK_NARROW)
}

fn left_width_min() -> f32 {
    f32::from(design::size::SIDEBAR_MIN)
}

fn left_width_limit() -> f32 {
    f32::from(design::size::SIDEBAR_MAX)
}

fn right_width_min() -> f32 {
    f32::from(design::size::INSPECTOR_MIN)
}

fn right_width_limit() -> f32 {
    f32::from(design::size::INSPECTOR_MAX)
}

fn dock_height_min() -> f32 {
    f32::from(design::size::DOCK_MIN)
}

fn dock_height_limit() -> f32 {
    f32::from(design::size::DOCK_MAX)
}

/// The status bar's height, and the height every item in it is drawn at.
///
/// `UI-SPEC` §4.17 fixes this at 24px. The bar used to take
/// `max(STATUS_BAR, CONTROL)`, and since `CONTROL` is 28 that quietly overrode the
/// spec on every launch: the one surface that is always on screen and never
/// changes was 4px taller than the design says. The items are text readouts with
/// one focusable link, and none of them needs a full control's height, so the bar
/// gets the number it was always supposed to have.
pub(super) fn status_bar_height() -> f32 {
    f32::from(design::size::STATUS_BAR)
}

/// The height of one item drawn on the bar, which is the bar minus the 1px rule on top of it.
///
/// `UI-SPEC.md` §4.17 fixes the bar at 24px *including* a 1px `border.subtle`, so the row inside
/// it is 23. Every item was drawn at the full 24 and centred in the 23, which put half a pixel
/// over the rule at the top and half a pixel past the window edge at the bottom: the one
/// focusable control on the strip was the one control that did not fit its own bar.
pub(super) fn status_bar_item_height() -> f32 {
    status_bar_height() - 1.0
}

fn hotbar_width() -> f32 {
    f32::from(design::size::HOTBAR_RAIL)
}

/// Panel geometry used to move a divider with the pointer.
#[derive(Clone, Copy)]
struct PanelGeometry {
    viewport_width: f32,
    viewport_height: f32,
    status_height: f32,
    hotbar_width: f32,
    left_width: f32,
    right_width: f32,
    dock_height: f32,
}

impl PanelGeometry {
    fn new(
        viewport_width: f32,
        viewport_height: f32,
        status_height: f32,
        hotbar_width: f32,
        left_width: f32,
        right_width: f32,
        dock_height: f32,
    ) -> Self {
        Self {
            viewport_width,
            viewport_height,
            status_height,
            hotbar_width,
            left_width,
            right_width,
            dock_height,
        }
    }

    /// Pointer axis that moves the divider.
    fn pointer_coordinate(target: DragTarget, position: gpui_kit::Point<gpui_kit::Pixels>) -> f32 {
        match target {
            DragTarget::Dock => f32::from(position.y),
            DragTarget::Left | DragTarget::Right => f32::from(position.x),
        }
    }

    /// Window coordinate of the panel edge the divider sits on.
    fn edge(&self, target: DragTarget) -> f32 {
        match target {
            DragTarget::Left => self.hotbar_width + self.left_width,
            DragTarget::Right => self.viewport_width - self.right_width,
            DragTarget::Dock => self.viewport_height - self.status_height - self.dock_height,
        }
    }

    /// Panel size for a divider edge on the pointer axis.
    fn panel_size(&self, target: DragTarget, edge: f32) -> f32 {
        match target {
            DragTarget::Left => edge - self.hotbar_width,
            DragTarget::Right => self.viewport_width - edge,
            DragTarget::Dock => self.viewport_height - self.status_height - edge,
        }
    }

    /// Panel size for a pointer that keeps the grab offset from the press.
    ///
    /// `grab` is the distance from the panel edge to the pointer when the press landed,
    /// so `pointer - grab` is the edge the pointer holds and a pointer that has not
    /// moved reproduces the edge of the press.
    fn dragged_panel_size(&self, target: DragTarget, pointer: f32, grab: f32) -> f32 {
        self.panel_size(target, pointer - grab)
    }
}

/// Divider drag that keeps the offset between the pointer and the panel edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DividerDrag {
    target: DragTarget,
    grab: f32,
}

fn dock_height_max_with_strips(
    viewport_height: f32,
    warning_strip_visible: bool,
    update_strip_visible: bool,
) -> f32 {
    let available = viewport_height
        - f32::from(design::size::TOOLBAR)
        - status_bar_height()
        - f32::from(design::border::HIT)
        - f32::from(design::size::MAIN_CONTENT_MIN)
        - if warning_strip_visible {
            f32::from(design::size::ROW)
        } else {
            0.0
        }
        - if update_strip_visible {
            f32::from(design::size::UPDATE_STRIP)
        } else {
            0.0
        };
    available.clamp(dock_height_min(), dock_height_limit())
}

fn min_full_width(sidebar_visible: bool, inspector_visible: bool, hotbar_visible: bool) -> f32 {
    let divider = f32::from(design::border::HIT);
    f32::from(design::size::CENTER_MIN)
        + if sidebar_visible { divider } else { 0.0 }
        + if inspector_visible { divider } else { 0.0 }
        + if hotbar_visible { hotbar_width() } else { 0.0 }
}

fn left_width_max_with_hotbar(
    viewport_width: f32,
    right_width: f32,
    inspector_visible: bool,
    hotbar_visible: bool,
) -> f32 {
    let reserved = min_full_width(true, inspector_visible, hotbar_visible)
        + if inspector_visible { right_width } else { 0.0 };
    (viewport_width - reserved).clamp(left_width_min(), left_width_limit())
}

fn right_width_max_with_hotbar(
    viewport_width: f32,
    left_width: f32,
    sidebar_visible: bool,
    hotbar_visible: bool,
) -> f32 {
    let reserved = min_full_width(sidebar_visible, true, hotbar_visible)
        + if sidebar_visible { left_width } else { 0.0 };
    (viewport_width - reserved).clamp(right_width_min(), right_width_limit())
}

/// The two panel widths this window width allows, after the reader's own choice and the
/// drag clamps.
///
/// The breakpoint budgets are ceilings, not replacements: a reader who dragged the Inspector
/// to 420 on a wide window and then narrowed it gets 280 until they widen it again, and gets
/// 420 back — not 280 forever. Overwriting the remembered width with the temporary one is how
/// a layout rule ends up destroying the choice it was meant to serve.
fn constrained_panel_widths_with_hotbar(
    viewport_width: f32,
    sidebar_visible: bool,
    inspector_visible: bool,
    hotbar_visible: bool,
    left_width: f32,
    right_width: f32,
) -> (f32, f32) {
    let left = if sidebar_visible {
        left_width.clamp(
            left_width_min(),
            left_width_max_with_hotbar(
                viewport_width,
                right_width,
                inspector_visible,
                hotbar_visible,
            ),
        )
    } else {
        left_width.clamp(left_width_min(), left_width_limit())
    };
    let ceiling = inspector_width_ceiling(viewport_width).unwrap_or(right_width_limit());
    let right = if inspector_visible {
        right_width.clamp(
            right_width_min(),
            right_width_max_with_hotbar(viewport_width, left, sidebar_visible, hotbar_visible)
                .min(ceiling),
        )
    } else {
        right_width.clamp(right_width_min(), ceiling)
    };
    (left, right)
}

/// How the Inspector meets the window it is in.
///
/// `UI-SPEC` §11.2 asks for three states, not two, and the third is the one that used to be
/// missing: a docked panel that *disappears* below a breakpoint takes the Inspector away
/// exactly when the window is small, which is when a reader with one monitor and a laptop
/// screen most needs to see what they just selected. So below [`INSPECTOR_FLOAT_BELOW`] the
/// panel floats over the centre's right edge instead, the centre keeps every pixel it had,
/// and selection still previews because nothing about following the selection changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectorLayout {
    Docked,
    Floating,
    Closed,
}

impl InspectorLayout {
    /// Whether the panel takes width from the centre, which is the question the width
    /// arithmetic and the row layout both ask.
    fn takes_width(self) -> bool {
        matches!(self, Self::Docked)
    }
}

/// Whether this window's width can show the Inspector at all.
///
/// `shell/panels.rs` asks this to decide whether the toolbar's Inspector switch is available,
/// and the answer is now yes for every width the window manager will hand this app. The
/// Inspector floats below [`INSPECTOR_FLOAT_BELOW`] rather than disappearing, so there is no
/// width at which the control would be a switch that does nothing — which is the failure
/// this function used to cause between 1000 and 1200.
pub(super) fn inspector_available(width: f32) -> bool {
    width >= MIN_LAYOUT_WIDTH
}

/// The Inspector's layout for a window `width` wide, when the reader has it open.
fn inspector_layout(open: bool, width: f32) -> InspectorLayout {
    if !open {
        return InspectorLayout::Closed;
    }
    if width < INSPECTOR_FLOAT_BELOW {
        InspectorLayout::Floating
    } else {
        InspectorLayout::Docked
    }
}

/// The width gate the shell owns, asked for the two panels at once.
///
/// The sidebar's answer here is the *window floor*, and it is deliberately the floor rather than a
/// second breakpoint: [`MIN_LAYOUT_WIDTH`] is `design::size::WINDOW_MIN.0`, which is also what
/// `main.rs` gives the window manager, so a gate below it would be a responsive step §6 does not
/// owe the reader. The narrow band a decorated window lands in answers with a title-bar switch to
/// the searchable kind list (`shell/panels.rs`), which is the one path to the tree there.
fn responsive_panel_visibility(
    sidebar_open: bool,
    inspector_open: bool,
    width: f32,
) -> (bool, bool) {
    let sidebar_visible = sidebar_open && width >= MIN_LAYOUT_WIDTH;
    let inspector_visible = inspector_layout(inspector_open, width) != InspectorLayout::Closed;
    (sidebar_visible, inspector_visible)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragTarget {
    Left,
    Right,
    Dock,
}

/// The accent wash behind a splitter's hit area, one step per state. The wash is what tells a
/// state apart from the next one: the rail width only has three steps to spend across three
/// interactive states.
const DIVIDER_HOVER_WASH_ALPHA: f32 = 0.12;
const DIVIDER_FOCUS_WASH_ALPHA: f32 = 0.18;
const DIVIDER_DRAG_WASH_ALPHA: f32 = 0.24;

/// The four states a splitter paints, as the pair of numbers it paints with.
///
/// `DESIGN.md` §5 asks for four. Focus and hover were the same pair, so the row delivered three:
/// a splitter the keyboard was on looked exactly like one the pointer was over, which is the one
/// state a keyboard user most needs to find. Now every pair differs from every other in at least
/// one channel: the wash steps up from hover to focus to drag, and the rail steps up again at
/// drag. The hit area is always `design::border::HIT`, and rest paints no wash at all, so the
/// 1px structural line stays the whole resting state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DividerPaint {
    Rest,
    Hover,
    Focus,
    Drag,
}

impl DividerPaint {
    /// The width of the visible line.
    fn rail(self) -> Pixels {
        match self {
            Self::Rest => design::border::LINE,
            Self::Hover | Self::Focus => design::border::FOCUS_RAIL,
            Self::Drag => design::border::TABLE_FOCUS_RAIL,
        }
    }

    /// The accent wash behind the hit area.
    ///
    /// Rest is fully transparent, so the 1px structural line is the whole resting state and the
    /// 20px hit area paints nothing around it.
    fn wash(self, highlight: gpui_kit::Hsla) -> gpui_kit::Hsla {
        let alpha = match self {
            Self::Rest => 0.0,
            Self::Hover => DIVIDER_HOVER_WASH_ALPHA,
            Self::Focus => DIVIDER_FOCUS_WASH_ALPHA,
            Self::Drag => DIVIDER_DRAG_WASH_ALPHA,
        };
        highlight.opacity(alpha)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    id: SharedString,
    label: SharedString,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaletteScope {
    #[default]
    Commands,
    Context,
    Namespace,
    Kind,
}

impl PaletteScope {
    /// The query a scope opens with.
    ///
    /// Every scope returns nothing, because the scope filter has already narrowed the list to
    /// the values it switches between. These words used to pre-fill the field so the card would
    /// never look idle, and `filter_commands_with` matches command ids as well as labels, so
    /// `open` hit every `kind.open.<group>/<version>/<Kind>`: 71 of 71 rows, none of which
    /// contains the word the reader can see. `DESIGN-PROPOSAL` §4.5 asks the search to match
    /// strings a person would type, and a word that appears in no row is not one.
    pub(crate) fn query(self) -> &'static str {
        match self {
            Self::Commands | Self::Context | Self::Namespace | Self::Kind => "",
        }
    }

    /// The palette covers the toolbar directly behind it, so a title that repeated
    /// the cluster, the namespace, and the open view would add nothing and could
    /// only truncate. `modality.md > Best practices` asks a modal view for a title
    /// that names its task, so that is all this carries.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Commands => "Command palette",
            Self::Context => "Switch context",
            Self::Namespace => "Switch namespace",
            Self::Kind => "Open a resource",
        }
    }
}

/// Content type for a center tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabContent {
    Resource,
    /// Cluster overview.
    Overview,
    Forwards,
    Helm,
    Settings,
    Preview,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PreviewIdentity {
    gvk: GroupVersionKind,
    namespace: Option<String>,
    name: String,
    uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreviewState {
    FollowSelection,
    Fixed(PreviewIdentity),
}

impl PreviewIdentity {
    fn from_row(spec: &ResourceSpec, row: &Row) -> Option<Self> {
        let gvk = spec
            .resource
            .as_ref()
            .map(|resource| {
                GroupVersionKind::gvk(&resource.group, &resource.version, &resource.kind)
            })
            .or_else(|| {
                row.obj
                    .types
                    .as_ref()
                    .and_then(|types| GroupVersionKind::try_from(types).ok())
            })?;
        Some(Self {
            gvk,
            namespace: row.obj.metadata.namespace.clone(),
            name: row.obj.metadata.name.clone()?,
            uid: row.obj.metadata.uid.clone().unwrap_or_default(),
        })
    }
}

struct ServiceAccountRequest {
    epoch: u64,
    session_epoch: u64,
    source_tab: usize,
    source_view: Entity<PodsView>,
    pod: ObjectRef,
    namespace: SharedString,
    target: ServiceAccountTarget,
}
fn service_account_object_matches(object: &DynamicObject, target: &ServiceAccountTarget) -> bool {
    object
        .types
        .as_ref()
        .is_some_and(|types| types.api_version == "v1" && types.kind == "ServiceAccount")
        && object.metadata.name.as_deref() == Some(target.name.as_str())
        && object.metadata.namespace.as_deref() == Some(target.namespace.as_str())
        && object
            .metadata
            .uid
            .as_deref()
            .is_some_and(|uid| !uid.is_empty())
}

fn service_account_object_detail(object: &DynamicObject) -> String {
    let api_version = object
        .types
        .as_ref()
        .map(|types| types.api_version.as_str())
        .unwrap_or("");
    let kind = object
        .types
        .as_ref()
        .map(|types| types.kind.as_str())
        .unwrap_or("");
    format!(
        "apiVersion={api_version} kind={kind} name={} namespace={} uid={}",
        object.metadata.name.as_deref().unwrap_or(""),
        object.metadata.namespace.as_deref().unwrap_or(""),
        object.metadata.uid.as_deref().unwrap_or("")
    )
}

fn preview_title(kind: &str, name: Option<&str>, fixed: bool) -> SharedString {
    if fixed {
        name.map_or_else(
            || SharedString::from(kind.to_owned()),
            |name| SharedString::from(format!("{kind} · {name}")),
        )
    } else {
        SharedString::from(format!("{kind} Preview"))
    }
}

/// Center tab definition.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CenterTab {
    content: TabContent,
    kind: SharedString,
    identity: Option<GroupVersionKind>,
    title: SharedString,
    icon: IconName,
    pinned: bool,
    /// Catalog entry loaded for this tab.
    entry: Option<ResourceEntry>,
    resource: Option<ApiResource>,
    preview: Option<PreviewState>,
}

#[derive(Clone)]
struct CenterTabDragPayload {
    index: usize,
    title: SharedString,
    icon: IconName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CenterTabDragState {
    source: usize,
    insertion: Option<usize>,
}

fn tab_drop_gap(open_tabs: &[usize], source: usize, target: usize, after: bool) -> Option<usize> {
    let source_position = open_tabs.iter().position(|tab| *tab == source)?;
    let target_position = open_tabs.iter().position(|tab| *tab == target)?;
    let mut insertion = target_position + usize::from(after);
    if source_position < insertion {
        insertion -= 1;
    }
    (insertion != source_position).then_some(insertion)
}

fn reorder_open_tabs(open_tabs: &mut Vec<usize>, source: usize, insertion: usize) -> bool {
    let Some(source_position) = open_tabs.iter().position(|tab| *tab == source) else {
        return false;
    };
    let insertion = insertion.min(open_tabs.len().saturating_sub(1));
    if source_position == insertion {
        return false;
    }
    let tab = open_tabs.remove(source_position);
    open_tabs.insert(insertion, tab);
    true
}

fn normalize_open_tabs(open_tabs: &mut Vec<usize>, tabs: &[CenterTab]) {
    let mut pinned = Vec::new();
    let mut ordinary = Vec::new();
    for index in open_tabs.iter().copied() {
        if tabs.get(index).is_some_and(|tab| tab.pinned) {
            pinned.push(index);
        } else {
            ordinary.push(index);
        }
    }
    pinned.extend(ordinary);
    *open_tabs = pinned;
}

fn pinned_open_tab_count(open_tabs: &[usize], tabs: &[CenterTab]) -> usize {
    open_tabs
        .iter()
        .filter(|index| tabs.get(**index).is_some_and(|tab| tab.pinned))
        .count()
}

fn bounded_tab_insertion(
    open_tabs: &[usize],
    tabs: &[CenterTab],
    source: usize,
    insertion: usize,
) -> Option<usize> {
    if !open_tabs.contains(&source) {
        return None;
    }
    let boundary = pinned_open_tab_count(open_tabs, tabs);
    let source_pinned = tabs.get(source).is_some_and(|tab| tab.pinned);
    let insertion = if source_pinned {
        insertion.min(boundary.saturating_sub(1))
    } else {
        insertion.max(boundary)
    };
    Some(insertion)
}

fn bounded_tab_drop_gap(
    open_tabs: &[usize],
    tabs: &[CenterTab],
    source: usize,
    target: usize,
    after: bool,
) -> Option<usize> {
    let insertion = tab_drop_gap(open_tabs, source, target, after)?;
    bounded_tab_insertion(open_tabs, tabs, source, insertion)
}

fn reorder_open_tabs_within_group(
    open_tabs: &mut Vec<usize>,
    tabs: &[CenterTab],
    source: usize,
    insertion: usize,
) -> bool {
    let Some(insertion) = bounded_tab_insertion(open_tabs, tabs, source, insertion) else {
        return false;
    };
    reorder_open_tabs(open_tabs, source, insertion)
}

fn move_open_tab_within_group(
    open_tabs: &mut Vec<usize>,
    tabs: &[CenterTab],
    source: usize,
    delta: isize,
) -> bool {
    let Some(source_position) = open_tabs.iter().position(|tab| *tab == source) else {
        return false;
    };
    if delta == 0 {
        return false;
    }
    let boundary = pinned_open_tab_count(open_tabs, tabs);
    let source_pinned = tabs.get(source).is_some_and(|tab| tab.pinned);
    let group_start = if source_pinned { 0 } else { boundary };
    let group_end = if source_pinned {
        boundary
    } else {
        open_tabs.len()
    };
    let target_position = if delta < 0 {
        source_position.checked_sub(delta.unsigned_abs())
    } else {
        source_position.checked_add(delta as usize)
    };
    let Some(target_position) = target_position else {
        return false;
    };
    if target_position < group_start || target_position >= group_end {
        return false;
    }
    let tab = open_tabs.remove(source_position);
    open_tabs.insert(target_position, tab);
    true
}

/// View entity for a center tab.
#[derive(Clone)]
enum TabView {
    Resource(Entity<PodsView>),
    Overview(Entity<OverviewView>),
    Forwards(Entity<ForwardsView>),
    Helm(Entity<HelmView>),
    Settings(Entity<SettingsView>),
    Preview(Entity<InspectorPanel>),
}

impl TabView {
    #[cfg(test)]
    fn entity_id(&self) -> gpui_kit::EntityId {
        match self {
            Self::Resource(view) => view.entity_id(),
            Self::Overview(view) => view.entity_id(),
            Self::Forwards(view) => view.entity_id(),
            Self::Helm(view) => view.entity_id(),
            Self::Settings(view) => view.entity_id(),
            Self::Preview(view) => view.entity_id(),
        }
    }

    fn resource(&self) -> Option<&Entity<PodsView>> {
        match self {
            Self::Resource(view) => Some(view),
            _ => None,
        }
    }
}

/// Resource catalog load state.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CatalogState {
    Loading,
    Ready,
    Failed(String),
}

const CATALOG_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

fn catalog_retry_delay(attempt: usize) -> Duration {
    CATALOG_RETRY_DELAYS[attempt.min(CATALOG_RETRY_DELAYS.len() - 1)]
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NamespaceState {
    Loading,
    Ready(Vec<SharedString>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StartupState {
    Loading,
    Ready,
    Unavailable(String),
}

impl StartupState {
    fn from_session(session: Option<&ClusterSession>) -> Self {
        match session {
            Some(ClusterSession::Unavailable { reason, .. })
                if reason == STARTUP_LOADING_REASON =>
            {
                Self::Loading
            }
            Some(ClusterSession::Unavailable { reason, .. }) => Self::Unavailable(reason.clone()),
            _ => Self::Ready,
        }
    }
}

/// Cluster connection state shown in the top bar.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConnectionState {
    Connecting,
    Live,
    /// Not `Live`, and a retry is on its way. Also what a connection that has said nothing
    /// for ten seconds becomes, which is `UI-SPEC` §4.1's own second row rather than a
    /// state invented beside it.
    Reconnecting(String),
    /// No usable connection was available at startup.
    Failed(String),
}

impl ConnectionState {
    fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Live => "Live",
            Self::Reconnecting(_) => "Reconnecting",
            Self::Failed(_) => "Unavailable",
        }
    }

    /// The channel the dot wears.
    ///
    /// `Live` is [`design::Severity::Muted`], not `Success`, and that is D5 rather than a
    /// preference: a healthy cluster is the state the reader spends all their time in, and a
    /// green dot spends the one colour the status channel has for something being wrong on a
    /// state that is not. `UI-SPEC` §4.1 says the same thing in pixels — `fg.tertiary`.
    fn severity(&self) -> design::Severity {
        match self {
            Self::Connecting | Self::Live => design::Severity::Muted,
            Self::Reconnecting(_) => design::Severity::Warning,
            Self::Failed(_) => design::Severity::Error,
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            Self::Reconnecting(reason) | Self::Failed(reason) => Some(reason),
            Self::Connecting | Self::Live => None,
        }
    }

    /// Whether the cluster is still being waited on, which is what the ten-second rule
    /// counts and what a resolving state cancels.
    fn is_waiting(&self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting(_))
    }
}

/// The reason a connection that has said nothing for [`SLOW_CONNECTION_AFTER`] reports.
///
/// Ten seconds is `UI-SPEC` §9.3's number. The timeouts it is asking about are not this
/// app's: `k8s_core::latency` gives an ordinary read thirty seconds and a watch stream sixty
/// or three hundred, so a cluster that has gone away produces no event at all for a minute.
/// Ten seconds of nothing is where "waiting" and "broken" stop looking different to a reader,
/// so that is where the connection point stops being quiet and says so.
const SLOW_CONNECTION_REASON: &str =
    "Still waiting for the cluster. Ten seconds is a slow response, not a failure yet.";

/// How long a connection may say nothing before the connection point says so.
///
/// Long enough that a laptop waking from sleep does not open with an alarm, short enough that
/// a reader who has lost their cluster finds out while they are still looking.
const SLOW_CONNECTION_AFTER: Duration = Duration::from_secs(10);

fn initial_catalog_state(session: Option<&ClusterSession>, startup: &StartupState) -> CatalogState {
    match startup {
        StartupState::Loading => CatalogState::Loading,
        StartupState::Unavailable(reason) => CatalogState::Failed(reason.clone()),
        StartupState::Ready => match session {
            Some(ClusterSession::Ready { .. }) => CatalogState::Loading,
            Some(ClusterSession::Unavailable { reason, .. }) => {
                CatalogState::Failed(reason.clone())
            }
            None => CatalogState::Ready,
        },
    }
}

fn initial_connection_state(
    session: Option<&ClusterSession>,
    startup: &StartupState,
) -> ConnectionState {
    match startup {
        StartupState::Loading => ConnectionState::Connecting,
        StartupState::Unavailable(reason) => ConnectionState::Failed(reason.clone()),
        StartupState::Ready => match session {
            Some(ClusterSession::Ready { .. }) => ConnectionState::Connecting,
            Some(ClusterSession::Unavailable { reason, .. }) => {
                ConnectionState::Failed(reason.clone())
            }
            None => ConnectionState::Live,
        },
    }
}

fn map_connection_state(state: &CoreConnectionState, startup: &StartupState) -> ConnectionState {
    match startup {
        StartupState::Loading => ConnectionState::Connecting,
        StartupState::Unavailable(reason) => ConnectionState::Failed(reason.clone()),
        StartupState::Ready => match state {
            CoreConnectionState::Ready {} => ConnectionState::Live,
            CoreConnectionState::Degraded { reason } => {
                ConnectionState::Reconnecting(reason.clone())
            }
            CoreConnectionState::Unknown {} | CoreConnectionState::Connecting {} => {
                ConnectionState::Connecting
            }
            CoreConnectionState::Offline {} => {
                ConnectionState::Failed("Connection unavailable.".to_owned())
            }
        },
    }
}

#[derive(Clone)]
struct HealthProbe {
    receiver: tokio::sync::watch::Receiver<Health>,
    latency: tokio::sync::watch::Receiver<k8s_core::latency::Latency>,
}

const NAMESPACE_LOAD_TIMEOUT: Duration = Duration::from_secs(10);

fn kubeconfig_source_warning(registry: &ClusterRegistry) -> Option<String> {
    (!registry.source_errors().is_empty()).then(|| {
        registry
            .source_errors()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

async fn load_cluster_namespaces(source: ClusterDataSource) -> Result<Vec<String>, String> {
    let result = tokio::time::timeout(NAMESPACE_LOAD_TIMEOUT, async move {
        source.port().namespaces().await
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(
            "The namespace list request timed out. Check context access, then try again."
                .to_owned(),
        ),
    }
}

/// Install resource table action handlers.
fn install_view_handlers(
    shell: &WeakEntity<Shell>,
    view: &Entity<PodsView>,
    spec: ResourceSpec,
    source_tab: usize,
    cx: &mut Context<Shell>,
) {
    let activate_shell = shell.clone();
    let activate_spec = spec.clone();
    view.update(cx, |view, _| {
        view.on_activate_row(move |row, _window, cx| {
            let shell = activate_shell.clone();
            let spec = activate_spec.clone();
            let _ = shell.update(cx, |shell, cx| {
                shell.open_row_details(row, spec, source_tab, cx);
            });
        });
    });
    let selection_shell = shell.clone();
    let selection_spec = spec;
    view.update(cx, |view, _| {
        view.on_selection_changed(move |row, cx| {
            let shell = selection_shell.clone();
            let spec = selection_spec.clone();
            cx.defer(move |cx| {
                shell
                    .update(cx, |shell, cx| {
                        shell.on_row_selection(row, spec, source_tab, cx)
                    })
                    .ok();
            });
        });
    });
    let logs_shell = shell.clone();
    view.update(cx, |view, _| {
        view.on_logs_requested(move |request, window, cx| {
            let request = request.clone();
            let shell = logs_shell.clone();
            window.defer(cx, move |window, cx| {
                shell
                    .update(cx, |shell, cx| shell.open_logs(request, window, cx))
                    .ok();
            });
        });
    });
    // The target owns its data. The handler does not read the table again.
    let delete_shell = shell.clone();
    view.update(cx, |view, _| {
        view.on_delete_requested(move |target, window, cx| {
            let shell = delete_shell.clone();
            window.defer(cx, move |window, cx| {
                shell
                    .update(cx, |shell, cx| shell.open_delete_dialog(target, window, cx))
                    .ok();
            });
        });
    });
    let scale_shell = shell.clone();
    view.update(cx, |view, _| {
        view.on_scale_requested(move |target, window, cx| {
            let shell = scale_shell.clone();
            window.defer(cx, move |window, cx| {
                shell
                    .update(cx, |shell, cx| shell.open_scale_dialog(target, window, cx))
                    .ok();
            });
        });
    });
    // Exec and port-forward handlers.
    let exec_shell = shell.clone();
    view.update(cx, |view, _| {
        view.on_exec_requested(move |target, window, cx| {
            let shell = exec_shell.clone();
            window.defer(cx, move |window, cx| {
                shell
                    .update(cx, |shell, cx| shell.open_exec_dialog(target, window, cx))
                    .ok();
            });
        });
    });
    let forward_shell = shell.clone();
    view.update(cx, |view, _| {
        view.on_forward_requested(move |target, window, cx| {
            let shell = forward_shell.clone();
            window.defer(cx, move |window, cx| {
                shell
                    .update(cx, |shell, cx| {
                        shell.open_port_forward_dialog(target, window, cx)
                    })
                    .ok();
            });
        });
    });
    let service_account_shell = shell.clone();
    let service_account_view = view.clone();
    view.update(cx, |view, _| {
        view.on_service_account_requested(move |target, window, cx| {
            let shell = service_account_shell.clone();
            let source_view = service_account_view.clone();
            window.defer(cx, move |_, cx| {
                shell
                    .update(cx, |shell, cx| {
                        shell.open_service_account_target(target, source_view, cx);
                    })
                    .ok();
            });
        });
    });
}

#[derive(Default)]
struct PanelRoute {
    active: Cell<bool>,
    generation: Cell<u64>,
    pushed: RefCell<Option<(String, String)>>,
}

impl PanelRoute {
    fn identity(&self, base: InspectorSession) -> InspectorSession {
        InspectorSession {
            id: base.id.wrapping_add(self.generation.get()),
            cluster_id: base.cluster_id,
        }
    }

    /// Remembers a pushed revision and reports whether it made the panel's object data stale.
    ///
    /// A new revision of the same object is a new identity, so the panel drops the describe,
    /// events, and metrics it cached for the revision that is gone. An apply in flight is the
    /// exception: the panel keeps the newer object as a pending selection and loads it when the
    /// result arrives, so the identity must not move under a write that is already on its way.
    fn record(&self, selection: &InspectorSelection, applying: bool) -> bool {
        let revision = content_revision(selection);
        let uid = selection.object.uid.clone();
        let changed = !applying
            && self
                .pushed
                .borrow()
                .as_ref()
                .is_some_and(|(previous_uid, previous)| {
                    previous_uid == &uid && previous != &revision
                });
        if changed {
            // The identity carries this counter, so the panel reads the new revision as a new
            // session. A different object needs no bump: it changes the selection itself, which
            // already reloads everything.
            self.generation.set(self.generation.get().wrapping_add(1));
        }
        *self.pushed.borrow_mut() = Some((uid, revision));
        changed
    }

    fn reset(&self) {
        self.generation.set(0);
        *self.pushed.borrow_mut() = None;
    }
}

fn content_revision(selection: &InspectorSelection) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    selection.yaml.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn routed_inspector_binding(
    inspector: Entity<InspectorPanel>,
    route: Rc<PanelRoute>,
    base: InspectorSession,
) -> InspectorBinding {
    InspectorBinding::new(move |update, cx| {
        if !route.active.get() {
            return;
        }
        if let InspectorUpdate::Selection(Some(selection)) = &update
            && route.record(selection, inspector.read(cx).is_applying())
        {
            let identity = route.identity(base);
            inspector.update(cx, |panel, _| panel.set_session_identity(identity));
        }
        inspector.update(cx, |panel, cx| match update {
            InspectorUpdate::Selection(selection) => panel.set_selection(selection, cx),
            InspectorUpdate::Yaml(yaml) => panel.set_yaml(yaml, cx),
        });
    })
}

fn install_inspector_apply_handler(
    shell: &WeakEntity<Shell>,
    panel: &Entity<InspectorPanel>,
    cx: &mut Context<Shell>,
) {
    let shell = shell.clone();
    let target = panel.downgrade();
    panel.update(cx, |panel, _| {
        // Each transport owns its own handles, so installing one cannot move the other's.
        let apply_shell = shell.clone();
        let apply_target = target.clone();
        panel.set_targeted_apply_handler(move |request, cx| {
            let shell = apply_shell.clone();
            let target = apply_target.clone();
            cx.defer(move |cx| {
                let Some(target) = target.upgrade() else {
                    return;
                };
                shell
                    .update(cx, |shell, cx| {
                        shell.apply_from_inspector(target, request, cx)
                    })
                    .ok();
            });
        });
        // The server-side check follows the same transport as the apply, so both are guarded by
        // the same session epoch and neither can outlive the review that asked for it.
        let check_shell = shell.clone();
        let check_target = target.clone();
        panel.set_targeted_check_handler(move |request, cx| {
            let shell = check_shell.clone();
            let target = check_target.clone();
            cx.defer(move |cx| {
                let Some(target) = target.upgrade() else {
                    return;
                };
                shell
                    .update(cx, |shell, cx| {
                        shell.check_from_inspector(target, request, cx)
                    })
                    .ok();
            });
        });
    });
}

fn install_dock_notice_handler(
    shell: &WeakEntity<Shell>,
    panel: &Entity<DockPanel>,
    cx: &mut Context<Shell>,
) {
    let shell = shell.clone();
    panel.update(cx, |panel, _| {
        panel.set_notice_handler(move |message, severity, cx| {
            let shell = shell.clone();
            cx.defer(move |cx| {
                shell
                    .update(cx, |shell, cx| {
                        shell.notify(message, severity, None, cx);
                    })
                    .ok();
            });
        });
    });
}

fn install_metrics_retry_handler(
    shell: &WeakEntity<Shell>,
    panel: &Entity<InspectorPanel>,
    cx: &mut Context<Shell>,
) {
    let shell = shell.clone();
    panel.update(cx, |panel, _| {
        panel.set_metrics_probe_retry_handler(move |cx| {
            let shell = shell.clone();
            cx.defer(move |cx| {
                shell
                    .update(cx, |shell, cx| shell.retry_metrics_probe(cx))
                    .ok();
            });
        });
    });
}

/// Build the Inspector metrics handle for a ready session.
fn metrics_handle_for(session: &ClusterSession) -> Option<MetricsHandle> {
    let registry = session.registry()?.clone();
    let cluster_id = session.cluster_id()?;
    let handle = session.tokio_handle()?.clone();
    let cluster = registry.get(cluster_id)?;
    let client = cluster.client().clone();
    let latency = cluster.latency();
    Some(MetricsHandle::new(handle, client).with_latency(latency))
}

/// Build a one-time Overview data handle.
fn overview_handle_for(session: &ClusterSession) -> Option<OverviewHandle> {
    let registry = session.registry()?.clone();
    let cluster = session.cluster_id()?;
    let handle = session.tokio_handle()?.clone();
    let client = registry.get(cluster)?.client().clone();
    Some(OverviewHandle::new(handle, client))
}

fn cluster_names(registry: &ClusterRegistry) -> Vec<SharedString> {
    registry
        .clusters()
        .iter()
        .map(|cluster| SharedString::from(cluster.name()))
        .chain(
            registry
                .context_errors()
                .iter()
                .map(|error| SharedString::from(error.context.as_str())),
        )
        .collect()
}

/// The cluster the active Hotbar bank claims, when the registry still has it.
///
/// A slot is a convenience for reaching a cluster, not a claim on the app: a slot
/// that names a cluster the registry has since lost only loses its own claim, and
/// the caller falls back to the kubeconfig's current context. Refusing every
/// context because one saved slot went stale would make a dead convenience
/// permanent.
fn hotbar_cluster(hotbar: &Hotbar, has_cluster: impl Fn(ClusterId) -> bool) -> Option<ClusterId> {
    let slot = hotbar.active_bank()?.slots.first()?;
    has_cluster(slot.cluster_id).then_some(slot.cluster_id)
}

fn first_valid_hotbar_cluster(registry: &ClusterRegistry) -> Option<ClusterId> {
    let (hotbar, _) = ClusterSession::load_hotbar(registry);
    hotbar_cluster(&hotbar, |id| registry.get(id).is_some())
}

fn preferred_cluster(
    registry: &ClusterRegistry,
    hotbar_choice: Option<ClusterId>,
) -> Option<ClusterId> {
    hotbar_choice
        .filter(|id| registry.get(*id).is_some())
        .or_else(|| registry.current_cluster_id())
}

fn select_startup_session(session: ClusterSession) -> ClusterSession {
    let Some(registry) = session.registry().cloned() else {
        return session;
    };
    let Some(handle) = session.tokio_handle().cloned() else {
        return session;
    };
    #[cfg(test)]
    let hotbar_choice = None;
    #[cfg(not(test))]
    let hotbar_choice = first_valid_hotbar_cluster(&registry);
    let selected = preferred_cluster(&registry, hotbar_choice);
    ClusterSession::from_registry_with_cluster(registry, handle, selected)
}

fn reloaded_session(
    registry: Arc<ClusterRegistry>,
    handle: tokio::runtime::Handle,
    preferred: Option<ClusterId>,
) -> Result<ClusterSession, String> {
    let selected = preferred
        .filter(|id| registry.get(*id).is_some())
        .or_else(|| preferred_cluster(&registry, first_valid_hotbar_cluster(&registry)));
    let session = ClusterSession::from_registry_with_cluster(registry, handle, selected);
    match session {
        ClusterSession::Ready { .. } => Ok(session),
        ClusterSession::Unavailable { reason, .. } => Err(reason),
    }
}

async fn load_cluster_registry(path: Option<PathBuf>) -> Result<Arc<ClusterRegistry>, String> {
    let registry = match path {
        Some(path) => ClusterRegistry::load(path).await,
        None => ClusterRegistry::load_default().await,
    }
    .map_err(|error| error.to_string())?;
    Ok(Arc::new(registry))
}

fn dispatch_theme_action(action: Box<dyn Action>, window: &mut Window, cx: &mut App) {
    if let Some(focus) = window.focused(cx) {
        focus.dispatch_action(action.as_ref(), window, cx);
    } else {
        window.dispatch_action(action, cx);
    }
}

fn dispatch_theme_choice(choice: &crate::settings::ThemeChoice, window: &mut Window, cx: &mut App) {
    use crate::settings::ThemeChoice;
    match choice {
        ThemeChoice::Light => dispatch_theme_action(Box::new(UseLightTheme), window, cx),
        ThemeChoice::Dark => dispatch_theme_action(Box::new(UseDarkTheme), window, cx),
        ThemeChoice::Named(name) => {
            dispatch_theme_action(Box::new(UseTheme { name: name.clone() }), window, cx)
        }
        ThemeChoice::System => dispatch_theme_action(Box::new(UseSystemTheme), window, cx),
    }
}

fn apply_theme_choice(
    choice: crate::settings::ThemeChoice,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), String> {
    use crate::settings::ThemeChoice;
    if let ThemeChoice::Named(name) = &choice {
        let Some(registry) = cx.try_global::<ThemeRegistry>() else {
            return Err(
                "Theme registry is unavailable. Check the theme configuration, then try again."
                    .to_owned(),
            );
        };
        if !registry.themes().contains_key(name.as_str()) {
            return Err(format!(
                "Theme not found: {name}. Choose an installed theme."
            ));
        }
    }
    let previous = crate::settings::theme_choice(cx);
    crate::settings::set_theme_choice(cx, choice.clone());
    dispatch_theme_choice(&choice, window, cx);
    let value = choice.value();
    if let Err(error) = crate::settings::update(cx, |settings| settings.theme = Some(value)) {
        crate::settings::set_theme_choice(cx, previous.clone());
        dispatch_theme_choice(&previous, window, cx);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
struct DemoSource;

#[cfg(test)]
struct DemoSubscription {
    _unbounded: Option<tokio::sync::mpsc::UnboundedSender<SourceEvent>>,
    _bounded: Option<tokio::sync::mpsc::Sender<SourceEvent>>,
}

#[cfg(test)]
impl ResourceSubscription for DemoSubscription {
    fn cancel(&mut self) {}
}

#[cfg(test)]
fn demo_events() -> Vec<SourceEvent> {
    let mut events = vec![SourceEvent::Init];
    events.extend((0..3).map(|index| {
        let object = Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": format!("demo-{index}"),
                    "namespace": "default",
                    "uid": format!("uid-demo-{index}"),
                },
            }))
            .expect("demo pod"),
        );
        SourceEvent::Store(StoreEvent {
            op: StoreOp::Apply,
            obj: object,
        })
    }));
    events.push(SourceEvent::InitDone);
    events
}

#[cfg(test)]
impl ResourceSource for DemoSource {
    fn subscribe(
        &mut self,
        events: tokio::sync::mpsc::UnboundedSender<SourceEvent>,
    ) -> Box<dyn ResourceSubscription> {
        for event in demo_events() {
            let _ = events.send(event);
        }
        Box::new(DemoSubscription {
            _unbounded: Some(events),
            _bounded: None,
        })
    }

    fn subscribe_bounded(
        &mut self,
        events: tokio::sync::mpsc::Sender<SourceEvent>,
    ) -> Box<dyn ResourceSubscription> {
        for event in demo_events() {
            let _ = events.try_send(event);
        }
        Box::new(DemoSubscription {
            _unbounded: None,
            _bounded: Some(events),
        })
    }
}

#[cfg(test)]
fn fake_source_factory() -> SourceFactory {
    Box::new(|| Box::new(DemoSource))
}

fn known_resource_identity(kind: &str) -> Option<GroupVersionKind> {
    match kind {
        "Pod" => Some(GroupVersionKind::gvk("", "v1", "Pod")),
        "Deployment" => Some(GroupVersionKind::gvk("apps", "v1", "Deployment")),
        _ => None,
    }
}

/// The badge a switcher row carries when it names the value already in use.
const PALETTE_CURRENT_BADGE: &str = "Current";

/// The recovery both connection warnings offer: reload the kubeconfig files, which is the step
/// that makes a context selectable at all. It dispatches the same action the toolbar and the
/// keymap use, so the button cannot drift from the command it stands for.
fn reload_kubeconfigs_action() -> ToastAction {
    ToastAction {
        label: "Reload Kubeconfigs",
        run: Rc::new(
            |shell: &mut Shell, window: &mut Window, cx: &mut Context<Shell>| {
                shell.dispatch(ReloadKubeconfigs, window, cx);
            },
        ),
    }
}

/// True when a palette row names the value that is already selected.
///
/// `Menu / Command parity` in `DESIGN.md` §4 gives the current item exactly two marks, the
/// `current` mark and the check, and no second wording. The switchers build their current row as
/// an unavailable command so the badge slot has something in it, which also made the row dim and
/// unresponsive and added a trailing `Current` word 531px from its own checkmark. `labels.md`
/// defines a tertiary label as text describing an *unavailable* item, and "already selected" is
/// not unavailable.
pub(crate) fn palette_command_is_current(command: &Command) -> bool {
    matches!(
        &command.run,
        CommandRun::Unavailable { badge, .. } if *badge == PALETTE_CURRENT_BADGE
    )
}

fn stable_command_id(prefix: &str, value: &str) -> SharedString {
    let mut id = String::with_capacity(prefix.len() + value.len() + 1);
    id.push_str(prefix);
    id.push('.');
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '/') {
            id.push(character);
        } else {
            id.push('_');
        }
    }
    SharedString::from(id)
}

/// Body or metadata label for a shell-owned surface.
fn shell_label(size: Pixels, text: impl Into<SharedString>) -> Label {
    Label::new(text).text_size(size)
}

/// Key the resource tree's expansion state lives under in `settings.json`.
///
/// It goes in the settings file's `extra` map rather than in a new field, so it rides the
/// directory watch and the atomic write the rest of the file already uses, and a settings file
/// written by an older build still parses.
const TREE_COLLAPSED_KEY: &str = "treeCollapsed";

/// The collapsed tree rows remembered for each cluster.
///
/// `outline-views.md` asks a disclosure view to "retain people's expansion choices … store the
/// state so you can display it again the next time". The state used to live only in memory and
/// was rebuilt from `default_collapsed()` on every successful catalog load, and the cached
/// catalog is a load too: starting the app cleared the tree once from disk and then again when
/// the live refresh arrived.
fn remembered_collapsed() -> HashMap<String, HashSet<SharedString>> {
    let settings = crate::settings::load();
    let Some(serde_json::Value::Object(by_cluster)) = settings.extra.get(TREE_COLLAPSED_KEY) else {
        return HashMap::new();
    };
    by_cluster
        .iter()
        .filter_map(|(cluster, ids)| {
            let ids = ids.as_array()?;
            let ids = ids
                .iter()
                .filter_map(|id| id.as_str().map(SharedString::from))
                .collect();
            Some((cluster.clone(), ids))
        })
        .collect()
}

/// Write the expansion state back, so the next run opens the tree the way it was left.
///
/// A failure is logged and nothing else: the tree still works from memory, and a settings file
/// that cannot be written is reported by the Settings panel rather than by a toast over the
/// resource tree.
fn persist_collapsed(cx: &mut App, by_cluster: &HashMap<String, HashSet<SharedString>>) {
    let value = serde_json::Value::Object(
        by_cluster
            .iter()
            .map(|(cluster, ids)| {
                (
                    cluster.clone(),
                    serde_json::Value::Array(
                        ids.iter()
                            .map(|id| serde_json::Value::String(id.to_string()))
                            .collect(),
                    ),
                )
            })
            .collect(),
    );
    if let Err(error) = crate::settings::update(cx, |settings| {
        settings.extra.insert(TREE_COLLAPSED_KEY.to_owned(), value);
    }) {
        eprintln!("k8s-gpui: could not save the resource tree expansion: {error}");
    }
}

/// Container names a Pod declares, in document order, for the exec chooser.
fn pod_container_names(object: &DynamicObject) -> Vec<SharedString> {
    object
        .data
        .get("spec")
        .and_then(|spec| spec.get("containers"))
        .and_then(serde_json::Value::as_array)
        .map(|containers| {
            containers
                .iter()
                .filter_map(|container| {
                    container
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(SharedString::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Exec target for a search result. `None` when the result is not a Pod, because a shell needs a
/// container to run in.
fn search_hit_exec_target(hit: &SearchHit) -> Option<ExecTarget> {
    if hit.resource.kind != "Pod" {
        return None;
    }
    Some(ExecTarget {
        namespace: hit.object.metadata.namespace.clone(),
        name: hit.object.metadata.name.clone()?.into(),
        containers: pod_container_names(&hit.object),
    })
}

/// Port-forward target for a search result, carrying the `containerPort` values the Pod declares
/// so the dialog can offer them as choices.
fn search_hit_forward_target(hit: &SearchHit) -> Option<PortForwardTarget> {
    if hit.resource.kind != "Pod" {
        return None;
    }
    Some(PortForwardTarget {
        namespace: hit.object.metadata.namespace.clone(),
        name: hit.object.metadata.name.clone()?.into(),
        ports: crate::panels::forwards::container_ports(&hit.object),
    })
}

/// Tooltip that shows the current keycap next to the label, so a button never
/// promises a shortcut the active surface has released.
fn empty_state_tooltip(
    label: impl Into<SharedString>,
    chord: Option<String>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    // The chord is parsed once, at build time: a malformed chord leaves the label on its own
    // rather than advertising a key that does nothing.
    let keycap = chord
        .as_deref()
        .and_then(|chord| Keystroke::parse(chord).ok());
    move |window, cx| {
        Tooltip::new(label.clone())
            .key_binding(keycap.clone().map(Kbd::new))
            .build(window, cx)
    }
}

/// What the center says after every view is closed.
const EMPTY_VIEW_HINT: &str =
    "Open a resource from the tree, or run a command from the Command Palette.";
/// Keymap action and context behind the empty state's primary action.
const PALETTE_ACTION: &str = "k8s_shell::ToggleCommandPalette";
const PALETTE_CONTEXT: &str = "Shell";

#[cfg(test)]
type NamespaceFuture =
    Rc<dyn Fn(ClusterId) -> crate::panels::inspector_data::OpsFuture<Vec<String>>>;

#[cfg(test)]
type CatalogFuture = Rc<dyn Fn() -> crate::panels::inspector_data::OpsFuture<ResourceCatalog>>;

#[cfg(test)]
type ApplyFuture = Rc<
    dyn Fn(ApplyRequest) -> crate::panels::inspector_data::OpsFuture<crate::panels::ApplyOutcome>,
>;

/// The test seam for the server-side check, shaped like the apply seam so a test can drive the
/// same path the live session takes.
#[cfg(test)]
type CheckFuture =
    Rc<dyn Fn(ApplyRequest) -> crate::panels::inspector_data::OpsFuture<k8s_core::ops::ApplyCheck>>;

#[cfg(test)]
type ServiceAccountFuture =
    Rc<dyn Fn(ServiceAccountTarget) -> crate::panels::inspector_data::OpsFuture<DynamicObject>>;

pub struct Shell {
    focus_handle: FocusHandle,
    focus_active_view_pending: bool,
    left_divider_focus: FocusHandle,
    right_divider_focus: FocusHandle,
    dock_divider_focus: FocusHandle,
    palette_focus_handle: FocusHandle,
    palette_previous_focus: Option<FocusHandle>,
    sidebar_previous_focus: Option<FocusHandle>,
    inspector_previous_focus: Option<FocusHandle>,
    dock_previous_focus: Option<FocusHandle>,
    /// Reason shown when an unavailable command is selected.
    palette_note: Option<&'static str>,
    /// Action dispatched after the current update.
    pending_action: Option<Box<dyn Action>>,
    tree: ResourceTree,
    tree_filter: String,
    tree_filter_input: Entity<TextInput>,
    tree_focus_handle: FocusHandle,
    /// First focus stop in the resource sidebar.
    hotbar_focus_handle: FocusHandle,
    hotbar_bank_focus: FocusHandle,
    /// Whether the bank menu is open, so the trigger can report it and stay selected.
    ///
    /// gpui-kit's `DropdownMenu` owns the menu entity and its dismissal, so all the shell has
    /// to keep is the open flag its own trigger reads.
    hotbar_bank_open: bool,
    /// Whether the resource header's overflow menu is open.
    ///
    /// Its own flag rather than a shared one with the hotbar's, because two
    /// menus in one window are two independent facts and one flag for both makes
    /// opening the second close the first from under the reader.
    resource_menu_open: bool,
    hotbar_add_focus: FocusHandle,
    hotbar_hide_focus: FocusHandle,
    hotbar_slot_cursor: usize,
    hotbar_scroll: ScrollHandle,
    tree_scroll: ScrollHandle,
    tabs_scroll: ScrollHandle,
    pinned_tabs_scroll: ScrollHandle,
    center_tabs_focus: FocusHandle,
    center_tabs_cursor: Option<usize>,
    center_tab_drag: Option<CenterTabDragState>,
    tab_context_menu: Option<Entity<PopupMenu>>,
    tab_context_menu_position: gpui_kit::Point<Pixels>,
    tab_context_menu_previous_focus: Option<FocusHandle>,
    tree_context_menu: Option<Entity<PopupMenu>>,
    tree_context_menu_position: gpui_kit::Point<Pixels>,
    tree_context_menu_previous_focus: Option<FocusHandle>,
    viewport_width: f32,
    viewport_height: f32,
    /// Index into the visible tree rows.
    tree_cursor: Option<usize>,
    collapsed: HashSet<SharedString>,
    /// The expansion state to restore, by cluster, read from `settings.json` at startup and
    /// updated whenever a row is toggled.
    collapsed_saved: HashMap<String, HashSet<SharedString>>,
    /// Clusters whose expansion has already been decided this run. A cluster is only seeded from
    /// its defaults the first time it is seen, so a catalog refresh cannot close the tree under
    /// the reader, and a cached catalog cannot close it before the live refresh even arrives.
    collapsed_seen: HashSet<String>,
    selected_node: Option<Selection>,
    clusters: Vec<SharedString>,
    /// Index into `clusters`, or `clusters.len()` when no context is active.
    active_cluster: usize,
    namespace: SharedString,
    namespace_state: NamespaceState,
    /// Last namespace selected for each cluster.
    namespace_by_cluster: HashMap<ClusterId, SharedString>,
    namespaces_by_cluster: HashMap<ClusterId, NamespaceState>,
    _namespace_task: Option<Task<()>>,
    sidebar_open: bool,
    /// Center tab registry. Index 0 is Pods and index 1 is Deployments.
    tabs: Vec<CenterTab>,
    /// View entities parallel to `tabs`.
    views: Vec<Option<TabView>>,
    open_tabs: Vec<usize>,
    active_tab: usize,
    inspector_open: bool,
    dock_open: bool,
    left_width: f32,
    right_width: f32,
    /// The Inspector width the reader chose, which a breakpoint narrows for the frame and
    /// never overwrites.
    ///
    /// A ceiling that is written back is not a ceiling, it is a new preference: narrow the
    /// window to 1300 once and the 280 row would become the reader's Inspector width for
    /// good. So the choice and the drawn width are two facts, the choice is what `layout.json`
    /// holds, and the drawn width is recomputed from it on every frame.
    right_width_chosen: f32,
    dock_height: f32,
    drag: Option<DividerDrag>,
    palette_open: bool,
    palette_scroll: ScrollHandle,
    palette_input: Entity<TextInput>,
    /// The palette card's real chrome, in logical pixels, measured from the laid-out card.
    ///
    /// Zero until the first measurement lands, and the prediction in `panels` covers that frame.
    /// The card is `overflow_hidden` with a flexible list, so any chrome the model leaves out is
    /// taken straight off the list, and the row that used to disappear was the last one.
    palette_chrome: Cell<f32>,
    palette_query: String,
    palette_scope: PaletteScope,
    palette_selection: Option<SharedString>,
    search: Entity<SearchView>,
    search_open: bool,
    search_epoch: Option<u64>,
    search_previous_focus: Option<FocusHandle>,
    service_account_epoch: u64,
    service_account_request: Option<ServiceAccountRequest>,
    service_account_task: Option<Task<()>>,
    #[cfg(test)]
    service_account_future: Option<ServiceAccountFuture>,
    /// Current modal dialog.
    dialog: Option<Dialog>,
    dialog_input_epoch: u64,
    dialog_input_value: String,
    dialog_input_caret: usize,
    /// Focused dialog control. Index 0 is the input or Cancel.
    dialog_focus: usize,
    dialog_focus_handle: FocusHandle,
    dialog_button_focus_handles: Vec<FocusHandle>,
    dialog_previous_focus: Option<FocusHandle>,
    commands: Vec<Command>,
    pods: Entity<PodsView>,
    inspector: Entity<InspectorPanel>,
    inspector_route: Rc<PanelRoute>,
    routes: HashMap<usize, Rc<PanelRoute>>,
    resource_selections: HashMap<usize, InspectorSelection>,
    active_resource_tab: Option<usize>,
    preview_hydration_pending: bool,
    dock_panel: Entity<DockPanel>,
    dock_observation: Subscription,
    window_title: String,
    update_state: UpdateUiState,
    update_actions: Option<UpdateActions>,
    update_strip_expanded: bool,
    update_notice: StartupNotice,
    update_notice_opened: bool,
    update_overlay_action: usize,
    update_overlay_focus: FocusHandle,
    focused: bool,
    connection: ConnectionState,
    health_probe: Option<HealthProbe>,
    connection_machine: StateMachine<CoreConnectionMachine>,
    connection_effects: UnboundedReceiver<CoreConnectionEffect>,
    _health_task: Option<Task<()>>,
    _health_schedule_task: Option<Task<()>>,
    health_started_epoch: Option<u64>,
    latency_tier: LatencyTier,
    latency_auto_paused: HashSet<usize>,
    startup_state: StartupState,
    catalog_state: CatalogState,
    catalog: Option<CatalogHandle>,
    catalog_failure: Option<String>,
    /// Cached catalog state while a refresh runs.
    catalog_stale: Option<bool>,
    cache: Option<Arc<ClusterCache>>,
    _catalog_task: Option<Task<()>>,
    catalog_retry_task: Option<Task<()>>,
    catalog_load_generation: u64,
    catalog_retry_generation: u64,
    catalog_retry_attempt: usize,
    catalog_retry_focus_tree: bool,
    /// Current cluster session.
    session: Option<ClusterSession>,
    /// Generation for rejecting stale asynchronous results.
    session_epoch: u64,
    owned_runtime: Option<Arc<tokio::runtime::Runtime>>,
    reload_in_progress: bool,
    registry_reload_callback: Option<Rc<dyn Fn(Arc<ClusterRegistry>)>>,
    kubeconfig_warning: Option<String>,
    #[cfg(test)]
    reload_kubeconfig_path: Option<PathBuf>,
    #[cfg(test)]
    namespace_future: Option<NamespaceFuture>,
    #[cfg(test)]
    apply_future: Option<ApplyFuture>,
    #[cfg(test)]
    check_future: Option<CheckFuture>,
    #[cfg(test)]
    catalog_future: Option<CatalogFuture>,
    /// Cluster operations used by the UI.
    cluster_handle: Option<ClusterHandle>,
    terminal_services: Option<TerminalServices>,
    /// Hotbar state machine and effect channel.
    hotbar_machine: StateMachine<HotbarMachine>,
    hotbar_effects: UnboundedReceiver<HotbarEffect>,
    hotbar_load_error: Option<String>,
    hotbar_open: bool,
    settings_layout_saved: Option<(bool, bool)>,
    /// Weak shell handle for callbacks.
    shell_weak: WeakEntity<Shell>,
    /// Current transient feedback message.
    toast: Option<Toast>,
    toast_epoch: u64,
    /// Persistent notification history.
    notifications: Vec<Notification>,
    notification_seq: u64,
    status_panel: StatusPanel,
    notification_focus: FocusHandle,
    /// First tab stop of the toolbar.
    top_bar_focus: FocusHandle,
    /// Toolbar tab stop of the context picker.
    top_bar_cluster_focus: FocusHandle,
    /// Toolbar tab stop of the namespace picker.
    top_bar_namespace_focus: FocusHandle,
    /// Toolbar tab stop of the command palette button.
    top_bar_palette_focus: FocusHandle,
    /// Toolbar tab stop of the settings button.
    top_bar_settings_focus: FocusHandle,
    /// Toolbar tab stop of the Inspector toggle.
    top_bar_inspector_focus: FocusHandle,
    /// Toolbar tab stop of the notification bell, which sits at the top edge of the window.
    top_bar_notifications_focus: FocusHandle,
    /// Whether the context picker popover is open.
    cluster_menu_open: bool,
    /// Whether the namespace picker popover is open.
    namespace_menu_open: bool,
    /// Port forward trigger tab stop of the status bar.
    ///
    /// The notification bell and the error count moved to the top bar, so this is the only
    /// control the status bar mounts and the only handle it tracks.
    status_bar_port_forward_focus: FocusHandle,
    /// Tab stop for the status bar's readouts.
    ///
    /// The bar answers four questions — running work, open sessions, notifications, and where
    /// the connection stands — and the metrics are `Role::Status`, which is not a tab stop. So
    /// keyboard navigation walked straight past every number and stopped on the one button, and
    /// the bar's own explanation of why it exists was reachable only by looking at it. One
    /// handle for the group is enough: the metrics are read, not operated.
    status_bar_metrics_focus: FocusHandle,
    notification_previous_focus: Option<FocusHandle>,
    notification_row_focus_handles: Vec<FocusHandle>,
    notification_clear_focus: FocusHandle,
    notifications_scroll: ScrollHandle,
    helm: Option<Helm>,
    helm_state: HelmCapability,
    helm_error: Option<String>,
    metrics_probe: MetricsProbeState,
    _metrics_probe_task: Option<Task<()>>,
    metrics_probe_epoch: u64,
    /// Whether the Inspector is visible and collecting metrics.
    inspector_rendered: bool,
    /// Restores shell focus when the active content unmounts.
    _focus_lost: Option<Subscription>,
    /// Intercepts keys before modal action dispatch.
    _palette_interceptor: Option<Subscription>,
    _keymap_observer: Option<Subscription>,
    /// The window box the last frame was at, so a frame that only repaints does not go
    /// looking for the display it is on.
    window_geometry_seen: Option<gpui_kit::Bounds<Pixels>>,
    /// The display and box last written to `layout.json`.
    window_geometry_saved: Option<(String, crate::settings::layout::WindowGeometry)>,
    /// Whether something `layout.json` holds has changed since it was last written.
    layout_dirty: bool,
    /// The earliest moment the pending write may land, so a drag is one write and not
    /// three hundred.
    layout_save_after: Option<std::time::Instant>,
    /// The characters typed towards a row, shared by the two lists that answer to them.
    type_ahead: TypeAhead,
    /// The pending "the connection has gone quiet" notice.
    _slow_task: Option<Task<()>>,
    /// Which notice is current, so a replaced one cannot write.
    slow_generation: u64,
}

impl Shell {
    /// Build a shell with a fake data source.
    ///
    /// Test-only, and deliberately so: `Shell::build` would otherwise have to keep a
    /// session-less arm alive in shipped builds, and the only way to reach it is this.
    /// Production starts at [`Self::with_cluster`], which always has a session.
    #[cfg(test)]
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::build(None, cx)
    }

    /// Build a shell for the supplied cluster session.
    pub fn with_cluster(session: ClusterSession, cx: &mut Context<Self>) -> Self {
        Self::build(Some(session), cx)
    }

    /// Replace the current session and rebind session-bound views.
    pub fn replace_session(&mut self, session: ClusterSession, cx: &mut Context<Self>) -> bool {
        self.apply_session(session, cx)
    }

    pub fn set_registry_reload_callback(&mut self, callback: Rc<dyn Fn(Arc<ClusterRegistry>)>) {
        self.registry_reload_callback = Some(callback);
    }

    /// The status bar's readout tab stop.
    ///
    /// The bar's metrics are `Role::Status`, and a status role is not a tab stop, so the numbers
    /// the bar exists to report were skipped by keyboard navigation. The bar's renderer attaches
    /// this handle to the group; the shell owns it because the shell owns the bar's focus order.
    pub fn status_bar_metrics_focus(&self) -> &FocusHandle {
        &self.status_bar_metrics_focus
    }

    fn build(session: Option<ClusterSession>, cx: &mut Context<Self>) -> Self {
        let session = session.map(select_startup_session);
        // `UI-REDESIGN` L11: the window opens the way it was left. A file that cannot be read
        // is reported once and then ignored, because the alternative — a window that opens
        // wrong and says so every frame — is worse than a window that opens at the default.
        let (layout, complaint) = crate::settings::layout::load();
        if let Some(complaint) = &complaint {
            eprintln!("k8s-gpui: {complaint}");
        }

        // A real session loads the tree after the catalog. A demo shell starts populated.
        let tree = match &session {
            Some(session) => ResourceTree::empty(session.cluster_name().unwrap_or("No Context")),
            None => ResourceTree::demo(),
        };
        let collapsed = tree.default_collapsed();
        let remembered_inspector_width = layout
            .panels
            .inspector_width
            .unwrap_or_else(right_width_default)
            .clamp(right_width_min(), right_width_limit());
        let collapsed_saved = remembered_collapsed();
        let has_session = session.is_some();
        let startup_state = StartupState::from_session(session.as_ref());
        // Keep failed context entries after usable entries.
        let clusters: Vec<SharedString> = match &session {
            Some(session) => session
                .registry()
                .map(|registry| cluster_names(registry))
                .filter(|clusters| !clusters.is_empty())
                .unwrap_or_else(|| {
                    vec![SharedString::from(
                        session.cluster_name().unwrap_or("No Context"),
                    )]
                }),
            None => tree.cluster_names(),
        };
        // An unavailable session has no active context.
        let active_cluster = session
            .as_ref()
            .and_then(ClusterSession::cluster_name)
            .and_then(|name| clusters.iter().position(|cluster| cluster.as_ref() == name))
            .unwrap_or(if has_session { clusters.len() } else { 0 });
        let active_cluster_name: SharedString = clusters
            .get(active_cluster)
            .cloned()
            .unwrap_or_else(|| SharedString::from(""));
        let namespace_state = match &session {
            Some(ClusterSession::Ready { .. }) => NamespaceState::Loading,
            Some(ClusterSession::Unavailable { reason, .. }) => {
                NamespaceState::Failed(reason.clone())
            }
            None => NamespaceState::Ready(Vec::new()),
        };
        let kubeconfig_warning = session
            .as_ref()
            .and_then(ClusterSession::registry)
            .and_then(|registry| kubeconfig_source_warning(registry));

        let (source_factory, health_probe, catalog, catalog_failure) = match &session {
            Some(session) => {
                // The initial namespace scope includes every namespace.
                let factory = session.pods_factory(None);
                let catalog = session.catalog();
                match session {
                    ClusterSession::Ready {
                        registry, cluster, ..
                    } => (
                        factory,
                        registry.get(*cluster).map(|cluster| HealthProbe {
                            receiver: cluster.health(),
                            latency: cluster.latency(),
                        }),
                        catalog,
                        None,
                    ),
                    ClusterSession::Unavailable { reason, .. } => (
                        factory,
                        None,
                        catalog,
                        if matches!(&startup_state, StartupState::Loading) {
                            None
                        } else {
                            Some(reason.clone())
                        },
                    ),
                }
            }
            #[cfg(test)]
            None => (fake_source_factory(), None, None, None),
            // `Shell::new` is the only caller of this arm and is `#[cfg(test)]`, so a
            // shipped build always arrives here with a session.
            #[cfg(not(test))]
            None => panic!("the session-less shell is test-only"),
        };
        let connection = initial_connection_state(session.as_ref(), &startup_state);
        let catalog_state = initial_catalog_state(session.as_ref(), &startup_state);
        let (connection_effects_tx, connection_effects) = unbounded_channel();
        let connection_machine = CoreConnectionMachine::new(connection_effects_tx).state_machine();

        let cluster_handle = session.as_ref().and_then(ClusterSession::cluster_handle);
        let cache = session.as_ref().and_then(ClusterSession::cluster_cache);
        let ops = cluster_handle
            .clone()
            .map(|handle| Rc::new(handle) as Rc<dyn ObjectOps>);
        let inspector = cx.new(|cx| InspectorPanel::new(cx));
        let dock_panel = cx.new(|cx| {
            // On launch there is no stream to show, so an unfolded Dock is 200px of
            // the window saying "nothing here". `UI-SPEC` §16.2 keeps the 28px strip
            // resident so the Dock can be open with a collapsed body; the strip is
            // the whole of the Dock until the reader asks for the body, which
            // `open_dock` does on every path that means "give me this stream".
            let mut dock = DockPanel::new(cx);
            dock.set_body_collapsed(true, cx);
            dock
        });
        let pods_route = Rc::new(PanelRoute::default());
        // PodsView keeps the Inspector selection in sync.
        let pods = cx.new(|cx| {
            let mut view = PodsView::new_with_inspector(
                source_factory,
                None,
                routed_inspector_binding(
                    inspector.clone(),
                    pods_route.clone(),
                    InspectorSession::default(),
                ),
                cx,
            );
            view.set_ops(ops);
            view
        });

        let shell_weak = cx.weak_entity();
        let dock_observation = cx.observe(&dock_panel, |_, _, cx| cx.notify());
        install_metrics_retry_handler(&shell_weak, &inspector, cx);
        let search = cx
            .new(|cx| SearchView::new(session.as_ref().and_then(SearchExecutor::from_session), cx));
        if let Some(session) = &session {
            // Bind Inspector and logs to the current session.
            let source = session.inspector_source();
            let identity = InspectorSession {
                id: 0,
                cluster_id: session.cluster_id(),
            };
            inspector.update(cx, |panel, _| {
                panel.set_source(source);
                panel.set_session_identity(identity);
            });
            let factory = session.log_factory();
            dock_panel.update(cx, |panel, cx| panel.set_log_factory(factory, cx));
            let metrics = metrics_handle_for(session);
            inspector.update(cx, |panel, cx| {
                panel.set_metrics_source(metrics, MetricsProbeState::Checking, cx)
            });
        }
        install_inspector_apply_handler(&shell_weak, &inspector, cx);
        install_dock_notice_handler(&shell_weak, &dock_panel, cx);
        install_view_handlers(&shell_weak, &pods, ResourceSpec::pods(), 0, cx);
        {
            let shell = shell_weak.clone();
            search.update(cx, |view, _| {
                view.set_open_handler(Rc::new(move |hit, window, cx| {
                    let shell = shell.clone();
                    window.defer(cx, move |window, cx| {
                        let Some(shell_entity) = shell.upgrade() else {
                            return;
                        };
                        let Some(epoch) = shell_entity.read(cx).search_epoch else {
                            return;
                        };
                        window.defer(cx, move |window, cx| {
                            shell
                                .update(cx, |shell, cx| {
                                    shell.open_search_result(hit, epoch, window, cx)
                                })
                                .ok();
                        });
                    });
                }));
            });
            let shell = shell_weak.clone();
            search.update(cx, |view, _| {
                view.set_action_handler(Rc::new(move |hit, action, window, cx| {
                    let shell = shell.clone();
                    window.defer(cx, move |window, cx| {
                        let Some(shell_entity) = shell.upgrade() else {
                            return;
                        };
                        let Some(epoch) = shell_entity.read(cx).search_epoch else {
                            return;
                        };
                        window.defer(cx, move |window, cx| {
                            shell
                                .update(cx, |shell, cx| {
                                    shell.run_search_result_action(hit, epoch, action, window, cx)
                                })
                                .ok();
                        });
                    });
                }));
            });
            let shell = shell_weak.clone();
            search.update(cx, |view, _| {
                view.set_close_handler(Rc::new(move |window, cx| {
                    let shell = shell.clone();
                    window.defer(cx, move |window, cx| {
                        window.defer(cx, move |window, cx| {
                            shell
                                .update(cx, |shell, cx| shell.close_search(window, cx))
                                .ok();
                        });
                    });
                }));
            });
        }
        // Observe keymap reload and preset changes.
        let keymap_observer = cx.has_global::<keymap::KeymapStatus>().then(|| {
            cx.observe_global::<keymap::KeymapStatus>(|this, cx| {
                this.on_keymap_status_changed(cx);
            })
        });
        // A missing registry produces an empty Hotbar.
        let (hotbar_effects_tx, hotbar_effects) = unbounded_channel();
        let mut hotbar_machine = HotbarMachine::new(hotbar_effects_tx).state_machine();
        let (hotbar, hotbar_load_error) = match session.as_ref().and_then(ClusterSession::registry)
        {
            Some(registry) => {
                let (hotbar, error) = ClusterSession::load_hotbar(registry);
                (hotbar, error.map(|error| error.to_string()))
            }
            None => (Hotbar::default(), None),
        };
        hotbar_machine.handle(&HotbarEvent::Load(hotbar));
        let palette_shell = shell_weak.clone();
        let palette_input = cx.new(|cx| {
            TextInput::new(
                "Search Commands",
                cx,
                move |text, cx| {
                    let shell = palette_shell.clone();
                    let text = text.to_owned();
                    cx.defer(move |cx| {
                        shell
                            .update(cx, |shell, cx| {
                                shell.palette_query = text;
                                shell.palette_selection = None;
                                shell.palette_note = None;
                                shell.sync_palette_selection();
                                shell.reveal_palette_selection();
                                cx.notify();
                            })
                            .ok();
                    });
                },
            )
            .with_accessibility(
                "Command Search",
                "Type to filter commands. Use Up and Down to navigate. Press Enter to run. Press Escape to close.",
                "Clear Command Search",
            )
            .with_width(px(
                panels::PALETTE_WIDTH - f32::from(design::space::XL),
            ))
        });
        let tree_filter_shell = shell_weak.clone();
        let tree_filter_input = cx.new(|cx| {
            TextInput::new("Filter Resources", cx, move |text, cx| {
                let shell = tree_filter_shell.clone();
                let text = text.to_owned();
                cx.defer(move |cx| {
                    shell
                        .update(cx, |shell, cx| shell.set_tree_filter(&text, cx))
                        .ok();
                });
            })
            .with_accessibility(
                "Filter Resources",
                "Type to filter resources. Press Escape to clear the filter.",
                "Clear Resource Filter",
            )
            .with_width(px(
                f32::from(design::size::SIDEBAR_MIN) - f32::from(design::space::SM) * 2.
            ))
        });
        tree_filter_input.read(cx).focus_handle(cx).tab_index(1);

        let mut shell = Self {
            cluster_handle,
            terminal_services: None,
            hotbar_machine,
            hotbar_effects,
            hotbar_load_error,
            hotbar_open: true,
            settings_layout_saved: None,
            shell_weak,
            toast: None,
            toast_epoch: 0,
            notifications: Vec::new(),
            notification_seq: 0,
            status_panel: StatusPanel::None,
            top_bar_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            top_bar_cluster_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            top_bar_namespace_focus: cx.focus_handle().tab_stop(true).tab_index(3),
            top_bar_palette_focus: cx.focus_handle().tab_stop(true).tab_index(4),
            top_bar_settings_focus: cx.focus_handle().tab_stop(true).tab_index(5),
            top_bar_inspector_focus: cx.focus_handle().tab_stop(true).tab_index(6),
            top_bar_notifications_focus: cx.focus_handle().tab_stop(true).tab_index(7),
            cluster_menu_open: false,
            namespace_menu_open: false,
            status_bar_port_forward_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            status_bar_metrics_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            notification_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            notification_previous_focus: None,
            notification_row_focus_handles: (0..NOTIFICATION_CAPACITY)
                .map(|index| {
                    cx.focus_handle()
                        .tab_stop(true)
                        .tab_index(index as isize + NOTIFICATION_CLEAR_TAB_INDEX)
                })
                .collect(),
            notification_clear_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(NOTIFICATION_CLEAR_TAB_INDEX),
            notifications_scroll: ScrollHandle::new(),
            helm: None,
            helm_state: HelmCapability::Checking,
            helm_error: None,
            metrics_probe: MetricsProbeState::Checking,
            _metrics_probe_task: None,
            metrics_probe_epoch: 0,
            inspector_rendered: false,
            focus_handle: cx.focus_handle(),
            focus_active_view_pending: false,
            left_divider_focus: cx.focus_handle().tab_stop(true),
            right_divider_focus: cx.focus_handle().tab_stop(true),
            dock_divider_focus: cx.focus_handle().tab_stop(true),
            palette_focus_handle: cx.focus_handle(),
            palette_previous_focus: None,
            sidebar_previous_focus: None,
            inspector_previous_focus: None,
            dock_previous_focus: None,
            palette_note: None,

            pending_action: None,
            tree,
            tree_filter: String::new(),
            tree_filter_input,
            tree_focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            // The Hotbar is the first sidebar focus stop.
            hotbar_focus_handle: cx.focus_handle().tab_stop(true),
            hotbar_bank_focus: cx.focus_handle().tab_stop(true).tab_index(0isize),
            hotbar_bank_open: false,
            resource_menu_open: false,
            hotbar_add_focus: cx.focus_handle().tab_stop(true).tab_index(0isize),
            hotbar_hide_focus: cx.focus_handle().tab_stop(true).tab_index(0isize),
            hotbar_slot_cursor: 0,
            hotbar_scroll: ScrollHandle::new(),
            tree_scroll: ScrollHandle::new(),
            tabs_scroll: ScrollHandle::new(),
            pinned_tabs_scroll: ScrollHandle::new(),

            center_tabs_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            center_tabs_cursor: None,
            center_tab_drag: None,
            tab_context_menu: None,
            tab_context_menu_position: gpui_kit::point(px(0.0), px(0.0)),
            tab_context_menu_previous_focus: None,
            tree_context_menu: None,
            tree_context_menu_position: gpui_kit::point(px(0.0), px(0.0)),
            tree_context_menu_previous_focus: None,
            viewport_width: MIN_LAYOUT_WIDTH,
            viewport_height: design::size::WINDOW_MIN.1,
            tree_cursor: Some(0),
            collapsed,
            collapsed_saved,
            collapsed_seen: HashSet::new(),
            selected_node: None,
            clusters,
            active_cluster,
            // The namespace the reader left this cluster on. Every other cluster still opens on
            // "All namespaces", because one remembered namespace for the window is a worse
            // default than none: a reader who works in two namespaces would get whichever one
            // happened to be current last.
            namespace: layout
                .cluster(active_cluster_name.as_str())
                .namespace
                .filter(|namespace| !namespace.is_empty())
                .map_or_else(|| SharedString::from(ALL_NAMESPACES), SharedString::from),
            namespace_state,
            namespace_by_cluster: HashMap::new(),
            namespaces_by_cluster: HashMap::new(),
            _namespace_task: None,
            sidebar_open: layout
                .cluster(active_cluster_name.as_str())
                .sidebar_open
                .unwrap_or(true),
            tabs: vec![
                CenterTab {
                    content: TabContent::Resource,
                    kind: SharedString::from("Pod"),
                    identity: Some(GroupVersionKind::gvk("", "v1", "Pod")),
                    title: SharedString::from("Pods"),
                    icon: design::kind_icon("Pod"),
                    entry: None,
                    resource: None,
                    pinned: false,
                    preview: None,
                },
                // `secondary-2` is SwitchTab(1), so the registry carries Deployments at index 1
                // and the window opens on Pods at index 0. Overview is not a registry slot: it
                // is opened on demand from the sidebar row, which is the only place a reader
                // goes looking for it. A startup tab that nothing opens is a view somebody has
                // to close before they can see the table they came for.
                CenterTab {
                    content: TabContent::Resource,
                    kind: SharedString::from("Deployment"),
                    identity: Some(GroupVersionKind::gvk("apps", "v1", "Deployment")),
                    title: SharedString::from("Deployments"),
                    icon: design::kind_icon("Deployment"),
                    entry: None,
                    resource: None,
                    pinned: false,
                    preview: None,
                },
            ],
            views: vec![None, None],
            open_tabs: vec![0],
            active_tab: 0,
            inspector_open: layout.panels.inspector_open.unwrap_or(false),
            // `UI-SPEC.md` §16.2 and §2.5 both say the Dock's 28px label strip is resident —
            // "标签条常驻 28px（body 折叠成 0 也在）" — because otherwise there is nothing on
            // screen to switch to and logs, the one panel a reader reaches for during an
            // incident, are invisible until they already know the chord. Below 760px the body
            // folds to 0 on its own (`panels::dock`), so a short window still opens on the
            // table. A remembered choice still wins, which is `UI-REDESIGN.md` L11.
            dock_open: layout.panels.dock_open.unwrap_or(true),
            left_width: layout
                .cluster(active_cluster_name.as_str())
                .sidebar_width
                .unwrap_or_else(left_width_default)
                .clamp(left_width_min(), left_width_limit()),
            right_width: remembered_inspector_width,
            right_width_chosen: remembered_inspector_width,
            dock_height: layout
                .panels
                .dock_height
                .unwrap_or(DOCK_HEIGHT_DEFAULT)
                .clamp(dock_height_min(), dock_height_limit()),
            drag: None,
            palette_open: false,
            palette_scroll: ScrollHandle::new(),
            palette_input,
            palette_chrome: Cell::new(0.0),
            palette_query: String::new(),
            palette_scope: PaletteScope::Commands,
            palette_selection: None,
            search,
            search_open: false,
            search_epoch: None,
            search_previous_focus: None,
            service_account_epoch: 0,
            service_account_request: None,
            service_account_task: None,
            #[cfg(test)]
            service_account_future: None,
            dialog: None,
            dialog_input_epoch: 0,
            dialog_input_value: String::new(),
            dialog_input_caret: 0,
            dialog_focus: 0,
            dialog_focus_handle: cx.focus_handle().tab_stop(false),
            // Four stops cover the widest dialog, the port-forward pair with two fields.
            dialog_button_focus_handles: (0..4)
                .map(|index| cx.focus_handle().tab_stop(true).tab_index(index as isize))
                .collect(),
            dialog_previous_focus: None,
            commands: demo_commands_with_capabilities(HelmCapability::Checking, false),
            pods,
            inspector,
            inspector_route: Rc::new(PanelRoute::default()),
            routes: HashMap::from([(0, pods_route)]),
            resource_selections: HashMap::new(),
            active_resource_tab: Some(0),
            preview_hydration_pending: false,
            dock_panel,
            dock_observation,
            window_title: String::new(),
            update_state: UpdateUiState::new(UpdatePhase::Unsupported)
                .with_error(UPDATER_UNAVAILABLE_REASON),
            update_actions: None,
            update_strip_expanded: true,
            update_notice: StartupNotice::default(),
            update_notice_opened: false,
            update_overlay_action: 0,
            update_overlay_focus: cx.focus_handle().tab_stop(true).tab_index(6isize),
            focused: false,

            connection,
            health_probe,
            connection_machine,
            connection_effects,
            _health_task: None,
            _health_schedule_task: None,
            health_started_epoch: None,
            latency_tier: LatencyTier::Local,
            latency_auto_paused: HashSet::new(),
            startup_state,
            catalog_state,
            catalog,
            catalog_failure,
            catalog_stale: None,
            cache,
            _catalog_task: None,
            catalog_retry_task: None,
            catalog_load_generation: 0,
            catalog_retry_generation: 0,
            catalog_retry_attempt: 0,
            catalog_retry_focus_tree: false,
            session,
            session_epoch: 0,
            owned_runtime: None,
            reload_in_progress: false,
            registry_reload_callback: None,
            kubeconfig_warning,
            #[cfg(test)]
            reload_kubeconfig_path: None,
            #[cfg(test)]
            namespace_future: None,
            #[cfg(test)]
            apply_future: None,
            #[cfg(test)]
            check_future: None,
            #[cfg(test)]
            catalog_future: None,
            _focus_lost: None,
            _palette_interceptor: None,
            _keymap_observer: keymap_observer,
            window_geometry_seen: None,
            window_geometry_saved: None,
            layout_dirty: false,
            layout_save_after: None,
            type_ahead: TypeAhead::default(),
            _slow_task: None,
            slow_generation: 0,
        };
        shell.sync_resource_inspector_route();
        shell.rebuild_commands();
        if matches!(shell.namespace_state, NamespaceState::Loading) {
            shell.start_namespace_load(cx);
        }
        shell
    }

    /// The Settings window title names the visible pane, so two tabs stay tellable apart.
    fn sync_settings_tab_title(&mut self, cx: &mut Context<Self>) {
        let index = self.active_tab;
        let Some(TabView::Settings(view)) = self.views.get(index).and_then(Option::as_ref) else {
            return;
        };
        let title = view.read(cx).pane_title();
        if let Some(tab) = self.tabs.get_mut(index)
            && tab.title.as_str() != title
        {
            tab.title = SharedString::from(title);
        }
    }

    fn active_view_title(&self) -> SharedString {
        if self.open_tabs.is_empty() {
            return SharedString::from("No Open View");
        }
        self.tabs
            .get(self.active_tab)
            .map(|tab| tab.title.clone())
            .unwrap_or_else(|| SharedString::from("Resources"))
    }

    fn active_tab_dirty(&self, cx: &App) -> bool {
        self.editor_source()
            .is_some_and(|panel| panel.read(cx).is_dirty(cx))
    }

    fn sync_preview_metrics_visibility(&mut self, cx: &mut Context<Self>) {
        for (index, slot) in self.views.iter().enumerate() {
            if let Some(TabView::Preview(view)) = slot {
                view.update(cx, |panel, cx| {
                    panel.set_metrics_visible(index == self.active_tab, cx)
                });
            }
        }
    }

    /// Tells the Overview whether it is the tab on screen.
    ///
    /// The Overview reloads the whole cluster on a timer, and a tab nobody is
    /// looking at has no reason to spend seven `list` calls every interval. The
    /// view's tick already honours this; without the shell setting it, a
    /// backgrounded Overview keeps the cluster busy for a screen it cannot show.
    fn sync_overview_visibility(&mut self, cx: &mut Context<Self>) {
        for (index, slot) in self.views.iter().enumerate() {
            if let Some(TabView::Overview(view)) = slot {
                view.update(cx, |view, _| view.set_visible(index == self.active_tab));
            }
        }
    }

    fn active_preview(&self) -> Option<Entity<InspectorPanel>> {
        match self.views.get(self.active_tab).and_then(Option::as_ref) {
            Some(TabView::Preview(view)) => Some(view.clone()),
            _ => None,
        }
    }

    fn editor_source(&self) -> Option<Entity<InspectorPanel>> {
        match self.tabs.get(self.active_tab).map(|tab| tab.content) {
            Some(TabContent::Preview) => self.active_preview(),
            Some(TabContent::Resource) => Some(self.inspector.clone()),
            _ => None,
        }
    }

    fn active_resource_selection(&self) -> Option<&InspectorSelection> {
        self.active_resource_tab
            .and_then(|tab| self.resource_selections.get(&tab))
    }

    fn active_tab_is_resource(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .is_some_and(|tab| tab.content == TabContent::Resource)
    }

    fn set_resource_selection(&mut self, tab: usize, selection: Option<InspectorSelection>) {
        match selection {
            Some(selection) => {
                self.resource_selections.insert(tab, selection);
            }
            None => {
                self.resource_selections.remove(&tab);
            }
        }
    }

    fn resource_route(&mut self, index: usize) -> Rc<PanelRoute> {
        self.routes
            .entry(index)
            .or_insert_with(|| Rc::new(PanelRoute::default()))
            .clone()
    }

    fn sync_resource_inspector_route(&self) {
        for (index, route) in &self.routes {
            let active = *index == self.active_tab
                && self
                    .tabs
                    .get(*index)
                    .is_some_and(|tab| tab.content == TabContent::Resource);
            route.active.set(active);
        }
    }

    fn inspector_session(&self) -> InspectorSession {
        InspectorSession {
            id: self.session_epoch,
            cluster_id: self.session.as_ref().and_then(ClusterSession::cluster_id),
        }
    }

    fn push_panel_selection(
        &self,
        panel: &Entity<InspectorPanel>,
        route: &Rc<PanelRoute>,
        selection: Option<InspectorSelection>,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = selection.as_ref()
            && route.record(selection, panel.read(cx).is_applying())
        {
            let identity = route.identity(self.inspector_session());
            panel.update(cx, |panel, _| panel.set_session_identity(identity));
        }
        panel.update(cx, |panel, cx| match selection {
            Some(selection) => panel.set_selection(Some(selection), cx),
            None => panel.set_selection(None, cx),
        });
    }

    fn sync_active_inspector_selection(&mut self, cx: &mut Context<Self>) {
        self.sync_resource_inspector_route();
        let selection = match self.active_preview() {
            Some(view) => view.read(cx).current_selection(cx),
            None if self.active_tab_is_resource() => self.active_resource_selection().cloned(),
            None => None,
        };
        let route = self.inspector_route.clone();
        self.push_panel_selection(&self.inspector, &route, selection, cx);
        self.lock_mirror_without_a_preview(cx);
    }

    fn hydrate_following_preview(&mut self, cx: &mut Context<Self>) {
        if !self.preview_hydration_pending {
            return;
        }
        let Some(index) = self.following_preview_index() else {
            self.preview_hydration_pending = false;
            return;
        };
        let Some(selection) = self.active_resource_selection().cloned() else {
            return;
        };
        self.configure_following_preview_tab(index, &selection);
        self.preview_hydration_pending = !self.set_preview_selection(index, Some(selection), cx);
    }

    fn configure_following_preview_tab(&mut self, index: usize, selection: &InspectorSelection) {
        let object = &selection.object;
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.kind = SharedString::from(object.resource.kind.clone());
        tab.identity = Some(GroupVersionKind::gvk(
            &object.resource.group,
            &object.resource.version,
            &object.resource.kind,
        ));
        tab.title = preview_title(
            object.resource.kind.as_str(),
            Some(object.name.as_str()),
            false,
        );
        tab.icon = design::kind_icon(object.resource.kind.as_str());
        tab.entry = None;
        tab.resource = Some(object.resource.clone());
    }

    fn lock_mirror_without_a_preview(&self, cx: &mut Context<Self>) {
        if self.active_tab_is_resource() {
            return;
        }
        let inspector = self.inspector.clone();
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let Some(window) = window else {
                return;
            };
            let inspector = inspector.clone();
            window
                .update(cx, move |_, window, cx| {
                    inspector.update(cx, |panel, cx| panel.set_editable(false, window, cx));
                })
                .ok();
        });
    }

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        normalize_open_tabs(&mut self.open_tabs, &self.tabs);
        if index >= self.tabs.len() {
            return false;
        }
        let tab_changed = self.active_tab != index;
        if tab_changed && self.active_tab_dirty(cx) {
            self.toast(
                "Apply or revert the unsaved YAML before leaving this tab.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return false;
        }
        if tab_changed {
            self.invalidate_service_account_request();
            self.sync_settings_layout_for_tab(index);
        }
        if !self.open_tabs.contains(&index) {
            self.open_tabs.push(index);
        }
        self.active_tab = index;
        self.center_tabs_cursor = Some(index);
        if self.active_tab_is_resource() {
            self.active_resource_tab = Some(index);
        }
        self.sync_resource_inspector_route();
        self.ensure_tab_view(index, cx);
        self.hydrate_following_preview(cx);
        self.sync_active_inspector_selection(cx);
        self.sync_preview_metrics_visibility(cx);
        self.sync_overview_visibility(cx);
        self.sync_resource_view_pauses(cx);
        self.sync_tree_selection();
        self.reveal_active_tab();
        true
    }

    /// Create a tab view on first activation.
    fn ensure_tab_view(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == 0 || index >= self.tabs.len() {
            return;
        }
        if self.views.get(index).is_some_and(Option::is_some) {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        match tab.content {
            TabContent::Resource => {
                let Some(entry) = tab.entry.clone() else {
                    return;
                };
                let Some(session) = self.session.clone() else {
                    return;
                };
                let spec =
                    ResourceSpec::new(entry.kind.clone(), tab.title.clone(), entry.namespaced())
                        .with_resource(entry.to_api_resource());
                let factory = session.source_factory(&entry, self.namespace_scope());
                let inspector = self.inspector.clone();
                let route = self.resource_route(index);
                let base = self.inspector_session();
                let ops = self.object_ops();
                let view_spec = spec.clone();
                let view = cx.new(|cx| {
                    let mut view = PodsView::for_resource(
                        factory,
                        None,
                        routed_inspector_binding(inspector, route, base),
                        view_spec,
                        cx,
                    );
                    view.set_ops(ops);
                    view
                });
                self.install_resource_view(&view, &spec, index, cx);
                self.views[index] = Some(TabView::Resource(view));
            }
            TabContent::Preview => {
                let source = self
                    .session
                    .as_ref()
                    .map(|session| session.inspector_source());
                let identity = self.inspector_session();
                let metrics = self.session.as_ref().and_then(metrics_handle_for);
                let metrics_state = self.metrics_probe.clone();
                let view = cx.new(|cx| InspectorPanel::new(cx));
                self.resource_route(index);
                install_metrics_retry_handler(&self.shell_weak, &view, cx);
                view.update(cx, |panel, cx| {
                    if let Some(source) = source {
                        panel.set_source(source);
                    }
                    panel.set_session_identity(identity);
                    panel.set_metrics_source(metrics, metrics_state, cx);
                    panel.set_metrics_visible(index == self.active_tab, cx);
                });
                install_inspector_apply_handler(&self.shell_weak, &view, cx);
                self.views[index] = Some(TabView::Preview(view));
            }
            TabContent::Overview => {
                let handle = self.overview_handle();
                let metrics_available = self.metrics_probe.is_available();
                let view = cx.new(|cx| {
                    let mut view = OverviewView::new(handle, metrics_available, cx);
                    view.refresh(cx);
                    view
                });
                // The Overview's biggest numbers are buttons, and the panel only draws one as
                // clickable when the host installs a route to it. Without a route a chip is
                // drawn as a figure instead: a control that goes nowhere is a lie. `UI-REDESIGN`
                // §3.5 makes the chip the route to the rows behind its count, so all three slots
                // are installed and the panel is told which kinds the catalog can actually open.
                let shell = self.shell_weak.clone();
                let route = |kind: &'static str| {
                    let shell = shell.clone();
                    Rc::new(move |window: &mut Window, cx: &mut App| {
                        let Some(shell) = shell.upgrade() else {
                            return;
                        };
                        shell.update(cx, |shell, cx| {
                            shell.open_kind_needing_attention(kind, window, cx)
                        });
                    }) as crate::panels::overview::ShowProblemsCallback
                };
                view.update(cx, |view, _| {
                    view.set_show_problems_callback(Some(route("Pod")));
                    view.set_overview_routes(crate::panels::overview::OverviewRoutes {
                        pods: Some(route("Pod")),
                        nodes: Some(route("Node")),
                        // "workloads" is not one kind, so there is no single table to open.
                        // The chip stays a figure rather than sending someone to Deployments
                        // when the count was mostly StatefulSets.
                        workloads: None,
                    })
                });
                self.views[index] = Some(TabView::Overview(view));
            }
            TabContent::Forwards => {
                let shell = self.shell_weak.clone();
                let new_callback: NewForwardCallback = Rc::new(move |window, cx| {
                    let Some(shell) = shell.upgrade() else {
                        return;
                    };
                    shell.update(cx, |shell, cx| {
                        shell.close_status_panel(window, cx);
                        shell.request_new_port_forward(window, cx);
                    });
                });
                let dock = self.dock_panel.clone();
                let view = cx.new(|cx| ForwardsView::new(dock, Some(new_callback), cx));
                self.views[index] = Some(TabView::Forwards(view));
            }
            TabContent::Helm => {
                let services = self.helm_services();
                let capability = self.helm_state;
                let reason = self.helm_error.clone();
                let view = cx.new(|cx| HelmView::new(services.bind(), services.handle.clone(), cx));
                view.update(cx, |view, cx| {
                    view.set_capability(capability, reason.clone(), cx)
                });
                self.install_helm_handlers(&view, cx);
                self.views[index] = Some(TabView::Helm(view));
            }
            TabContent::Settings => {
                let view = cx.new(SettingsView::new);
                self.install_settings_handlers(&view, cx);
                self.views[index] = Some(TabView::Settings(view));
                // Sync capabilities that loaded before this view existed.
                self.sync_settings_capabilities(cx);
            }
        }
    }

    /// Build the Overview data handle for the current session.
    fn overview_handle(&self) -> Option<OverviewHandle> {
        overview_handle_for(self.session.as_ref()?)
    }

    /// Build Helm services for the current session.
    fn helm_services(&self) -> HelmServices {
        let context = self
            .session
            .as_ref()
            .and_then(|session| session.cluster_name().map(str::to_owned));
        let kubeconfig_sources = self
            .session
            .as_ref()
            .and_then(ClusterSession::registry)
            .map(|registry| registry.sources().to_vec())
            .unwrap_or_default();
        HelmServices {
            helm: self.helm.clone(),
            handle: self
                .session
                .as_ref()
                .and_then(ClusterSession::tokio_handle)
                .cloned(),
            context,
            kubeconfig_sources,
        }
    }

    fn rebind_helm_views(&self, cx: &mut Context<Self>) {
        let services = self.helm_services();
        for slot in &self.views {
            let Some(TabView::Helm(view)) = slot else {
                continue;
            };
            self.sync_helm_cluster(view, cx);
            let helm = services.bind();
            let handle = services.handle.clone();
            view.update(cx, |view, cx| view.set_client(helm, handle, cx));
        }
    }

    fn install_helm_handlers(&self, view: &Entity<HelmView>, cx: &mut Context<Self>) {
        self.sync_helm_cluster(view, cx);
        let shell = self.shell_weak.clone();
        let entity = view.clone();
        view.update(cx, |view, _| {
            view.on_action_requested(move |action, window, cx| {
                let shell = shell.clone();
                let entity = entity.clone();
                window.defer(cx, move |window, cx| {
                    shell
                        .update(cx, |shell, cx| {
                            shell.open_helm_confirm(action, entity, window, cx)
                        })
                        .ok();
                });
            });
        });
        let shell = self.shell_weak.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, severity, cx| {
                let shell = shell.clone();
                cx.defer(move |cx| {
                    shell
                        .update(cx, |shell, cx| {
                            shell.notify(message, severity, None, cx);
                        })
                        .ok();
                });
            });
        });
    }

    fn install_settings_handlers(&self, view: &Entity<SettingsView>, cx: &mut Context<Self>) {
        view.update(cx, |view, _| {
            view.set_theme_handler(apply_theme_choice);
        });
        let update_state = self.update_state.clone();
        let update_actions = self.update_actions.clone();
        view.update(cx, |view, cx| {
            view.set_update_state(update_state, cx);
            view.set_update_actions(update_actions, cx);
        });
        let shell = self.shell_weak.clone();
        view.update(cx, |view, _| {
            view.set_keymap_handlers(
                move |window, cx| {
                    let shell = shell.clone();
                    window.defer(cx, move |window, cx| {
                        shell
                            .update(cx, |shell, cx| {
                                shell.command_create_or_show_keymap(window, cx)
                            })
                            .ok();
                    });
                },
                move |_window, cx| {
                    keymap::reload(cx).ok();
                },
            );
        });
        let shell = self.shell_weak.clone();
        view.update(cx, |view, _| {
            view.set_notice_handler(move |message, severity, cx| {
                let shell = shell.clone();
                cx.defer(move |cx| {
                    shell
                        .update(cx, |shell, cx| {
                            shell.notify(message, severity, None, cx);
                        })
                        .ok();
                });
            });
        });
    }

    /// Install action handlers and the current Ask menu on a resource table.
    fn install_resource_view(
        &self,
        view: &Entity<PodsView>,
        spec: &ResourceSpec,
        source_tab: usize,
        cx: &mut Context<Self>,
    ) {
        install_view_handlers(&self.shell_weak, view, spec.clone(), source_tab, cx);
    }

    fn visible_tree_rows(&self) -> Vec<TreeRow> {
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .map(|name| name.as_ref())
            .unwrap_or("cluster");
        self.tree
            .rows_for_cluster_filtered(cluster, &self.collapsed, &self.tree_filter)
    }

    fn render_tree_with_filter(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let empty = matches!(self.catalog_state, CatalogState::Ready)
            && self.tree_filter_active()
            && self.visible_tree_rows().is_empty();
        v_flex()
            .id("resource-tree-panel")
            .debug_selector(|| "resource-tree-panel".to_owned())
            .flex_none()
            .w(px(self.left_width))
            .h_full()
            .min_w(px(0.0))
            // `role::surface_chrome`, by name. The panel used to read the theme field
            // `colors.panel_background` directly — the field *behind* the role, which holds the
            // same value in both shipped themes and would have moved the moment a theme moved
            // the editor plane. The title bar, this panel, the rail, the Dock and the status bar
            // are one chrome band and they all read the one role now.
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            .child(
                // The filter field, the group head and the rows share one leading inset
                // (`space::SM`), so the three bands the sidebar is made of start on the same
                // edge. The field also owns the band's top padding: it used to have
                // `pt(space::SM)` and no bottom padding, which left it flush against the group
                // head's own rule — two adjacent bands with nothing between them.
                h_flex()
                    .flex_none()
                    .px(design::space::SM)
                    .py(design::space::SM)
                    .child(self.tree_filter_input.clone()),
            )
            .when(empty, |this| {
                this.child(
                    h_flex()
                        .id("tree-filter-empty")
                        .role(Role::Status)
                        .px(design::space::MD)
                        .py(design::space::SM)
                        .child(
                            Label::new(
                                "No matching resources. Clear the filter to see all resources.",
                            )
                            .text_size(design::text::BODY)
                            .text_color(design::colors(cx).text_muted),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.render_tree(window, cx)),
            )
    }

    pub fn set_tree_filter(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.tree_filter == query {
            return;
        }
        let current_id = self
            .visible_tree_rows()
            .get(self.tree_cursor.unwrap_or(0))
            .map(|row| row.id.clone());
        self.tree_filter = query.to_owned();
        let rows = self.visible_tree_rows();
        self.tree_cursor = current_id
            .and_then(|id| rows.iter().position(|row| row.id == id))
            .or_else(|| (!rows.is_empty()).then_some(0));
        self.tree_scroll
            .scroll_to_item(self.tree_cursor.unwrap_or(0));
        cx.notify();
    }

    pub fn clear_tree_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_filter_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.set_tree_filter("", cx);
    }

    fn tree_filter_active(&self) -> bool {
        !self.tree_filter.trim().is_empty()
    }

    fn sync_tree_selection(&mut self) {
        // With no view open there is no active tab to mirror into the tree.
        if self.open_tabs.is_empty() {
            return;
        }
        let Some((content, kind, identity)) = self
            .tabs
            .get(self.active_tab)
            .map(|tab| (tab.content, tab.kind.clone(), tab.identity.clone()))
        else {
            return;
        };
        if content != TabContent::Resource && content != TabContent::Overview {
            self.selected_node = None;
            return;
        }
        let row = self.visible_tree_rows().into_iter().find(|row| {
            if content == TabContent::Overview {
                return row.resource_kind.as_ref() == Some(&kind);
            }
            row.resource_gvk
                .as_ref()
                .map_or(row.resource_kind.as_ref() == Some(&kind), |row_identity| {
                    Some(row_identity) == identity.as_ref()
                })
        });
        self.selected_node = row.map(|row| Selection {
            id: row.id,
            label: row.label,
        });
    }

    fn center_tab_cursor(&self) -> usize {
        self.center_tabs_cursor
            .filter(|index| self.open_tabs.contains(index))
            .or_else(|| {
                self.open_tabs
                    .contains(&self.active_tab)
                    .then_some(self.active_tab)
            })
            .or_else(|| self.open_tabs.first().copied())
            .unwrap_or(self.active_tab)
    }

    fn tab_action_target(&self, window: &Window, cx: &App) -> usize {
        if self.center_tabs_focus.is_focused(window)
            || self.center_tabs_focus.contains_focused(window, cx)
        {
            return self.center_tab_cursor();
        }
        self.active_tab
    }

    fn reveal_center_tab(&self, index: usize) {
        let Some(position) = self.open_tabs.iter().position(|tab| *tab == index) else {
            return;
        };
        if self.center_tab_is_pinned(index) {
            self.pinned_tabs_scroll.scroll_to_item(position);
        } else {
            self.tabs_scroll.scroll_to_item(
                position.saturating_sub(pinned_open_tab_count(&self.open_tabs, &self.tabs)),
            );
        }
    }

    fn on_center_tabs_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.center_tabs_focus.contains_focused(window, cx) {
            return;
        }
        let current = self.center_tab_cursor();
        let Some(position) = self.open_tabs.iter().position(|tab| *tab == current) else {
            return;
        };
        match event.keystroke.key.as_str() {
            "left" | "up" => {
                if let Some(index) = position
                    .checked_sub(1)
                    .and_then(|position| self.open_tabs.get(position).copied())
                {
                    self.center_tabs_cursor = Some(index);
                }
            }
            "right" | "down" => {
                if let Some(index) = self.open_tabs.get(position + 1).copied() {
                    self.center_tabs_cursor = Some(index);
                }
            }
            "home" => {
                if let Some(index) = self.open_tabs.first().copied() {
                    self.center_tabs_cursor = Some(index);
                }
            }
            "end" => {
                if let Some(index) = self.open_tabs.last().copied() {
                    self.center_tabs_cursor = Some(index);
                }
            }
            "enter" | "return" | "space" | " " => {
                self.activate_tab(current, cx);
                cx.stop_propagation();
                return;
            }
            "contextmenu" | "menu" => {
                self.open_tab_context_menu(current, None, window, cx);
                cx.stop_propagation();
                return;
            }
            "f10" if event.keystroke.modifiers.shift => {
                self.open_tab_context_menu(current, None, window, cx);
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        if let Some(index) = self.center_tabs_cursor {
            self.reveal_center_tab(index);
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn center_tab_is_pinned(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| tab.pinned)
    }

    fn normalize_open_tabs(&mut self) {
        normalize_open_tabs(&mut self.open_tabs, &self.tabs);
    }

    fn toggle_center_tab_pin(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.pinned = !tab.pinned;
        self.normalize_open_tabs();
        if let Some(position) = self.open_tabs.iter().position(|tab| *tab == index) {
            let boundary = pinned_open_tab_count(&self.open_tabs, &self.tabs);
            self.open_tabs.remove(position);
            let insertion = if self.center_tab_is_pinned(index) {
                boundary.saturating_sub(1)
            } else {
                boundary
            };
            self.open_tabs.insert(insertion, index);
            self.normalize_open_tabs();
            self.center_tabs_cursor = Some(index);
        }
        self.reveal_center_tab(index);
        cx.notify();
    }

    fn move_center_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.center_tab_cursor();
        if !self.open_tabs.contains(&source) {
            return;
        }
        if source != self.active_tab && self.active_tab_dirty(cx) {
            self.toast(
                "Apply or revert the unsaved YAML in the active tab before you move another tab."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        self.normalize_open_tabs();
        if !move_open_tab_within_group(&mut self.open_tabs, &self.tabs, source, delta) {
            return;
        }
        self.center_tabs_cursor = Some(source);
        self.reveal_center_tab(source);
        window.focus(&self.center_tabs_focus, cx);
        cx.notify();
    }

    fn close_other_center_tabs(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_tab_close(TabCloseRequest::Others(index), window, cx);
    }

    fn close_all_center_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_tab_close(TabCloseRequest::All, window, cx);
    }

    fn tab_close_targets(&self, request: TabCloseRequest) -> Vec<usize> {
        match request {
            TabCloseRequest::One(index) if self.open_tabs.contains(&index) => {
                if self.open_tabs.len() == 1 {
                    self.open_tabs.clone()
                } else {
                    vec![index]
                }
            }
            TabCloseRequest::One(_) => Vec::new(),
            TabCloseRequest::Others(index) => self
                .open_tabs
                .iter()
                .copied()
                .filter(|tab| *tab != index && !self.tabs.get(*tab).is_some_and(|tab| tab.pinned))
                .collect(),
            TabCloseRequest::All => self
                .open_tabs
                .iter()
                .copied()
                .filter(|tab| !self.tabs.get(*tab).is_some_and(|tab| tab.pinned))
                .collect(),
        }
    }

    fn tab_close_is_dirty(&self, targets: &[usize], cx: &App) -> bool {
        (targets.contains(&self.active_tab) && self.active_tab_dirty(cx))
            || targets.iter().any(|index| {
                matches!(
                    self.views.get(*index).and_then(Option::as_ref),
                    Some(TabView::Preview(view)) if view.read(cx).is_dirty(cx)
                )
            })
    }

    fn request_tab_close(
        &mut self,
        request: TabCloseRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.tab_close_targets(request);
        if targets.is_empty() {
            // The tab bar is not mounted while nothing is open, so it must not take focus.
            if self.open_tabs.is_empty() {
                self.restore_shell_focus_if_empty(window, cx);
            } else {
                window.focus(&self.center_tabs_focus, cx);
            }
            return;
        }
        if self.tab_close_is_dirty(&targets, cx) {
            self.close_tab_context_menu(window, cx);
            self.dialog = Some(Dialog::ConfirmTabClose { request });
            self.open_dialog_focus(window, cx);
            return;
        }
        self.perform_tab_close(request, window, cx);
    }

    fn discard_dirty_tabs(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        if targets.contains(&self.active_tab)
            && let Some(source) = self.editor_source()
        {
            source.update(cx, |panel, cx| panel.discard_dirty(cx));
        }
        for index in targets {
            if let Some(TabView::Preview(view)) = self.views.get(*index).and_then(Option::as_ref) {
                view.update(cx, |panel, cx| panel.discard_dirty(cx));
            }
        }
    }

    fn perform_tab_close(
        &mut self,
        request: TabCloseRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tabs_focused = self.center_tabs_focus.is_focused(window)
            || self.center_tabs_focus.contains_focused(window, cx);
        let targets = self.tab_close_targets(request);
        if targets.is_empty() {
            return;
        }
        if self.settings_active() && targets.contains(&self.active_tab) {
            self.leave_settings_layout();
        }
        self.discard_dirty_tabs(&targets, cx);
        let active_position = self
            .open_tabs
            .iter()
            .position(|tab| *tab == self.active_tab)
            .unwrap_or(0);
        self.open_tabs.retain(|tab| !targets.contains(tab));
        for index in targets {
            self.latency_auto_paused.remove(&index);
            self.resource_selections.remove(&index);
            if let Some(route) = self.routes.get(&index) {
                route.reset();
            }
            if index != 0
                && let Some(view) = self.views.get_mut(index)
            {
                *view = None;
            }
        }
        if self.open_tabs.is_empty() {
            // Closing the last view leaves the shell on an explicit empty surface. Opening a
            // substitute tab here would put cluster stats in front of someone who just asked
            // for an empty work surface.
            self.active_tab = 0;
            self.center_tabs_cursor = None;
        } else if !self.open_tabs.contains(&self.active_tab) {
            self.active_tab = self
                .open_tabs
                .get(active_position)
                .or_else(|| self.open_tabs.get(active_position.saturating_sub(1)))
                .copied()
                .or_else(|| self.open_tabs.first().copied())
                .unwrap_or(0);
            self.center_tabs_cursor = Some(self.active_tab);
        }
        if !self.open_tabs.is_empty() {
            self.sync_resource_inspector_route();
            self.ensure_tab_view(self.active_tab, cx);
            self.sync_active_inspector_selection(cx);
        }
        self.sync_preview_metrics_visibility(cx);
        self.sync_overview_visibility(cx);
        self.sync_resource_view_pauses(cx);
        self.sync_tree_selection();
        self.reveal_active_tab();
        if tabs_focused && !self.open_tabs.is_empty() {
            window.focus(&self.center_tabs_focus, cx);
        } else {
            self.focus_active_view_and_clear_pending(window, cx);
        }
        cx.notify();
    }

    fn update_center_tab_drag(
        &mut self,
        source: usize,
        target: usize,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.normalize_open_tabs();
        let insertion = bounded_tab_drop_gap(&self.open_tabs, &self.tabs, source, target, after);
        let next = CenterTabDragState { source, insertion };
        if self.center_tab_drag == Some(next) {
            return;
        }
        self.center_tab_drag = Some(next);
        cx.notify();
    }

    /// Track a drop past the last tab without dropping a tab level insertion.
    fn update_center_tab_drop_at_end(
        &mut self,
        source: usize,
        target: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(insertion) =
            bounded_tab_drop_gap(&self.open_tabs, &self.tabs, source, target, true)
        else {
            return;
        };
        if self
            .center_tab_drag
            .is_some_and(|drag| drag.source == source && drag.insertion.is_some())
        {
            return;
        }
        self.center_tab_drag = Some(CenterTabDragState {
            source,
            insertion: Some(insertion),
        });
        cx.notify();
    }

    fn clear_center_tab_drag(&mut self, cx: &mut Context<Self>) {
        if self.center_tab_drag.take().is_some() {
            cx.notify();
        }
    }

    fn reorder_center_tab(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(insertion) = self.center_tab_drag.and_then(|drag| drag.insertion) else {
            return;
        };
        self.center_tab_drag = None;
        if source != self.active_tab && self.active_tab_dirty(cx) {
            self.toast(
                "Apply or revert the unsaved YAML before you move this tab.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        if !reorder_open_tabs_within_group(&mut self.open_tabs, &self.tabs, source, insertion) {
            cx.notify();
            return;
        }
        self.normalize_open_tabs();
        if !self.activate_tab(source, cx) {
            cx.notify();
            return;
        }
        // The drag can move the active tab, so the Settings layout follows the new tab.
        self.sync_settings_layout_for_tab(source);
        self.reveal_center_tab(source);
        window.focus(&self.center_tabs_focus, cx);
        cx.notify();
    }

    fn close_tab_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.request_tab_close(TabCloseRequest::One(index), window, cx);
    }

    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self
            .open_tabs
            .iter()
            .position(|tab| *tab == self.active_tab)
        else {
            return;
        };
        let next = (position as isize + delta).rem_euclid(self.open_tabs.len() as isize) as usize;
        let Some(tab) = self.open_tabs.get(next).copied() else {
            return;
        };
        if tab != self.active_tab {
            if !self.activate_tab(tab, cx) {
                return;
            }
        } else {
            self.center_tabs_cursor = Some(tab);
        }
        self.sync_tree_selection();
        self.reveal_active_tab();
        self.focus_active_view_and_clear_pending(window, cx);
        cx.notify();
    }

    /// Focus target of the active tab, or `None` when the center does not mount it.
    fn active_view_focus(&self, cx: &Context<Self>) -> Option<FocusHandle> {
        if self.connection_failure_is_primary(cx) {
            return None;
        }
        // The empty work surface mounts no view, so nothing in the center can take focus.
        if self.open_tabs.is_empty() {
            return None;
        }
        // Tab zero always renders the shared Pods table.
        if self.active_tab == 0
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| tab.kind.as_ref() == "Pod")
        {
            return Some(self.pods.read(cx).table_focus_handle(cx));
        }
        match self.views.get(self.active_tab).and_then(Option::as_ref) {
            Some(TabView::Resource(view)) => Some(view.read(cx).table_focus_handle(cx)),
            Some(TabView::Overview(view)) => Some(view.read(cx).focus_handle()),
            Some(TabView::Forwards(view)) => Some(view.read(cx).focus_handle()),
            Some(TabView::Helm(view)) => Some(view.read(cx).focus_handle()),
            Some(TabView::Settings(view)) => Some(view.read(cx).focus_handle()),
            Some(TabView::Preview(view)) => Some(view.read(cx).focus_handle()),
            None => None,
        }
    }

    /// Move focus to the active tab content.
    fn focus_active_view(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(focus) = self.active_view_focus(cx) else {
            return false;
        };
        window.focus(&focus, cx);
        true
    }

    fn restore_shell_focus_if_empty(&self, window: &mut Window, cx: &mut Context<Self>) {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }
    }

    fn focus_active_view_and_clear_pending(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let focused = self.focus_active_view(window, cx);
        self.focus_active_view_pending = false;
        if !focused {
            self.restore_shell_focus_if_empty(window, cx);
        }
        focused
    }

    fn on_tree_click(&mut self, row: TreeRow, cx: &mut Context<Self>) {
        // The resource search is a modal over the whole window, and its scrim now covers the
        // sidebar with everything else. The overlay's own `occlude` only reaches its own bounds,
        // so without this a click on the tree behind it switched tabs and closed nothing.
        if self.search_open {
            return;
        }
        match row.kind {
            TreeRowKind::Overview => {
                self.open_special_tab(TabContent::Overview, "Overview", IconName::Monitor, cx);
            }
            TreeRowKind::Kind => {
                let Some(kind) = row.resource_kind.clone() else {
                    return;
                };
                let identity = row.resource_gvk.clone().or_else(|| {
                    row.entry
                        .as_ref()
                        .map(ResourceEntry::identity)
                        .or_else(|| known_resource_identity(&kind))
                });
                if self.open_resource_tab(kind, row.label.clone(), identity, row.entry, cx) {
                    self.selected_node = Some(Selection {
                        id: row.id,
                        label: row.label,
                    });
                }
            }
            _ => self.toggle_row(row.id, cx),
        }
        cx.notify();
    }

    /// Opens the resource table for `kind` with the problems filter already on.
    ///
    /// The Overview's chips count the rows behind a number, so the destination is
    /// the rows, not the table. Opening the table alone is what made a button
    /// report 9,900 problems and then show every pod in the cluster.
    fn open_kind_needing_attention(
        &mut self,
        kind: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .map_or("cluster", |name| name.as_ref());
        let Some(row) = self
            .tree
            .rows_for_cluster(cluster, &HashSet::new())
            .into_iter()
            .find(|row| row.resource_kind.as_deref() == Some(kind))
        else {
            self.toast(
                format!(
                    "{kind}s are not in the resource catalog. Refresh resources, then try again."
                ),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let Some(entry) = row.entry else {
            self.toast(
                format!(
                    "{kind}s have no catalog entry to open. Refresh resources, then try again."
                ),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if !self.open_resource_tab(
            SharedString::from(kind),
            // The tree already labels the kind row the way the tab should read.
            row.label,
            Some(entry.identity()),
            Some(entry),
            cx,
        ) {
            return;
        }
        if let Some(view) = self.active_resource_view() {
            view.update(cx, |view, cx| view.set_problems_only(true, cx));
        }
        self.focus_active_view_and_clear_pending(window, cx);
        cx.notify();
    }

    /// Open or activate a resource tab.
    fn open_resource_tab(
        &mut self,
        kind: SharedString,
        title: SharedString,
        identity: Option<GroupVersionKind>,
        entry: Option<ResourceEntry>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .current_kind()
            .is_some_and(|(current, _)| current != kind)
            && self.active_tab_dirty(cx)
        {
            self.toast(
                "Apply or revert the unsaved YAML before opening another resource kind.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return false;
        }
        let index = match self.tabs.iter().position(|tab| {
            tab.content == TabContent::Resource && tab.identity == identity && tab.kind == kind
        }) {
            Some(index) => {
                self.tabs[index].identity = identity.clone();
                self.tabs[index].entry = entry.clone();
                self.tabs[index].resource = entry.as_ref().map(ResourceEntry::to_api_resource);
                index
            }
            None => {
                let icon = design::kind_icon(kind.as_ref());
                self.tabs.push(CenterTab {
                    content: TabContent::Resource,
                    kind,
                    identity,
                    title,
                    icon,
                    resource: entry.as_ref().map(ResourceEntry::to_api_resource),
                    entry,
                    pinned: false,
                    preview: None,
                });
                self.views.push(None);
                self.tabs.len() - 1
            }
        };
        if !self.activate_tab(index, cx) {
            return false;
        }
        cx.notify();
        true
    }

    fn following_preview_index(&self) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.content == TabContent::Preview
                && matches!(tab.preview.as_ref(), Some(PreviewState::FollowSelection))
        })
    }

    fn fixed_preview_index(&self, identity: &PreviewIdentity) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.content == TabContent::Preview
                && matches!(tab.preview.as_ref(), Some(PreviewState::Fixed(existing)) if existing == identity)
        })
    }

    fn configure_preview_tab(&mut self, index: usize, row: &Row, spec: &ResourceSpec, fixed: bool) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.kind = spec.kind.clone();
        tab.identity = spec.resource.as_ref().map(|resource| {
            GroupVersionKind::gvk(&resource.group, &resource.version, &resource.kind)
        });
        tab.title = preview_title(spec.kind.as_ref(), row.obj.metadata.name.as_deref(), fixed);
        tab.icon = design::kind_icon(spec.kind.as_ref());
        tab.entry = None;
        tab.resource = spec.resource.clone();
    }

    fn selection_for_row(row: &Row, resource: &ApiResource) -> Option<InspectorSelection> {
        let name = row.obj.metadata.name.clone()?;
        let uid = row.obj.metadata.uid.clone()?;
        let yaml = serde_yaml_ng::to_string(row.obj.as_ref()).ok()?;
        Some(InspectorSelection {
            object: ObjectRef {
                resource: resource.clone(),
                namespace: row.obj.metadata.namespace.clone(),
                name,
                uid,
            },
            yaml,
        })
    }

    fn preview_view(&self, index: usize) -> Option<Entity<InspectorPanel>> {
        match self.views.get(index).and_then(Option::as_ref) {
            Some(TabView::Preview(view)) => Some(view.clone()),
            _ => None,
        }
    }

    fn set_preview_selection(
        &self,
        index: usize,
        selection: Option<InspectorSelection>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.preview_view(index) else {
            return false;
        };
        let Some(route) = self.routes.get(&index).cloned() else {
            return false;
        };
        self.push_panel_selection(&view, &route, selection, cx);
        true
    }

    fn set_preview_content(
        &self,
        index: usize,
        row: &Row,
        resource: Option<ApiResource>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = self.preview_view(index) else {
            return false;
        };
        let selection = resource
            .as_ref()
            .and_then(|resource| Self::selection_for_row(row, resource));
        match selection {
            Some(selection) => self.set_preview_selection(index, Some(selection), cx),
            None => {
                let yaml = serde_yaml_ng::to_string(row.obj.as_ref()).ok();
                view.update(cx, |panel, cx| panel.set_yaml(yaml, cx));
                true
            }
        }
    }

    fn clear_preview_content(&self, index: usize, cx: &mut Context<Self>) {
        let Some(view) = self.preview_view(index) else {
            return;
        };
        view.update(cx, |panel, cx| panel.set_yaml(None, cx));
    }

    fn invalidate_service_account_request(&mut self) {
        if self.service_account_request.is_some() {
            self.service_account_epoch = self.service_account_epoch.wrapping_add(1);
            self.service_account_request = None;
            self.service_account_task = None;
        }
    }

    fn service_account_request_is_current(
        &self,
        request: &ServiceAccountRequest,
        cx: &App,
    ) -> bool {
        request.session_epoch == self.session_epoch
            && request.source_tab == self.active_tab
            && request.namespace == self.namespace
            && self
                .resource_view(request.source_tab)
                .as_ref()
                .map(Entity::entity_id)
                == Some(request.source_view.entity_id())
            && request.source_view.read(cx).selection_ref(cx).as_ref() == Some(&request.pod)
    }

    fn finish_service_account(
        &mut self,
        epoch: u64,
        result: Result<DynamicObject, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.service_account_request.as_ref() else {
            return;
        };
        if request.epoch != epoch || !self.service_account_request_is_current(request, cx) {
            self.service_account_request = None;
            return;
        }
        let target = request.target.clone();
        self.service_account_request = None;
        let object = match result {
            Ok(object) => object,
            Err(reason) => {
                eprintln!(
                    "k8s-gpui: open Service Account {}/{} failed: {reason}",
                    target.namespace, target.name
                );
                self.notify(
                    "The app did not open the Service Account. Check that it exists and that you have access, then try again."
                        .to_owned(),
                    design::Severity::Error,
                    Some(reason),
                    cx,
                );
                return;
            }
        };
        if !service_account_object_matches(&object, &target) {
            let detail = service_account_object_detail(&object);
            eprintln!("k8s-gpui: Service Account identity mismatch: {detail}");
            self.notify(
                "The Service Account response did not match the selected Pod. Refresh the Pod list, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some(detail),
                cx,
            );
            return;
        }
        let spec = ResourceSpec::service_accounts();
        let row = Row {
            obj: Arc::new(object),
            cells: Vec::new(),
        };
        let Some(identity) = PreviewIdentity::from_row(&spec, &row) else {
            self.notify(
                "The Service Account response did not match the selected Pod. Refresh the Pod list, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some("The Service Account response has no preview identity.".to_owned()),
                cx,
            );
            return;
        };
        self.pin_preview(&row, &spec, identity, None, cx);
    }

    fn open_service_account_target(
        &mut self,
        target: ServiceAccountTarget,
        source_view: Entity<PodsView>,
        cx: &mut Context<Self>,
    ) {
        self.service_account_request = None;
        self.service_account_task = None;
        self.service_account_epoch = self.service_account_epoch.wrapping_add(1);
        let epoch = self.service_account_epoch;
        if self.active_resource_view().as_ref().map(Entity::entity_id)
            != Some(source_view.entity_id())
        {
            return;
        }
        let Some(pod) = source_view.read(cx).selection_ref(cx) else {
            self.notify(
                "Select a Pod before opening its Service Account.".to_owned(),
                design::Severity::Warning,
                None,
                cx,
            );
            return;
        };
        if source_view.read(cx).service_account_target(cx) != Some(Ok(target.clone())) {
            return;
        }
        #[cfg(test)]
        let future = self
            .service_account_future
            .as_ref()
            .map(|future| future(target.clone()));
        #[cfg(not(test))]
        let future: Option<crate::session::OpsFuture<DynamicObject>> = None;
        let future = match future {
            Some(future) => future,
            None => {
                let Some(handle) = self.cluster_handle.clone() else {
                    self.notify(
                        "The app did not open the Service Account because no cluster connection is available. Connect to a cluster, then try again."
                            .to_owned(),
                        design::Severity::Error,
                        None,
                        cx,
                    );
                    return;
                };
                handle.read_service_account(&target)
            }
        };
        self.service_account_request = Some(ServiceAccountRequest {
            epoch,
            session_epoch: self.session_epoch,
            source_tab: self.active_tab,
            source_view,
            pod,
            namespace: self.namespace.clone(),
            target,
        });
        self.service_account_task = Some(cx.spawn(async move |this, cx| {
            let result = future.await;
            this.update(cx, |shell, cx| {
                shell.finish_service_account(epoch, result, cx)
            })
            .ok();
        }));
    }

    fn open_service_account(
        &mut self,
        _: &OpenServiceAccount,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .current_kind()
            .is_none_or(|(kind, _)| kind.as_ref() != "Pod")
        {
            self.notify(
                "Open a Pod view before opening a Service Account.".to_owned(),
                design::Severity::Warning,
                None,
                cx,
            );
            return;
        }
        let Some(view) = self.active_resource_view() else {
            return;
        };
        let target = match view.read(cx).service_account_target(cx) {
            Some(Ok(target)) => target,
            Some(Err(reason)) => {
                self.notify(reason.to_owned(), design::Severity::Error, None, cx);
                return;
            }
            None => {
                self.notify(
                    "Select a Pod before opening its Service Account.".to_owned(),
                    design::Severity::Warning,
                    None,
                    cx,
                );
                return;
            }
        };
        self.open_service_account_target(target, view, cx);
    }

    fn on_row_selection(
        &mut self,
        row: Option<Row>,
        spec: ResourceSpec,
        source_tab: usize,
        cx: &mut Context<Self>,
    ) {
        let selection = row.as_ref().and_then(|row| {
            spec.resource
                .as_ref()
                .and_then(|resource| Self::selection_for_row(row, resource))
        });
        self.set_resource_selection(source_tab, selection);
        if self.active_resource_tab != Some(source_tab) {
            return;
        }
        self.invalidate_service_account_request();
        if let Some(index) = self.following_preview_index() {
            match row {
                Some(row) => {
                    self.configure_preview_tab(index, &row, &spec, false);
                    self.preview_hydration_pending =
                        !self.set_preview_content(index, &row, spec.resource.clone(), cx);
                }
                None => {
                    self.reset_following_preview_tab(index);
                    self.clear_preview_content(index, cx);
                }
            }
        }
        if source_tab == self.active_tab {
            self.sync_active_inspector_selection(cx);
        }
        cx.notify();
    }

    fn reset_following_preview_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.title = SharedString::from("Preview");
            tab.kind = SharedString::from("Preview");
            tab.identity = None;
            tab.resource = None;
        }
    }

    fn clear_resource_selections(&mut self, cx: &mut Context<Self>) {
        self.resource_selections.clear();
        if let Some(index) = self.following_preview_index() {
            self.reset_following_preview_tab(index);
            self.clear_preview_content(index, cx);
        }
        self.preview_hydration_pending = self.following_preview_index().is_some();
        self.sync_active_inspector_selection(cx);
    }

    fn remember_row_selection(&mut self, source: Option<usize>, row: &Row, spec: &ResourceSpec) {
        let Some(source) = source else {
            return;
        };
        self.set_resource_selection(
            source,
            spec.resource
                .as_ref()
                .and_then(|resource| Self::selection_for_row(row, resource)),
        );
    }

    fn open_preview(
        &mut self,
        row: &Row,
        spec: &ResourceSpec,
        source: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let (index, created) = match self.following_preview_index() {
            Some(index) => (index, false),
            None => {
                self.tabs.push(CenterTab {
                    content: TabContent::Preview,
                    kind: spec.kind.clone(),
                    identity: spec.resource.as_ref().map(|resource| {
                        GroupVersionKind::gvk(&resource.group, &resource.version, &resource.kind)
                    }),
                    title: preview_title(spec.kind.as_ref(), None, false),
                    icon: design::kind_icon(spec.kind.as_ref()),
                    entry: None,
                    resource: spec.resource.clone(),
                    pinned: false,
                    preview: Some(PreviewState::FollowSelection),
                });
                self.views.push(None);
                (self.tabs.len() - 1, true)
            }
        };
        self.routes.entry(index).or_default();
        if !self.activate_tab(index, cx) {
            if created {
                self.tabs.pop();
                self.views.pop();
                self.routes.remove(&index);
            }
            return;
        }
        self.configure_preview_tab(index, row, spec, false);
        self.remember_row_selection(source, row, spec);
        self.preview_hydration_pending =
            !self.set_preview_content(index, row, spec.resource.clone(), cx);
        self.sync_active_inspector_selection(cx);
        self.focus_active_view_pending = true;
        self.defer_focus_active_view(cx);
        cx.notify();
    }

    fn pin_preview(
        &mut self,
        row: &Row,
        spec: &ResourceSpec,
        identity: PreviewIdentity,
        source: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.fixed_preview_index(&identity) {
            if !self.activate_tab(index, cx) {
                return;
            }
            self.remember_row_selection(source, row, spec);
            if !self
                .preview_view(index)
                .is_some_and(|view| view.read(cx).is_dirty(cx))
            {
                self.set_preview_content(index, row, spec.resource.clone(), cx);
            }
            self.preview_hydration_pending = false;
            self.sync_active_inspector_selection(cx);
            self.focus_active_view_pending = true;
            self.defer_focus_active_view(cx);
            cx.notify();
            return;
        }
        let (index, created) = match self.following_preview_index() {
            Some(index) => (index, false),
            None => {
                self.tabs.push(CenterTab {
                    content: TabContent::Preview,
                    kind: spec.kind.clone(),
                    identity: Some(identity.gvk.clone()),
                    title: preview_title(
                        spec.kind.as_ref(),
                        row.obj.metadata.name.as_deref(),
                        true,
                    ),
                    icon: design::kind_icon(spec.kind.as_ref()),
                    entry: None,
                    resource: spec.resource.clone(),
                    pinned: false,
                    preview: None,
                });
                self.views.push(None);
                (self.tabs.len() - 1, true)
            }
        };
        self.routes.entry(index).or_default();
        if !self.activate_tab(index, cx) {
            if created {
                self.tabs.pop();
                self.views.pop();
                self.routes.remove(&index);
            }
            return;
        }
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.preview = Some(PreviewState::Fixed(identity));
        }
        self.configure_preview_tab(index, row, spec, true);
        self.remember_row_selection(source, row, spec);
        self.preview_hydration_pending =
            !self.set_preview_content(index, row, spec.resource.clone(), cx);
        self.sync_active_inspector_selection(cx);
        self.focus_active_view_pending = true;
        self.defer_focus_active_view(cx);
        cx.notify();
    }

    /// Opens a row's detail view.
    ///
    /// Reached from Enter, from the row menu's `Open Details`, and from
    /// Describe. A click deliberately does not come through here: clicking a row
    /// selects it, and only selects it, so scanning a list never replaces the
    /// table with the editor. A row with a stable identity pins its own tab, so
    /// two rows never fight over one preview.
    fn open_row_details(
        &mut self,
        row: Row,
        spec: ResourceSpec,
        source_tab: usize,
        cx: &mut Context<Self>,
    ) {
        if source_tab != self.active_tab && !self.activate_tab(source_tab, cx) {
            return;
        }
        let source = Some(source_tab);
        match PreviewIdentity::from_row(&spec, &row) {
            Some(identity) => self.pin_preview(&row, &spec, identity, source, cx),
            None => self.open_preview(&row, &spec, source, cx),
        }
    }

    /// Open or activate a special tab.
    fn open_special_tab(
        &mut self,
        content: TabContent,
        title: &'static str,
        icon: IconName,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.active_tab_dirty(cx)
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| tab.content != content)
        {
            self.toast(
                "Apply or revert the unsaved YAML before leaving this tab.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return false;
        }
        let index = match self.tabs.iter().position(|tab| tab.content == content) {
            Some(index) => index,
            None => {
                self.tabs.push(CenterTab {
                    content,
                    kind: SharedString::from(title),
                    identity: None,
                    title: SharedString::from(title),
                    icon,
                    entry: None,
                    resource: None,
                    pinned: false,
                    preview: None,
                });
                self.views.push(None);
                self.tabs.len() - 1
            }
        };
        if !self.activate_tab(index, cx) {
            return false;
        }
        cx.notify();
        true
    }

    fn settings_active(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .is_some_and(|tab| tab.content == TabContent::Settings)
    }

    fn enter_settings_layout(&mut self) {
        if self.settings_layout_saved.is_none() {
            self.settings_layout_saved = Some((self.sidebar_open, self.hotbar_open));
        }
        // Settings draws its own category column inside the content area, so the resource tree
        // stays on screen beside it. It used to be hidden here as well as by
        // `panel_visibility`, which left the sidebar and Inspector toggles disabled with a toast
        // as their only behaviour, and the only way back was closing the Settings tab.
        self.hotbar_open = false;
        self.constrain_layout(self.viewport_width, self.viewport_height);
    }

    fn leave_settings_layout(&mut self) {
        if let Some((sidebar_open, hotbar_open)) = self.settings_layout_saved.take() {
            self.sidebar_open = sidebar_open;
            self.hotbar_open = hotbar_open;
            self.constrain_layout(self.viewport_width, self.viewport_height);
        }
    }

    fn sync_settings_layout_for_tab(&mut self, index: usize) {
        let was_settings = self.settings_active();
        let is_settings = self
            .tabs
            .get(index)
            .is_some_and(|tab| tab.content == TabContent::Settings);
        match (was_settings, is_settings) {
            (false, true) => self.enter_settings_layout(),
            (true, false) => self.leave_settings_layout(),
            _ => {}
        }
    }

    fn resource_view(&self, index: usize) -> Option<Entity<PodsView>> {
        if index == 0 {
            return Some(self.pods.clone());
        }
        self.views
            .get(index)
            .and_then(Option::as_ref)
            .and_then(TabView::resource)
            .cloned()
    }

    fn sync_resource_view_pauses(&mut self, cx: &mut Context<Self>) {
        if self.latency_tier != LatencyTier::HighLatency {
            let indexes = std::mem::take(&mut self.latency_auto_paused);
            for index in indexes {
                if let Some(view) = self.resource_view(index) {
                    view.update(cx, |view, cx| {
                        view.resume_from_latency(cx);
                    });
                }
            }
            return;
        }

        let active_resource = self
            .tabs
            .get(self.active_tab)
            .filter(|tab| tab.content == TabContent::Resource)
            .map(|_| self.active_tab);
        let resources = self
            .open_tabs
            .iter()
            .copied()
            .filter_map(|index| {
                self.tabs
                    .get(index)
                    .filter(|tab| tab.content == TabContent::Resource)
                    .and_then(|_| self.resource_view(index))
                    .map(|view| (index, view))
            })
            .collect::<Vec<_>>();
        let resource_indices = resources
            .iter()
            .map(|(index, _)| *index)
            .collect::<HashSet<_>>();
        self.latency_auto_paused
            .retain(|index| resource_indices.contains(index));
        for (index, view) in resources {
            if active_resource == Some(index) {
                if self.latency_auto_paused.remove(&index) {
                    view.update(cx, |view, cx| {
                        view.resume_from_latency(cx);
                    });
                }
            } else if view.update(cx, |view, cx| view.pause_for_latency(cx)) {
                self.latency_auto_paused.insert(index);
            } else {
                self.latency_auto_paused.remove(&index);
            }
        }
    }

    fn active_resource_view(&self) -> Option<Entity<PodsView>> {
        // The shared Pods view is mounted by the center, so it is not the active resource
        // view while nothing is open.
        if self.open_tabs.is_empty() {
            return None;
        }
        self.resource_view(self.active_tab)
    }

    fn toggle_row(&mut self, id: SharedString, cx: &mut Context<Self>) {
        if self.tree_filter_active() {
            return;
        }
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.remember_collapsed(cx);
    }

    /// The expansion state this cluster should open with.
    ///
    /// The first time a cluster is seen its state comes from `settings.json` if the reader left
    /// one there, and from the tree's own defaults otherwise. Every later catalog for the same
    /// cluster keeps the state the reader has, so neither the disk cache on startup nor the live
    /// refresh behind it can close the tree under them.
    fn collapsed_for_new_catalog(&mut self, cluster: &str) -> HashSet<SharedString> {
        if !self.collapsed_seen.insert(cluster.to_owned()) {
            return self.collapsed.clone();
        }
        self.collapsed_saved
            .get(cluster)
            .cloned()
            .unwrap_or_else(|| self.tree.default_collapsed())
    }

    /// Keep the saved expansion in step with the tree, and write it out.
    fn remember_collapsed(&mut self, cx: &mut Context<Self>) {
        let Some(cluster) = self.clusters.get(self.active_cluster).cloned() else {
            return;
        };
        self.collapsed_saved
            .insert(cluster.to_string(), self.collapsed.clone());
        persist_collapsed(cx, &self.collapsed_saved);
    }

    fn object_ops(&self) -> Option<Rc<dyn ObjectOps>> {
        self.cluster_handle
            .clone()
            .map(|handle| Rc::new(handle) as Rc<dyn ObjectOps>)
    }

    fn hotbar_visible_for_layout(&self, sidebar_visible: bool) -> bool {
        !self.settings_active() && sidebar_visible && self.hotbar_open
    }

    /// The three panel facts one frame's layout needs, so the row, the width arithmetic and
    /// the divider all answer the same question instead of three near-equal ones.
    fn panel_layout(&self, width: f32) -> (bool, InspectorLayout) {
        // A Preview tab *is* the Inspector: `editor_source` hands the centre the
        // editable copy and `sync_active_inspector_selection` mirrors the selection
        // into `self.inspector`, which `lock_mirror_without_a_preview` then makes
        // read-only. So with a Preview open the right panel is the same object,
        // the same YAML and the same tab set as the centre, locked, and nothing on
        // screen says which of the two follows the selection. The centre already
        // answers that, so the panel gives its width back rather than repeating the
        // answer. Hiding rather than labelling: a label would still leave two copies
        // of the same YAML on screen, one editable and one not, with the reader
        // having to guess which to type into.
        //
        // Settings does not take the panels. It is a center tab like any other, so the sidebar
        // and the Inspector keep their own switches, their own state and their own recovery
        // while it is open; the settings view draws its own category column inside the content.
        let centre_is_the_inspector = self.active_preview().is_some();
        let inspector_open = self.inspector_open && !centre_is_the_inspector;
        let sidebar_visible = self.sidebar_open && width >= MIN_LAYOUT_WIDTH;
        (sidebar_visible, inspector_layout(inspector_open, width))
    }

    fn panel_visibility(&self, width: f32) -> (bool, bool) {
        let (sidebar_visible, inspector) = self.panel_layout(width);
        (sidebar_visible, inspector != InspectorLayout::Closed)
    }

    /// Something `layout.json` holds has changed; the write waits for the reader to stop.
    ///
    /// Trailing edge, not leading: a leading write during a drag records the position the
    /// divider was at when the drag *started*, which is a position nobody chose and which the
    /// next launch would then open at.
    fn layout_changed(&mut self) {
        self.layout_dirty = true;
        self.layout_save_after = Some(std::time::Instant::now() + LAYOUT_SAVE_DELAY);
    }

    /// Notes where the window is, so the next launch can open it there on this display.
    ///
    /// `UI-SPEC` §9.3 asks for geometry remembered per display rather than once for the app,
    /// and the display is the only thing that makes that possible: a box is a pair of
    /// coordinates, and coordinates without a screen are the bug this whole item is about.
    /// The lookup is skipped on every frame that did not move, which is every frame in a
    /// window that is simply running, because enumerating the displays allocates.
    fn record_window_geometry(&mut self, window: &Window, cx: &mut Context<Self>) {
        let bounds = window.bounds();
        if self.window_geometry_seen == Some(bounds) {
            return;
        }
        self.window_geometry_seen = Some(bounds);
        let geometry = crate::settings::layout::WindowGeometry {
            x: f32::from(bounds.origin.x).round() as i32,
            y: f32::from(bounds.origin.y).round() as i32,
            width: f32::from(bounds.size.width).round().max(1.0) as u32,
            height: f32::from(bounds.size.height).round().max(1.0) as u32,
        };
        let key = window.display(&*cx).map(|display| {
            let uuid = display.uuid().ok().map(|uuid| uuid.to_string());
            crate::settings::layout::display_key(uuid.as_deref(), u64::from(display.id()))
        });
        if let Some(key) = key
            && self.window_geometry_saved.as_ref() != Some(&(key.clone(), geometry))
        {
            self.window_geometry_saved = Some((key, geometry));
            self.layout_changed();
        }
    }

    /// Writes the pending layout, once the reader has stopped moving things.
    ///
    /// A drag across a 4K display is a continuous change, and writing a file on every frame
    /// of it would be a hundred writes a second for something the reader will look at once.
    /// The window is the writer because the window is the only thing that knows both halves
    /// of the answer — the panel state lives here and the bounds live on the window — and
    /// there is no hook that fires for either without polling, so this is that poll.
    fn flush_layout(&mut self, cx: &mut Context<Self>) {
        let due = self
            .layout_save_after
            .is_some_and(|after| std::time::Instant::now() >= after);
        if !self.layout_dirty || !due {
            return;
        }
        self.layout_dirty = false;
        self.layout_save_after = None;
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .cloned()
            .unwrap_or_else(|| SharedString::from(""));
        let panels = crate::settings::layout::PanelLayout {
            sidebar_width: Some(self.left_width),
            sidebar_open: Some(self.sidebar_open),
            inspector_width: Some(self.right_width_chosen),
            inspector_open: Some(self.inspector_open),
            dock_open: Some(self.dock_open),
            dock_height: Some(self.dock_height),
        };
        let cluster_layout = crate::settings::layout::ClusterLayout {
            namespace: Some(self.namespace.to_string()),
            sidebar_width: Some(self.left_width),
            sidebar_open: Some(self.sidebar_open),
        };
        let window = self.window_geometry_saved.clone();
        crate::settings::layout::save(cx, move |layout| {
            if let Some((key, geometry)) = window {
                layout.displays.insert(key, geometry);
            }
            layout.panels = panels;
            if !cluster.is_empty() {
                layout.clusters.insert(cluster.to_string(), cluster_layout);
            }
        });
    }

    fn constrain_layout(&mut self, viewport_width: f32, viewport_height: f32) {
        let (sidebar_visible, inspector) = self.panel_layout(viewport_width);
        let (left_width, right_width) = constrained_panel_widths_with_hotbar(
            viewport_width,
            sidebar_visible,
            inspector.takes_width(),
            self.hotbar_visible_for_layout(sidebar_visible),
            self.left_width,
            self.right_width_chosen,
        );
        self.left_width = left_width;
        self.right_width = right_width;
        let update_strip_visible =
            self.update_state.shows_strip() && self.update_state.phase != UpdatePhase::Unsupported;
        self.dock_height = self.dock_height.clamp(
            dock_height_min(),
            dock_height_max_with_strips(
                viewport_height,
                self.kubeconfig_warning.is_some(),
                update_strip_visible,
            ),
        );
    }

    fn status_summary(&self, cx: &App) -> StatusSummary {
        let operations = self
            .active_resource_view()
            .map_or(0, |view| view.read(cx).pending_count(cx));
        let dock = self.dock_panel.read(cx);
        let sessions = dock.terminal_count() + usize::from(dock.request().is_some());
        StatusSummary {
            operations,
            sessions,
            errors: error_notification_count(&self.notifications),
            notifications: self.notifications.len(),
            active_notifications: active_notification_count(&self.notifications),
            port_forwards: dock.forward_summary(),
            // The Dock hides its own chip while it is collapsed, so the state travels with the
            // summary instead of being lost with the panel.
            log_status: dock.log_status_label(),
        }
    }

    fn reveal_active_tab(&self) {
        self.reveal_center_tab(self.active_tab);
    }

    fn remember_panel_focus(window: &Window, cx: &App, destination: &mut Option<FocusHandle>) {
        if destination.is_none()
            && let Some(handle) = window.focused(cx)
        {
            *destination = Some(handle);
        }
    }

    fn dialog_button_focus(&self, index: usize) -> FocusHandle {
        self.dialog_button_focus_handles
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.dialog_focus_handle.clone())
    }

    /// Number of controls the open dialog cycles through with Tab.
    ///
    /// Index 0 is the input or Cancel; the rest are the dialog buttons.
    fn dialog_control_count(&self) -> usize {
        match self.dialog {
            Some(
                Dialog::ConfirmDelete { .. }
                | Dialog::HotbarRemove { .. }
                | Dialog::ConfirmTabClose { .. }
                | Dialog::HelmConfirm { input: None, .. },
            ) => 2,
            // Two fields, Cancel, and Start Port Forward.
            Some(Dialog::PortForward { .. }) => 4,
            Some(
                Dialog::Exec { .. }
                | Dialog::Scale { .. }
                | Dialog::HotbarBankName { .. }
                | Dialog::HelmConfirm { input: Some(_), .. },
            ) => 3,
            None => 0,
        }
    }

    /// Focus index after an arrow key in a dialog whose controls are Cancel and the action.
    ///
    /// The two buttons are a row, with Cancel leading and the action trailing, so an arrow key
    /// stops at the end it points at instead of wrapping to the other one. Wrapping would answer
    /// Up or Left from the safe Cancel default with the destructive button, which is the one move
    /// a stray keypress must never make. The center tabs' roving focus does not wrap either.
    fn two_button_dialog_focus(current: usize, forward: bool) -> usize {
        if forward {
            (current + 1).min(1)
        } else {
            current.saturating_sub(1)
        }
    }

    /// Track which toolbar picker popover is open so the trigger can report it.
    pub(super) fn set_picker_menu_open(
        &mut self,
        kind: PickerKind,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        let changed = match kind {
            PickerKind::Cluster => {
                let changed = self.cluster_menu_open != open;
                self.cluster_menu_open = open;
                changed
            }
            PickerKind::Namespace => {
                let changed = self.namespace_menu_open != open;
                self.namespace_menu_open = open;
                changed
            }
        };
        if changed {
            cx.notify();
        }
    }

    fn is_dialog_focus(&self, handle: &FocusHandle) -> bool {
        *handle == self.dialog_focus_handle
            || self
                .dialog_button_focus_handles
                .iter()
                .any(|dialog_handle| dialog_handle == handle)
    }

    fn open_inspector(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.inspector_open {
            return;
        }
        Self::remember_panel_focus(window, cx, &mut self.inspector_previous_focus);
        self.inspector_open = true;
    }

    /// Make the Dock visible and put the keyboard in it.
    ///
    /// Every entry point - `Open Logs`, `Exec`, a port forward that started - comes here, and the
    /// call means "I asked for the Dock, so the Dock is where the keyboard goes". Returning early
    /// when the Dock was already on screen answered a different question, and the shell answered it
    /// in the wrong direction: with the 28px strip resident (`UI-SPEC.md` §16.2) the Dock is open
    /// on every launch, so `Open Logs` from the palette did nothing at all - no panel, no focus,
    /// and the reader left wondering whether the command had fired. The visibility check belongs
    /// to a caller that only wants to *reveal* the Dock; here the focus is part of the ask.
    fn open_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Self::remember_panel_focus(window, cx, &mut self.dock_previous_focus);
        let was_closed = !self.dock_open;
        self.dock_open = true;
        if was_closed {
            self.constrain_layout(self.viewport_width, self.viewport_height);
        }
        // Every path that reaches here means "I asked for the Dock", and what it
        // asked for is a stream, a shell or a forward — all of which live in the
        // body. `UI-SPEC` §16.2 keeps the 28px strip resident precisely so the
        // Dock can be open with a collapsed body; on launch that is the whole of
        // the Dock, because there is nothing to show yet. Unfolding is part of the
        // ask, so a fresh `Open Logs` does not leave the reader a strip and
        // nothing under it. `set_body_collapsed` only records the request —
        // `body_collapsed` still answers from the viewport, so a window shorter
        // than `DOCK_COLLAPSE_BELOW` folds again and the reader gets the same
        // state their own `⌃` would have produced.
        self.dock_panel
            .update(cx, |dock, cx| dock.set_body_collapsed(false, cx));
        let dock_focus = self.dock_panel.read(cx).focus_handle();
        if !window
            .focused(cx)
            .is_some_and(|focused| focused == dock_focus)
        {
            window.focus(&dock_focus, cx);
        }
    }

    fn restore_panel_focus(
        window: &mut Window,
        cx: &mut App,
        source: &mut Option<FocusHandle>,
        fallback: &FocusHandle,
    ) {
        match source.take() {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(fallback, cx),
        }
    }

    fn close_status_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.status_panel {
            StatusPanel::None => return,
            StatusPanel::Notifications => {
                self.status_panel = StatusPanel::None;
                Self::restore_panel_focus(
                    window,
                    cx,
                    &mut self.notification_previous_focus,
                    &self.focus_handle,
                );
            }
        }
        cx.notify();
    }

    fn open_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status_panel == StatusPanel::Notifications {
            return;
        }
        self.close_status_panel(window, cx);
        self.notification_previous_focus = window
            .focused(cx)
            .filter(|handle| *handle != self.notification_focus);
        self.status_panel = StatusPanel::Notifications;
        window.focus(&self.notification_focus, cx);
        cx.notify();
    }

    fn close_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status_panel == StatusPanel::Notifications {
            self.close_status_panel(window, cx);
        }
    }

    fn retry_connection(&mut self, cx: &mut Context<Self>) {
        if matches!(
            &self.startup_state,
            StartupState::Loading | StartupState::Unavailable(_)
        ) && self
            .session
            .as_ref()
            .and_then(ClusterSession::registry)
            .is_none()
        {
            self.start_kubeconfig_reload(None, cx);
            return;
        }
        if matches!(self.connection, ConnectionState::Reconnecting(_))
            && self.health_probe.is_some()
        {
            self.dispatch_connection_event(CoreConnectionEvent::Retry, cx);
            return;
        }
        if self.active_cluster >= self.clusters.len() {
            if let Some(result) = self
                .session
                .as_ref()
                .map(ClusterSession::reload_hotbar_session)
            {
                match result {
                    Ok(next) => {
                        if let Some(name) = next.cluster_name()
                            && let Some(index) = self
                                .clusters
                                .iter()
                                .position(|cluster| cluster.as_ref() == name)
                        {
                            self.active_cluster = index;
                        }
                        self.apply_session(next, cx);
                        return;
                    }
                    Err(reason) => self.notify(
                        "The Hotbar failed to load. Check the Hotbar file, then try again."
                            .to_owned(),
                        design::Severity::Warning,
                        Some(reason),
                        cx,
                    ),
                }
            }
        } else if let Some(next) = self.session.as_ref().and_then(|session| {
            self.clusters
                .get(self.active_cluster)
                .and_then(|name| session.switch_to_context(name.as_ref()))
        }) {
            self.apply_session(next, cx);
            return;
        }
        self.retry_catalog(cx);
        if let Some(view) = self.active_resource_view() {
            view.update(cx, |view, cx| view.refresh(cx));
        }
        cx.notify();
    }

    /// Switch clusters and rebuild all session-bound views.
    fn switch_cluster(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if index >= self.clusters.len() {
            return false;
        }
        if index == self.active_cluster {
            return true;
        }
        if !self.ensure_clean_for_session_change(cx) {
            return false;
        }
        let Some(session) = self.session.clone() else {
            self.active_cluster = index;
            self.session_epoch = self.session_epoch.wrapping_add(1);
            self.reset_catalog_retry();
            self.namespace = SharedString::from(ALL_NAMESPACES);
            self.namespace_state = NamespaceState::Ready(Vec::new());
            self._namespace_task = None;
            self.reset_cluster_bound_ui(cx);
            self.rebind_helm_views(cx);
            let identity = InspectorSession {
                id: self.session_epoch,
                cluster_id: None,
            };
            self.inspector.update(cx, |panel, cx| {
                panel.set_session_identity(identity);
                panel.set_selection(None, cx);
            });
            self.ensure_tab_view(self.active_tab, cx);
            self.sync_active_inspector_selection(cx);
            self.search
                .update(cx, |search, cx| search.set_executor(None, cx));
            self.rebuild_commands();
            self.defer_focus_active_view(cx);
            cx.notify();
            return true;
        };
        let Some(name) = self.clusters.get(index).cloned() else {
            return false;
        };
        let Some(next) = session.switch_to_context(&name) else {
            self.notify(
                "The app did not switch contexts. The current session is unchanged. Select an available context, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some(format!("Context {name} is not available.")),
                cx,
            );
            return false;
        };
        self.active_cluster = index;
        self.apply_session(next, cx)
    }

    fn invalidate_cluster_metadata(&mut self) {
        for tab in &mut self.tabs {
            match tab.content {
                TabContent::Resource => {
                    tab.entry = None;
                    tab.resource = None;
                }
                TabContent::Preview => {
                    tab.identity = None;
                    tab.resource = None;
                    tab.preview = Some(PreviewState::FollowSelection);
                    tab.title = preview_title(tab.kind.as_ref(), None, false);
                }
                _ => {}
            }
        }
    }

    fn is_stable_shell_focus(&self, handle: &FocusHandle, cx: &App) -> bool {
        *handle == self.focus_handle
            || *handle == self.tree_focus_handle
            || *handle == self.hotbar_focus_handle
            || *handle == self.hotbar_bank_focus
            || *handle == self.hotbar_add_focus
            || *handle == self.hotbar_hide_focus
            || *handle == self.center_tabs_focus
            || *handle == self.top_bar_focus
            || *handle == self.top_bar_cluster_focus
            || *handle == self.top_bar_namespace_focus
            || *handle == self.top_bar_palette_focus
            || *handle == self.top_bar_settings_focus
            || *handle == self.top_bar_inspector_focus
            || *handle == self.top_bar_notifications_focus
            || *handle == self.status_bar_port_forward_focus
            || *handle == self.tree_filter_input.read(cx).focus_handle(cx)
    }

    fn restore_focus_after_reset(&self, focus: FocusHandle, cx: &mut Context<Self>) {
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let Some(window) = window else {
                return;
            };
            window
                .update(cx, |_, window, cx| window.focus(&focus, cx))
                .ok();
        });
    }

    fn reset_cluster_bound_ui(&mut self, cx: &mut Context<Self>) {
        let previous_focus = self
            .dialog_previous_focus
            .take()
            .or_else(|| self.search_previous_focus.take())
            .or_else(|| self.palette_previous_focus.take())
            .or_else(|| self.notification_previous_focus.take())
            .or_else(|| self.sidebar_previous_focus.take())
            .or_else(|| self.inspector_previous_focus.take())
            .or_else(|| self.dock_previous_focus.take());
        let stable_focus = previous_focus.filter(|focus| self.is_stable_shell_focus(focus, cx));
        self.focus_active_view_pending = stable_focus.is_none();
        self.pending_action = None;
        self.invalidate_service_account_request();
        self.latency_tier = LatencyTier::Local;
        self.latency_auto_paused.clear();
        self.dialog = None;
        self.dialog_input_epoch = self.dialog_input_epoch.wrapping_add(1);
        self.dialog_input_value.clear();
        self.dialog_input_caret = 0;
        self.dialog_focus = 0;
        self.dialog_previous_focus = None;
        self.center_tabs_cursor = Some(self.active_tab);
        self.search_open = false;
        self.search_epoch = None;
        self.search_previous_focus = None;
        self.palette_open = false;
        self.palette_scope = PaletteScope::Commands;
        self.palette_query.clear();
        self.palette_selection = None;
        self.palette_note = None;
        self.palette_previous_focus = None;
        self.palette_input
            .update(cx, |input, cx| input.clear_pending(cx));
        self.status_panel = StatusPanel::None;
        self.notification_previous_focus = None;
        self.search.update(cx, |search, cx| {
            search.close(cx);
            search.set_executor(None, cx);
        });
        self.invalidate_cluster_metadata();
        self.resource_selections.clear();
        self.active_resource_tab = self.active_tab_is_resource().then_some(self.active_tab);
        for route in self.routes.values() {
            route.reset();
        }
        self.inspector_route.reset();
        self.preview_hydration_pending = self.following_preview_index().is_some();
        for view in &mut self.views {
            if !matches!(view, Some(TabView::Helm(_))) {
                *view = None;
            }
        }
        // Recreated here, so it starts folded for the same reason it does in
        // `Shell::new`: switching cluster leaves no stream to show, and an
        // unfolded Dock is a panel of "nothing here" between the table and the
        // status bar. `open_dock` unfolds it on every path that means "give me
        // this stream".
        self.dock_panel = cx.new(|cx| {
            let mut dock = DockPanel::new(cx);
            dock.set_body_collapsed(true, cx);
            dock
        });
        self.dock_observation = cx.observe(&self.dock_panel, |_, _, cx| cx.notify());
        install_dock_notice_handler(&self.shell_weak, &self.dock_panel, cx);
        if let Some(focus) = stable_focus {
            self.restore_focus_after_reset(focus, cx);
        }
    }

    fn defer_focus_active_view(&self, cx: &mut Context<Self>) {
        let shell = self.shell_weak.clone();
        let epoch = self.session_epoch;
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let Some(window) = window else {
                if let Some(shell) = shell.upgrade() {
                    shell.update(cx, |shell, _| shell.focus_active_view_pending = false);
                }
                return;
            };
            window
                .update(cx, |_, window, cx| {
                    let Some(shell) = shell.upgrade() else {
                        return;
                    };
                    let focus_shell = shell.clone();
                    window.defer(cx, move |window, cx| {
                        focus_shell.update(cx, |shell, cx| {
                            if shell.session_epoch != epoch || !shell.focus_active_view_pending {
                                return;
                            }
                            if shell.focus_active_view(window, cx) {
                                shell.focus_active_view_pending = false;
                                return;
                            }
                            shell.focus_active_view_pending = false;
                            shell.restore_shell_focus_if_empty(window, cx);
                        });
                    });
                })
                .ok();
        });
    }

    fn invalidate_catalog_task(&mut self) {
        self.catalog_load_generation = self.catalog_load_generation.wrapping_add(1);
        self._catalog_task = None;
    }

    fn reset_catalog_retry(&mut self) {
        self.catalog_retry_attempt = 0;
        self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
        self.catalog_retry_task = None;
        self.catalog_retry_focus_tree = false;
        self.invalidate_catalog_task();
    }

    fn defer_focus_tree(&self, cx: &mut Context<Self>) {
        let shell = self.shell_weak.clone();
        let epoch = self.session_epoch;
        cx.defer(move |cx| {
            let window = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next());
            let Some(window) = window else {
                return;
            };
            window
                .update(cx, |_, window, cx| {
                    let Some(shell) = shell.upgrade() else {
                        return;
                    };
                    if shell.read(cx).session_epoch == epoch {
                        let focus = shell.read(cx).tree_focus_handle.clone();
                        window.focus(&focus, cx);
                    }
                })
                .ok();
        });
    }

    /// Apply a new cluster session.
    fn apply_session(&mut self, session: ClusterSession, cx: &mut Context<Self>) -> bool {
        if !self.ensure_clean_for_session_change(cx) {
            return false;
        }
        let previous_cluster = self.session.as_ref().and_then(ClusterSession::cluster_id);
        if let Some(id) = previous_cluster {
            self.namespace_by_cluster.insert(id, self.namespace.clone());
            self.namespaces_by_cluster
                .insert(id, self.namespace_state.clone());
        }
        self._namespace_task = None;
        self._health_task = None;
        self._health_schedule_task = None;
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.reset_catalog_retry();
        if let Some(registry) = session.registry() {
            self.clusters = cluster_names(registry);
        }
        if let Some(name) = session.cluster_name()
            && let Some(index) = self
                .clusters
                .iter()
                .position(|cluster| cluster.as_ref() == name)
        {
            self.active_cluster = index;
        } else if self.active_cluster > self.clusters.len() {
            self.active_cluster = self.clusters.len();
        }
        self.startup_state = StartupState::from_session(Some(&session));
        self.health_started_epoch = None;
        self.reset_cluster_bound_ui(cx);
        let name = session
            .cluster_name()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("No Context"));
        self.namespace = session
            .cluster_id()
            .and_then(|id| self.namespace_by_cluster.get(&id).cloned())
            .unwrap_or_else(|| SharedString::from(ALL_NAMESPACES));
        self.namespace_state = session
            .cluster_id()
            .filter(|id| Some(*id) != previous_cluster)
            .and_then(|id| self.namespaces_by_cluster.remove(&id))
            .unwrap_or_else(|| match &session {
                ClusterSession::Ready { .. } => NamespaceState::Loading,
                ClusterSession::Unavailable { reason, .. } => {
                    NamespaceState::Failed(reason.clone())
                }
            });
        if let NamespaceState::Ready(names) = &self.namespace_state
            && self.namespace.as_ref() != ALL_NAMESPACES
            && !names
                .iter()
                .any(|name| name.as_ref() == self.namespace.as_ref())
        {
            self.namespace = SharedString::from(ALL_NAMESPACES);
        }

        self.catalog_failure = match &session {
            ClusterSession::Unavailable { reason, .. } => Some(reason.clone()),
            ClusterSession::Ready { .. } => None,
        };
        self.catalog = session.catalog();
        self.cache = session.cluster_cache();
        self.catalog_stale = None;
        self.catalog_state = initial_catalog_state(Some(&session), &self.startup_state);
        self.connection = initial_connection_state(Some(&session), &self.startup_state);
        self.health_probe = match &session {
            ClusterSession::Ready {
                registry, cluster, ..
            } => registry.get(*cluster).map(|cluster| HealthProbe {
                receiver: cluster.health(),
                latency: cluster.latency(),
            }),
            ClusterSession::Unavailable { .. } => None,
        };
        self.cluster_handle = session.cluster_handle();
        self.session = Some(session.clone());
        self.rebind_helm_views(cx);
        self.kubeconfig_warning = session
            .registry()
            .and_then(|registry| kubeconfig_source_warning(registry));
        if matches!(self.namespace_state, NamespaceState::Loading) {
            self.start_namespace_load(cx);
        }
        let (hotbar, hotbar_error) = session
            .registry()
            .map(|registry| ClusterSession::load_hotbar(registry))
            .unwrap_or((Hotbar::default(), None));
        self.hotbar_load_error = hotbar_error.map(|error| error.to_string());
        self.dispatch_hotbar(HotbarEvent::Load(hotbar), cx);
        self.search.update(cx, |search, cx| {
            search.set_executor(SearchExecutor::from_session(&session), cx);
        });

        self.tree = ResourceTree::empty(&name);
        let collapsed = self.collapsed_for_new_catalog(&name);
        self.collapsed = collapsed;
        self.tree_cursor = Some(0);
        self.selected_node = None;

        let inspector = self.inspector.clone();
        let pods_route = self.resource_route(0);
        let pods_identity = InspectorSession {
            id: self.session_epoch,
            cluster_id: session.cluster_id(),
        };
        let factory = session.pods_factory(self.namespace_scope());
        let ops = self.object_ops();
        self.pods = cx.new(|cx| {
            let mut view = PodsView::new_with_inspector(
                factory,
                None,
                routed_inspector_binding(inspector, pods_route, pods_identity),
                cx,
            );
            view.set_ops(ops);
            view
        });
        self.install_resource_view(&self.pods, &ResourceSpec::pods(), 0, cx);
        self.ensure_tab_view(self.active_tab, cx);

        let source = session.inspector_source();
        let metrics = metrics_handle_for(&session);
        let identity = InspectorSession {
            id: self.session_epoch,
            cluster_id: session.cluster_id(),
        };
        self.inspector.update(cx, |panel, cx| {
            panel.set_source(source);
            panel.set_session_identity(identity);
            panel.set_metrics_source(metrics, MetricsProbeState::Checking, cx);
            panel.set_selection(None, cx);
        });
        self.sync_active_inspector_selection(cx);

        if let Some(services) = self.terminal_services.as_mut() {
            services.context = session.cluster_name().map(str::to_owned);
        }
        self.sync_terminal_services(cx);
        let factory = session.log_factory();
        self.dock_panel.update(cx, |panel, cx| {
            panel.set_log_factory(factory, cx);
        });

        self.start_health_probe(cx);
        self.start_catalog_load(cx);
        self.refresh_capability_probes(cx);
        self.rebuild_commands();
        self.defer_focus_active_view(cx);
        eprintln!("k8s-gpui: switched cluster to {name}");
        cx.notify();
        true
    }

    fn start_namespace_load(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            self.namespace_state = NamespaceState::Ready(Vec::new());
            return;
        };
        let (Some(registry), Some(cluster), Some(handle)) = (
            session.registry().cloned(),
            session.cluster_id(),
            session.tokio_handle().cloned(),
        ) else {
            self.namespace_state = NamespaceState::Failed(
                "Context connection is unavailable. Select a context, then refresh the namespace list."
                    .to_owned(),
            );
            return;
        };
        let source = data_source_for_cluster(&registry, cluster);
        self.namespace_state = NamespaceState::Loading;
        let epoch = self.session_epoch;
        #[cfg(test)]
        let injected = self.namespace_future.as_ref().map(|future| future(cluster));
        self._namespace_task = Some(cx.spawn(async move |this, cx| {
            #[cfg(test)]
            let result = match injected {
                Some(future) => future.await,
                None => match source {
                    Some(source) => handle
                        .spawn(async move { load_cluster_namespaces(source).await })
                        .await
                        .unwrap_or_else(|error| {
                            Err(format!("The namespace list task failed: {error}"))
                        }),
                    None => Err(
                        "That context is no longer available. Reload kubeconfigs and try again."
                            .to_owned(),
                    ),
                },
            };
            #[cfg(not(test))]
            let result = match source {
                Some(source) => handle
                    .spawn(async move { load_cluster_namespaces(source).await })
                    .await
                    .unwrap_or_else(|error| {
                        Err(format!("The namespace list task failed: {error}"))
                    }),
                None => Err(
                    "That context is no longer available. Reload kubeconfigs and try again."
                        .to_owned(),
                ),
            };
            this.update(cx, |shell, cx| {
                shell.on_namespaces_loaded(epoch, result, cx)
            })
            .ok();
        }));
    }

    fn on_namespaces_loaded(
        &mut self,
        epoch: u64,
        result: Result<Vec<String>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_epoch != epoch {
            return;
        }
        let state = match result {
            Ok(mut names) => {
                names.sort();
                names.dedup();
                NamespaceState::Ready(names.into_iter().map(SharedString::from).collect())
            }
            Err(reason) => NamespaceState::Failed(reason),
        };
        let mut reset_namespace = false;
        if let NamespaceState::Ready(names) = &state {
            reset_namespace = self.namespace.as_ref() != ALL_NAMESPACES
                && !names
                    .iter()
                    .any(|name| name.as_ref() == self.namespace.as_ref());
        }
        if reset_namespace && self.active_tab_dirty(cx) {
            self.toast(
                "Apply or revert the unsaved YAML before the namespace refresh changes the scope."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            reset_namespace = false;
        }
        self.namespace_state = state.clone();
        if reset_namespace {
            self.namespace = SharedString::from(ALL_NAMESPACES);
            self.sync_terminal_services(cx);
        }
        if let Some(id) = self.session.as_ref().and_then(ClusterSession::cluster_id) {
            self.namespace_by_cluster.insert(id, self.namespace.clone());
            self.namespaces_by_cluster.insert(id, state);
        }
        if reset_namespace {
            self.clear_resource_selections(cx);
            self.rebuild_resource_views(cx);
            self.focus_active_view_pending = true;
            self.defer_focus_active_view(cx);
        }
        self.rebuild_commands();
        cx.notify();
    }

    /// Set the namespace scope and rebuild resource data sources.
    fn set_namespace(&mut self, next: SharedString, cx: &mut Context<Self>) {
        let available = next.as_ref() == ALL_NAMESPACES
            || matches!(&self.namespace_state, NamespaceState::Ready(names) if names.contains(&next));
        if !available || next == self.namespace {
            return;
        }
        if self.active_tab_dirty(cx) {
            self.toast(
                "Apply or revert the unsaved YAML before changing namespace.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        self.namespace = next;
        self.sync_terminal_services(cx);
        self.layout_changed();
        if let Some(id) = self.session.as_ref().and_then(ClusterSession::cluster_id) {
            self.namespace_by_cluster.insert(id, self.namespace.clone());
        }
        self.clear_resource_selections(cx);
        self.rebuild_resource_views(cx);
        self.rebuild_commands();
        self.focus_active_view_pending = true;
        self.defer_focus_active_view(cx);
        cx.notify();
    }

    fn namespace_scope(&self) -> Option<&str> {
        (self.namespace.as_ref() != ALL_NAMESPACES).then(|| self.namespace.as_ref())
    }

    /// Rebuild resource views for the current namespace scope.
    fn rebuild_resource_views(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if !matches!(session, ClusterSession::Ready { .. }) {
            return;
        }
        let scope = self.namespace_scope().map(str::to_owned);
        let inspector = self.inspector.clone();
        let pods_route = self.resource_route(0);
        let pods_identity = InspectorSession {
            id: self.session_epoch,
            cluster_id: session.cluster_id(),
        };

        let pods_factory = session.pods_factory(scope.as_deref());
        let ops = self.object_ops();
        self.pods = cx.new(|cx| {
            let mut view = PodsView::new_with_inspector(
                pods_factory,
                None,
                routed_inspector_binding(inspector.clone(), pods_route, pods_identity),
                cx,
            );
            view.set_ops(ops);
            view
        });
        self.install_resource_view(&self.pods, &ResourceSpec::pods(), 0, cx);

        let rebuilds: Vec<(usize, ResourceSpec, SourceFactory)> = self
            .tabs
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(index, tab)| {
                self.views.get(index)?.as_ref()?;
                let entry = tab.entry.as_ref()?;
                let spec =
                    ResourceSpec::new(entry.kind.clone(), tab.title.clone(), entry.namespaced())
                        .with_resource(entry.to_api_resource());
                let factory = session.source_factory(entry, scope.as_deref());
                Some((index, spec, factory))
            })
            .collect();
        for (index, spec, factory) in rebuilds {
            let ops = self.object_ops();
            let view_spec = spec.clone();
            let route = self.resource_route(index);
            let identity = self.inspector_session();
            let view = cx.new(|cx| {
                let mut view = PodsView::for_resource(
                    factory,
                    None,
                    routed_inspector_binding(inspector.clone(), route, identity),
                    view_spec,
                    cx,
                );
                view.set_ops(ops);
                view
            });
            self.install_resource_view(&view, &spec, index, cx);
            self.views[index] = Some(TabView::Resource(view));
        }
    }

    // Keymap actions

    fn check_for_updates(
        &mut self,
        _: &CheckForUpdates,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_update_check(cx);
    }

    fn restart_to_update(
        &mut self,
        _: &RestartToUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_update_restart(cx);
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.search_open {
            return true;
        }
        let focused = window.focused(cx);
        let previous_focus = if self.palette_open {
            self.palette_previous_focus.take().or_else(|| {
                let palette_input = self.palette_input.read(cx).focus_handle(cx);
                focused.filter(|handle| {
                    *handle != palette_input && *handle != self.palette_focus_handle
                })
            })
        } else {
            focused.or_else(|| {
                self.active_resource_view()
                    .map(|view| view.read(cx).table_focus_handle(cx))
            })
        };
        self.close_status_panel(window, cx);
        if self.palette_open {
            self.palette_open = false;
            self.palette_scope = PaletteScope::Commands;
            self.palette_query.clear();
            self.palette_input
                .update(cx, |input, cx| input.clear(window, cx));
            self.palette_selection = None;
            self.palette_note = None;
        }
        self.search_previous_focus = previous_focus;
        self.search_open = true;
        self.search_epoch = Some(self.session_epoch);
        let opened = self
            .search
            .update(cx, |search, cx| search.open_cluster(window, cx));
        if !opened {
            self.search_open = false;
            self.search_epoch = None;
            self.search_previous_focus = None;
            return false;
        }
        let focus = self.search.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
        true
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_open {
            return;
        }
        self.search_open = false;
        self.search_epoch = None;
        self.search.update(cx, |search, cx| search.close(cx));
        if let Some(focus) = self.search_previous_focus.take() {
            window.focus(&focus, cx);
        } else if let Some(view) = self.active_resource_view() {
            let focus = view.read(cx).table_focus_handle(cx);
            window.focus(&focus, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn command_search_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search(window, cx);
    }

    fn search_resources(
        &mut self,
        _: &SearchResources,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_search_resources(window, cx);
    }

    fn open_search_result(
        &mut self,
        hit: SearchHit,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_epoch != epoch || self.search_epoch != Some(epoch) {
            return;
        }
        let spec = ResourceSpec::new(
            hit.resource.kind.clone(),
            hit.resource.plural.clone(),
            hit.object.metadata.namespace.is_some(),
        )
        .with_resource(hit.resource.to_api_resource());
        self.close_search(window, cx);
        let row = Row {
            obj: hit.object,
            cells: Vec::new(),
        };
        match PreviewIdentity::from_row(&spec, &row) {
            Some(identity) => self.pin_preview(&row, &spec, identity, None, cx),
            None => self.open_preview(&row, &spec, None, cx),
        }
        self.focus_active_view_and_clear_pending(window, cx);
        cx.notify();
    }

    /// Runs a search result action, so exec and port-forward are one keystroke from a result.
    ///
    /// A result is a whole object, so both actions build their own target from the hit. Opening
    /// the result first would not help: the tab it opens is an Inspector, and an Inspector holds
    /// no row selection for a Pods table to act on.
    fn run_search_result_action(
        &mut self,
        hit: SearchHit,
        epoch: u64,
        action: SearchResultAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_epoch != epoch || self.search_epoch != Some(epoch) {
            return;
        }
        match action {
            SearchResultAction::Open => self.open_search_result(hit, epoch, window, cx),
            SearchResultAction::Exec => {
                let Some(target) = search_hit_exec_target(&hit) else {
                    self.toast(
                        "Open Shell is available only for Pods.".to_owned(),
                        design::Severity::Warning,
                        cx,
                    );
                    return;
                };
                self.close_search(window, cx);
                self.open_exec_dialog(target, window, cx);
            }
            SearchResultAction::PortForward => {
                let Some(target) = search_hit_forward_target(&hit) else {
                    self.toast(
                        "Start Port Forward is available only for Pods.".to_owned(),
                        design::Severity::Warning,
                        cx,
                    );
                    return;
                };
                self.close_search(window, cx);
                self.open_port_forward_dialog(target, window, cx);
            }
        }
    }

    fn open_palette_with_scope(
        &mut self,
        scope: PaletteScope,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog.is_some() {
            self.cancel_dialog(window, cx);
        }
        if self.search_open {
            self.close_search(window, cx);
        }
        // A modal must not sit under a context menu: the menu is drawn in a later
        // layer, so it would cover the palette.
        self.close_tab_context_menu(window, cx);
        self.close_tree_context_menu(window, cx);
        self.close_status_panel(window, cx);
        if !self.palette_open {
            self.palette_previous_focus = window
                .focused(cx)
                .filter(|handle| *handle != self.palette_focus_handle);
        }
        self.palette_open = true;
        self.palette_scope = scope;
        self.palette_query = query.to_owned();
        self.palette_input
            .update(cx, |input, cx| input.set_text(query, window, cx));
        self.palette_selection = None;
        self.palette_note = None;
        self.sync_palette_selection();
        let input_focus = self.palette_input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        self.reveal_palette_selection();
        cx.notify();
    }

    fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_open {
            self.close_palette(window, cx);
        } else {
            self.open_palette_with_scope(
                PaletteScope::Commands,
                PaletteScope::Commands.query(),
                window,
                cx,
            );
        }
    }

    fn open_context_switcher(
        &mut self,
        _: &OpenContextSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebuild_commands();
        self.open_palette_with_scope(
            PaletteScope::Context,
            PaletteScope::Context.query(),
            window,
            cx,
        );
    }

    fn open_namespace_switcher(
        &mut self,
        _: &OpenNamespaceSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebuild_commands();
        self.open_palette_with_scope(
            PaletteScope::Namespace,
            PaletteScope::Namespace.query(),
            window,
            cx,
        );
    }

    fn open_resource_kind_switcher(
        &mut self,
        _: &OpenResourceKindSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebuild_commands();
        self.open_palette_with_scope(PaletteScope::Kind, PaletteScope::Kind.query(), window, cx);
    }

    fn toggle_left_panel(
        &mut self,
        _: &ToggleLeftPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar_open {
            self.sidebar_open = false;
            let fallback = self.focus_handle.clone();
            Self::restore_panel_focus(window, cx, &mut self.sidebar_previous_focus, &fallback);
        } else {
            Self::remember_panel_focus(window, cx, &mut self.sidebar_previous_focus);

            self.sidebar_open = true;
        }
        self.constrain_layout(self.viewport_width, self.viewport_height);
        self.layout_changed();
        cx.notify();
    }

    fn toggle_right_panel(
        &mut self,
        _: &ToggleRightPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inspector_open {
            self.inspector_open = false;
            let fallback = self.focus_handle.clone();
            Self::restore_panel_focus(window, cx, &mut self.inspector_previous_focus, &fallback);
        } else {
            self.open_inspector(window, cx);
        }
        self.layout_changed();
        cx.notify();
    }

    fn toggle_dock(&mut self, _: &ToggleDock, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock_open {
            self.dock_open = false;
            let fallback = self.focus_handle.clone();
            Self::restore_panel_focus(window, cx, &mut self.dock_previous_focus, &fallback);
        } else {
            self.open_dock(window, cx);
        }
        self.constrain_layout(self.viewport_width, self.viewport_height);
        self.layout_changed();
        cx.notify();
    }

    /// Toggle the status popover above the status bar.
    fn toggle_notifications(
        &mut self,
        _: &ToggleNotifications,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.status_panel == StatusPanel::None {
            self.open_notifications(window, cx);
        } else {
            self.close_status_panel(window, cx);
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.tab_action_target(window, cx);
        self.close_tab_index(target, window, cx);
    }

    fn close_other_tabs(
        &mut self,
        _: &CloseOtherTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.tab_action_target(window, cx);
        self.close_other_center_tabs(target, window, cx);
    }

    fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_center_tabs(window, cx);
    }

    fn toggle_pin_tab(&mut self, _: &TogglePinTab, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.center_tab_cursor();
        if self.open_tabs.contains(&index) {
            self.toggle_center_tab_pin(index, cx);
            window.focus(&self.center_tabs_focus, cx);
        }
    }

    fn move_tab_left(&mut self, _: &MoveTabLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_center_tab(-1, window, cx);
    }

    fn move_tab_right(&mut self, _: &MoveTabRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_center_tab(1, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }

    fn switch_tab(&mut self, action: &SwitchTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.activate_tab(action.index, cx) {
            self.focus_active_view_and_clear_pending(window, cx);
            cx.notify();
        }
    }

    /// One rung of the Escape ladder, and the ladder never runs out.
    ///
    /// `UI-SPEC` §9.3 asks for three things and they are all here: close the popover, cancel
    /// the operation, step back one layer — and the last clause is the one that is easy to
    /// miss, which is that **there is no state in which this does nothing**. A dead Escape
    /// does not read as "there is nothing to dismiss", it reads as a broken app, and a reader
    /// who hits it once stops trusting the key everywhere else in the product.
    ///
    /// The order is bottom-up: the topmost surface first, then the query inside it, then the
    /// layer under that. The rung below the query is why a palette with three characters in it
    /// takes two Escapes and not one — a reader who typed a query and pressed Escape means
    /// "take this back", and the query is the part they would have to retype, while the
    /// palette itself costs them nothing. That is also what Spotlight does.
    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        // A context menu is the topmost surface, so it closes first.
        if self.tree_context_menu.is_some() {
            self.close_tree_context_menu(window, cx);
            return;
        }
        if self.tab_context_menu.is_some() {
            self.close_tab_context_menu(window, cx);
            return;
        }
        if self.dialog.is_some() {
            self.cancel_dialog(window, cx);
            return;
        }
        if self.palette_open {
            self.dismiss_palette(window, cx);
            return;
        }
        if self.search_open {
            self.close_search(window, cx);
            return;
        }
        if self.status_panel != StatusPanel::None {
            self.close_status_panel(window, cx);
            return;
        }
        // The floating Inspector is an overlay, so it is dismissed before anything that lives
        // underneath it — and it is the only panel that can be in this state at all, because
        // a docked one has no layer to step back from.
        if self.panel_layout(self.viewport_width).1 == InspectorLayout::Floating {
            self.inspector_open = false;
            self.layout_changed();
            let fallback = self.focus_handle.clone();
            Self::restore_panel_focus(window, cx, &mut self.inspector_previous_focus, &fallback);
            cx.notify();
            return;
        }
        if self.update_strip_expanded {
            self.close_update_overlay(window, cx);
            return;
        }
        if self.tree_filter_active() {
            // A filter is a narrowing the reader applied, so Escape is how they take it back
            // without reaching for the field and selecting eight characters. The Dock's own
            // filter is the same rule and the Dock owns its own Escape.
            self.clear_tree_filter(window, cx);
            return;
        }
        if self.toast.is_some() {
            self.toast = None;
            cx.notify();
            return;
        }
        if self.selected_node.take().is_some() {
            cx.notify();
            return;
        }
        // The bottom of the ladder. Nothing is open, nothing is selected, and Escape still
        // has to mean something, so it puts the keyboard on the resource tree: a visible
        // focus ring on the one list that is always on screen. Closing the active tab
        // instead would be a louder answer, and it would be the wrong one — a reader who
        // pressed Escape while looking at a table did not ask to lose their view.
        if self.tree_focus_handle.contains_focused(window, cx) {
            // Already on the tree, so there is nowhere further back to go and the keyboard
            // returns to whatever the reader was looking at.
            if let Some(focus) = self.active_view_focus(cx) {
                window.focus(&focus, cx);
            }
        } else {
            window.focus(&self.tree_focus_handle, cx);
        }
        cx.notify();
    }

    /// Clears the palette's query, or closes the palette once it is empty.
    fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_query.is_empty() {
            self.close_palette(window, cx);
            return;
        }
        self.palette_query.clear();
        self.palette_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.palette_selection = None;
        self.palette_note = None;
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_scope = PaletteScope::Commands;
        self.palette_query.clear();
        self.palette_input
            .update(cx, |input, cx| input.clear(window, cx));
        self.palette_selection = None;
        self.palette_note = None;
        match self.palette_previous_focus.take() {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus_handle, cx),
        }
        cx.notify();
    }

    // Dialog actions

    /// Open the delete confirmation for the selected object.
    fn open_delete_dialog(
        &mut self,
        target: DeleteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let object = &target.object;
        let title: SharedString = format!("Delete {}?", object.name).into();
        let cluster_suffix = self
            .session
            .as_ref()
            .and_then(ClusterSession::cluster_name)
            .map(|name| format!(" · Context {name}"))
            .unwrap_or_default();
        let detail: SharedString = match &object.namespace {
            Some(namespace) => {
                format!(
                    "{} in namespace {namespace}{cluster_suffix}. This action is permanent.",
                    object.resource.kind
                )
            }
            None => format!(
                "{}{cluster_suffix}. This action is permanent.",
                object.resource.kind
            ),
        }
        .into();
        let Some(view) = self.active_resource_view() else {
            return;
        };
        self.dialog = Some(Dialog::ConfirmDelete {
            objects: vec![target.object.clone()],
            target,
            title,
            detail,
            count: 1,
            view,
        });
        self.open_dialog_focus(window, cx);
    }
    fn dialog_input(&self) -> Option<Entity<TextInput>> {
        match &self.dialog {
            Some(
                Dialog::Scale { input, .. }
                | Dialog::PortForward { input, .. }
                | Dialog::HotbarBankName { input, .. }
                | Dialog::HelmConfirm {
                    input: Some(input), ..
                },
            ) => Some(input.clone()),
            _ => None,
        }
    }

    /// The text field the current dialog focus belongs to, if any.
    ///
    /// The port-forward dialog has two fields, so the focused index decides which one answers.
    fn dialog_field(&self) -> Option<Entity<TextInput>> {
        if self.dialog_focus == PORT_FORWARD_LOCAL_FOCUS
            && let Some(Dialog::PortForward { local, .. }) = self.dialog.as_ref()
        {
            return Some(local.clone());
        }
        self.dialog_input()
    }

    /// True when the focused dialog control is a text field, so typing belongs to it.
    fn dialog_focus_is_field(&self) -> bool {
        if self.dialog_focus == PORT_FORWARD_LOCAL_FOCUS {
            return matches!(self.dialog, Some(Dialog::PortForward { .. }));
        }
        self.dialog_focus == 0 && self.dialog_input().is_some()
    }

    fn new_dialog_input(
        &self,
        kind: DialogInputKind,
        initial: String,
        epoch: u64,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let (placeholder, label, description, clear_label, max_length) = kind.spec();
        let numeric = matches!(
            kind,
            DialogInputKind::Scale
                | DialogInputKind::PortForward
                | DialogInputKind::PortForwardLocal
        );
        let shell = self.shell_weak.clone();
        cx.new(|cx| {
            let input = TextInput::new(placeholder, cx, move |text, cx| {
                let shell = shell.clone();
                let text = text.to_owned();
                cx.defer(move |cx| {
                    shell
                        .update(cx, |shell, cx| {
                            shell.on_dialog_input_changed(epoch, &text, cx)
                        })
                        .ok();
                });
            })
            .with_text(initial)
            .without_leading_icon()
            .without_escape_hint()
            .with_accessibility(label, description, clear_label)
            .with_width(px((width
                - 2.0 * f32::from(design::space::LG)
                - 2.0 * f32::from(design::border::LINE))
            .max(0.0)));
            if numeric {
                input
                    .with_role(Role::NumberInput)
                    .with_numeric_input(max_length)
            } else {
                input.with_role(Role::TextInput).with_max_length(max_length)
            }
        })
    }

    fn on_dialog_input_changed(&mut self, epoch: u64, text: &str, cx: &mut Context<Self>) {
        if epoch != self.dialog_input_epoch {
            return;
        }
        // The port-forward dialog reports from two fields, so the changed one owns the invalid
        // state and the other one keeps whatever the user typed there.
        if let Some(Dialog::PortForward { input, local, .. }) = self.dialog.as_ref()
            && input.read(cx).text() != text
        {
            if local.read(cx).text() == text {
                let invalid = parse_local_port(text).is_err();
                let local = local.clone();
                local.update(cx, |input, cx| input.set_invalid(invalid, cx));
            }
            return;
        }
        let Some(input) = self.dialog_input() else {
            return;
        };
        if input.read(cx).text() != text {
            return;
        }
        let previous = self.dialog_input_value.clone();
        let previous_len = previous.chars().count();
        let mut text = text.to_owned();
        let text_len = text.chars().count();
        if matches!(&self.dialog, Some(Dialog::Scale { .. }))
            && self.dialog_input_caret == 0
            && !text.is_empty()
            && text != previous
            && text_len <= previous_len
        {
            text = format!("{text}{previous}");
            input.update(cx, |input, cx| input.set_text_pending(text.clone(), cx));
            self.dialog_input_caret = text_len;
        } else if text_len > previous_len {
            self.dialog_input_caret += text_len - previous_len;
        } else if text_len < previous_len {
            self.dialog_input_caret = self
                .dialog_input_caret
                .saturating_sub(previous_len - text_len);
        }
        self.dialog_input_value = text.clone();
        let invalid = match &self.dialog {
            Some(Dialog::Scale { .. }) => parse_replicas(&text).err().map(str::to_owned),
            Some(Dialog::PortForward { .. }) => parse_port(&text).err().map(str::to_owned),
            Some(Dialog::HotbarBankName { .. }) => text
                .trim()
                .is_empty()
                .then(|| "Enter a bank name.".to_owned()),
            Some(Dialog::HelmConfirm { input: Some(_), .. }) => {
                parse_chart_reference(&text).err().map(str::to_owned)
            }
            _ => return,
        };
        input.update(cx, |input, cx| input.set_invalid(invalid.is_some(), cx));
        match &mut self.dialog {
            Some(Dialog::Scale { error, .. }) => *error = parse_replicas(&text).err(),
            Some(Dialog::PortForward { error, .. })
            | Some(Dialog::HotbarBankName { error, .. }) => *error = invalid,
            Some(Dialog::HelmConfirm {
                action: HelmAction::Upgrade { chart, .. },
                error,
                ..
            }) => {
                *error = invalid;
                match parse_chart_reference(&text) {
                    Ok(reference) => *chart = reference,
                    Err(_) => chart.clear(),
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn focus_dialog_control(&self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.dialog_input();
        if let Some(input) = input
            && self.dialog_focus == 0
        {
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        // The port-forward dialog spends index 1 on its second field, not on Cancel.
        if self.dialog_focus == PORT_FORWARD_LOCAL_FOCUS
            && let Some(Dialog::PortForward { local, .. }) = self.dialog.as_ref()
        {
            let focus = local.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        let focus = match self.dialog {
            Some(Dialog::Exec { .. }) if self.dialog_focus == 0 => self.dialog_focus_handle.clone(),
            _ => self.dialog_button_focus(self.dialog_focus),
        };
        window.focus(&focus, cx);
    }

    fn set_dialog_input_caret(
        &mut self,
        input: &Entity<TextInput>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = index.min(input.read(cx).text().chars().count());
        self.dialog_input_caret = target;
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        // The caret is moved on the field's own state, which only exists once the
        // field has rendered, so the move is deferred past this update. A frame
        // callback would be a frame too late for the keystroke that follows the
        // click: nothing else asks for one.
        let input = input.clone();
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| {
                if let Some(state) = input.state().cloned() {
                    state.update(cx, |state, cx| state.set_selected_range(0..0, cx));
                }
                for _ in 0..target {
                    let right = Keystroke {
                        modifiers: Default::default(),
                        key: "right".into(),
                        key_char: None,
                    };
                    input.handle_keystroke(&right, window, cx);
                }
            });
        });
    }

    /// Open the scale input with the current replica count.
    fn open_scale_dialog(
        &mut self,
        target: ScaleTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active_resource_view() else {
            return;
        };
        let epoch = self.dialog_input_epoch.wrapping_add(1);
        self.dialog_input_epoch = epoch;
        let initial = target.replicas.to_string();
        let input = self.new_dialog_input(
            DialogInputKind::Scale,
            initial.clone(),
            epoch,
            dialog_width(f32::from(window.viewport_size().width)),
            cx,
        );
        self.dialog_input_value = initial.clone();
        self.dialog_input_caret = initial.chars().count();
        self.dialog = Some(Dialog::Scale {
            target,
            view,
            input,
            error: None,
        });
        self.open_dialog_focus(window, cx);
    }

    // Terminal and port-forward actions

    fn sync_terminal_services(&mut self, cx: &mut Context<Self>) {
        let namespace = self.namespace_scope().map(str::to_owned);
        if let Some(services) = self.terminal_services.as_mut() {
            services.namespace = namespace;
        }
        let services = self.terminal_services.clone();
        self.dock_panel
            .update(cx, |dock, cx| dock.set_terminal_services(services, cx));
    }

    /// Set the terminal services used by the Dock.
    pub fn set_terminal_services(
        &mut self,
        services: Option<TerminalServices>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_services = services;
        self.sync_terminal_services(cx);
    }

    pub fn set_update_state(&mut self, state: UpdateUiState, cx: &mut Context<Self>) {
        let state = if self.update_actions.is_none() {
            UpdateUiState::new(UpdatePhase::Unsupported).with_error(UPDATER_UNAVAILABLE_REASON)
        } else {
            state.normalized()
        };
        self.update_strip_expanded = state.phase == UpdatePhase::Unsupported;
        // The notice is a one-shot claim, so it is spent here rather than in render: render runs
        // again for every frame, and a claim spent there would drop the card on the second one.
        // A build with no update actions cannot act on the notice, so it never opens one.
        self.update_notice_opened =
            self.update_strip_expanded && self.update_notice.claim(self.update_actions.is_some());
        self.update_overlay_action = 0;
        self.update_state = state;
        self.sync_update_state(cx);
        cx.notify();
    }

    pub fn set_update_actions(&mut self, actions: UpdateActions, cx: &mut Context<Self>) {
        self.update_actions = Some(actions.clone());
        if self.update_state.error.as_deref() == Some(UPDATER_UNAVAILABLE_REASON) {
            self.update_state = UpdateUiState::default();
            self.update_strip_expanded = false;
            self.update_notice_opened = false;
            self.sync_update_state(cx);
        }
        self.rebuild_commands();
        for slot in &self.views {
            if let Some(TabView::Settings(view)) = slot {
                let actions = actions.clone();
                view.update(cx, |view, cx| view.set_update_actions(Some(actions), cx));
            }
        }
        cx.notify();
    }

    pub fn update_state(&self) -> &UpdateUiState {
        &self.update_state
    }

    /// Collapse the update overlay and hand focus back to the shell.
    fn close_update_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.update_strip_expanded {
            return;
        }
        self.update_strip_expanded = false;
        self.update_notice_opened = false;
        if self.update_overlay_focus.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn sync_update_state(&self, cx: &mut Context<Self>) {
        let state = self.update_state.clone();
        for slot in &self.views {
            if let Some(TabView::Settings(view)) = slot {
                view.update(cx, |view, cx| view.set_update_state(state.clone(), cx));
            }
        }
    }

    fn invoke_update_action(&mut self, action: UpdateActionKind, cx: &mut Context<Self>) {
        let Some(actions) = self.update_actions.as_ref() else {
            self.notify(
                UPDATER_UNAVAILABLE_REASON.to_owned(),
                design::Severity::Warning,
                None,
                cx,
            );
            return;
        };
        let callback = match action {
            UpdateActionKind::Check => actions.check.clone(),
            UpdateActionKind::Retry => actions.retry.clone(),
            UpdateActionKind::Restart => actions.restart.clone(),
        };
        cx.defer(move |cx| callback(cx));
    }

    fn run_update_check(&mut self, cx: &mut Context<Self>) {
        self.invoke_update_action(UpdateActionKind::Check, cx);
    }

    fn run_update_retry(&mut self, cx: &mut Context<Self>) {
        self.invoke_update_action(UpdateActionKind::Retry, cx);
    }

    fn run_update_restart(&mut self, cx: &mut Context<Self>) {
        self.invoke_update_action(UpdateActionKind::Restart, cx);
    }

    // Hotbar actions

    fn hotbar(&self) -> Option<&Hotbar> {
        self.hotbar_machine.state().hotbar()
    }

    /// Dispatch a Hotbar event and process its effects.
    fn dispatch_hotbar(&mut self, event: HotbarEvent, cx: &mut Context<Self>) {
        self.hotbar_machine.handle(&event);
        while let Ok(effect) = self.hotbar_effects.try_recv() {
            self.run_hotbar_effect(effect, cx);
        }
    }

    fn run_hotbar_effect(&mut self, effect: HotbarEffect, cx: &mut Context<Self>) {
        match effect {
            HotbarEffect::Persist => {
                let saved = self.hotbar().map(Hotbar::save_default).unwrap_or(Ok(()));
                if let Err(error) = saved {
                    self.notify(
                        "The app did not save the Hotbar. Check the file permissions, then try again."
                            .to_owned(),
                        design::Severity::Error,
                        Some(error.to_string()),
                        cx,
                    );
                    return;
                }
            }
            HotbarEffect::Notify {
                reason: Some(reason),
            } => {
                self.toast(reason, design::Severity::Warning, cx);
            }
            HotbarEffect::Notify { reason: None } => {}
        }
        cx.notify();
    }

    fn toggle_hotbar(&mut self, _: &ToggleHotbar, _window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_active() {
            self.toast(
                "The resource Hotbar is unavailable while Settings is open. Close Settings to use it.".to_owned(),
                design::Severity::Info,
                cx,
            );
            return;
        }
        self.hotbar_open = !self.hotbar_open;
        let width = self.viewport_width;
        let height = self.viewport_height;
        self.constrain_layout(width, height);
        cx.notify();
    }

    /// Switch to the cluster stored in a Hotbar slot.
    fn switch_hotbar_cluster(&mut self, cluster_id: ClusterId, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.session.clone() else {
            return false;
        };
        let Some(registry) = session.registry() else {
            return false;
        };
        let Some(cluster) = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.id() == cluster_id)
        else {
            self.toast(
                "That context is no longer in your kubeconfig. Reload kubeconfigs and try again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return false;
        };
        let Some(index) = self
            .clusters
            .iter()
            .position(|name| name.as_ref() == cluster.name())
        else {
            self.toast(
                "That context is no longer in your kubeconfig. Reload kubeconfigs and try again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return false;
        };
        self.switch_cluster(index, cx)
    }

    /// Switch clusters from a Hotbar slot action.
    fn on_hotbar_switch_cluster(
        &mut self,
        action: &SwitchCluster,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_hotbar_slot(action.slot, cx);
    }

    /// Switch banks from a Hotbar bank action.
    fn on_hotbar_switch_bank(
        &mut self,
        action: &SwitchBank,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_hotbar_bank(action.index, cx);
    }

    fn switch_hotbar_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some((bank_index, cluster_id)) = self.hotbar().and_then(|hotbar| {
            let bank = hotbar.active_bank()?;
            let entry = bank.slots.get(slot)?;
            Some((hotbar.active, entry.cluster_id))
        }) else {
            return;
        };
        self.hotbar_slot_cursor = slot.min(k8s_core::hotbar::MAX_SLOTS_PER_BANK - 1);

        if self.switch_hotbar_cluster(cluster_id, cx) {
            self.dispatch_hotbar(
                HotbarEvent::SwitchSlot {
                    bank: bank_index,
                    slot,
                },
                cx,
            );
            self.run_hotbar_effect(HotbarEffect::Persist, cx);
        }
    }

    fn switch_hotbar_bank(&mut self, index: usize, cx: &mut Context<Self>) {
        self.dispatch_hotbar(HotbarEvent::SwitchBank { index }, cx);
        let count = self
            .hotbar()
            .and_then(Hotbar::active_bank)
            .map_or(0, |bank| bank.slots.len());
        self.hotbar_slot_cursor = self.hotbar_slot_cursor.min(count.saturating_sub(1));
    }

    /// Add the current cluster to the active bank.
    fn add_current_cluster_to_hotbar(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            self.toast(
                "Select a context to use the Hotbar.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let (Some(cluster_id), Some(name)) = (
            session.cluster_id(),
            session.cluster_name().map(str::to_owned),
        ) else {
            self.toast(
                "Select a context to use the Hotbar.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if self.hotbar().is_none() {
            self.dispatch_hotbar(
                HotbarEvent::CreateBank {
                    name: "default".to_owned(),
                },
                cx,
            );
        }
        self.dispatch_hotbar(
            HotbarEvent::AddSlot {
                bank: self.hotbar().map_or(0, |hotbar| hotbar.active),
                slot: k8s_core::hotbar::Slot::new(cluster_id, name),
            },
            cx,
        );
    }

    /// Open the bank name input.
    fn open_hotbar_bank_dialog(
        &mut self,
        index: Option<usize>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.dialog_input_epoch.wrapping_add(1);
        self.dialog_input_epoch = epoch;
        let input = self.new_dialog_input(
            DialogInputKind::BankName,
            initial,
            epoch,
            dialog_width(f32::from(window.viewport_size().width)),
            cx,
        );
        self.dialog = Some(Dialog::HotbarBankName {
            index,
            input,
            error: None,
        });
        self.open_dialog_focus(window, cx);
    }

    /// Create or rename a Hotbar bank.
    fn confirm_hotbar_bank_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::HotbarBankName { index, input, .. }) = self.dialog.as_ref() else {
            return;
        };
        let text = input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            if let Some(Dialog::HotbarBankName { error, .. }) = self.dialog.as_mut() {
                *error = Some("Enter a bank name.".to_owned());
            }
            if let Some(input) = self.dialog_input() {
                input.update(cx, |input, cx| input.set_invalid(true, cx));
            }
            cx.notify();
            return;
        }
        let index = *index;
        self.dialog = None;
        self.close_dialog_focus(window, cx);
        match index {
            None => {
                self.dispatch_hotbar(HotbarEvent::CreateBank { name: text }, cx);
            }
            Some(index) => {
                self.dispatch_hotbar(HotbarEvent::RenameBank { index, name: text }, cx);
            }
        }
    }

    /// Open the bank removal confirmation.
    fn open_hotbar_remove_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hotbar) = self.hotbar() else {
            self.toast(
                "No Hotbar banks exist. Create one from the Command Palette.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let Some(bank) = hotbar.active_bank() else {
            return;
        };
        self.dialog = Some(Dialog::HotbarRemove {
            index: hotbar.active,
            name: bank.name.clone().into(),
        });
        self.open_dialog_focus(window, cx);
    }

    fn confirm_hotbar_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::HotbarRemove { index, .. }) = self.dialog.take() else {
            return;
        };
        self.close_dialog_focus(window, cx);
        self.dispatch_hotbar(HotbarEvent::RemoveBank { index }, cx);
    }

    fn command_hotbar_add_cluster(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.add_current_cluster_to_hotbar(cx);
    }

    fn command_hotbar_create_bank(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.hotbar().map_or(1, |hotbar| hotbar.banks.len() + 1);
        self.open_hotbar_bank_dialog(None, format!("Bank {next}"), window, cx);
    }

    fn command_hotbar_rename_bank(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, name)) = self.hotbar().map(|hotbar| {
            (
                hotbar.active,
                hotbar
                    .active_bank()
                    .map(|bank| bank.name.clone())
                    .unwrap_or_default(),
            )
        }) else {
            self.toast(
                "No Hotbar banks exist. Create one from the Command Palette.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        self.open_hotbar_bank_dialog(Some(index), name, window, cx);
    }

    /// Remove the active Hotbar bank after confirmation.
    fn command_hotbar_remove_bank(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_hotbar_remove_dialog(window, cx);
    }

    /// Open a shell in the selected Pod container.
    fn open_exec_dialog(
        &mut self,
        target: ExecTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_services.is_none() {
            // The sentence names the cause, and the one thing that can fix it is a button. A
            // warning the reader has to translate into a menu is a warning with no action.
            self.toast_with_action(
                "Open Shell needs a context connection. Select a context, then try again."
                    .to_owned(),
                design::Severity::Warning,
                reload_kubeconfigs_action(),
                None,
                cx,
            );
            return;
        }
        if target.containers.len() > 1 {
            self.dialog = Some(Dialog::Exec {
                target,
                selected: 0,
            });
            self.open_dialog_focus(window, cx);
            return;
        }
        let container = target.containers.first().cloned();
        self.start_exec(target, container, window, cx);
    }

    /// Start a KubeExec terminal.
    fn start_exec(
        &mut self,
        target: ExecTarget,
        container: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = TerminalKind::Exec {
            namespace: target.namespace.unwrap_or_else(|| "default".to_owned()),
            pod: target.name.to_string(),
            container: container.map(|container| container.to_string()),
        };
        self.open_dock(window, cx);
        let result = self
            .dock_panel
            .update(cx, |dock, cx| dock.open_terminal(kind, cx));
        match result {
            Ok(()) => {
                let index = self.dock_panel.read(cx).terminal_count().saturating_sub(1);
                self.dock_panel
                    .update(cx, |dock, cx| dock.activate_terminal(index, window, cx));
            }
            Err(reason) => {
                // A session that could not start still took the Dock, so the Dock keeps the
                // keyboard. Otherwise the arrows go on driving the resource table behind a Dock
                // the user just opened for this terminal.
                self.focus_dock(window, cx);
                self.notify(
                    "The app did not open a shell in the container. Check the container, then try again."
                        .to_owned(),
                    design::Severity::Error,
                    Some(reason),
                    cx,
                );
            }
        }
    }

    /// Start a shell in the selected container.
    fn confirm_exec_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Exec { target, selected }) = self.dialog.as_ref() else {
            return;
        };
        let target = target.clone();
        let container = target.containers.get(*selected).cloned();
        self.dialog = None;
        self.close_dialog_focus(window, cx);
        self.start_exec(target, container, window, cx);
    }

    /// Open the port-forward input for the selected Pod.
    fn open_port_forward_dialog(
        &mut self,
        target: PortForwardTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_services.is_none() {
            self.toast_with_action(
                "Port forwarding needs a context connection. Select a context, then try again."
                    .to_owned(),
                design::Severity::Warning,
                reload_kubeconfigs_action(),
                None,
                cx,
            );
            return;
        }
        let epoch = self.dialog_input_epoch.wrapping_add(1);
        self.dialog_input_epoch = epoch;
        let width = dialog_width(f32::from(window.viewport_size().width));
        // The first declared port is the one the Pod most likely wants, so it starts selected and
        // the dialog is one keystroke from starting a forward.
        let selected = target.ports.first().map(|port| port.port);
        let input = self.new_dialog_input(
            DialogInputKind::PortForward,
            selected.map(|port| port.to_string()).unwrap_or_default(),
            epoch,
            width,
            cx,
        );
        let local = self.new_dialog_input(
            DialogInputKind::PortForwardLocal,
            String::new(),
            epoch,
            width,
            cx,
        );
        self.dialog = Some(Dialog::PortForward {
            target,
            input,
            local,
            selected,
            error: None,
        });
        self.open_dialog_focus(window, cx);
    }

    /// Chooses one of the container ports the Pod declares, so the common case needs no typing.
    fn select_remote_port(&mut self, port: u16, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::PortForward {
            input,
            local,
            selected,
            ..
        }) = self.dialog.as_mut()
        else {
            return;
        };
        *selected = Some(port);
        let input = input.clone();
        let local = local.clone();
        // The field's own change handler clears a stale error and refreshes the caret state.
        input.update(cx, |input, cx| input.set_text(port.to_string(), window, cx));
        local.update(cx, |local, cx| local.set_invalid(false, cx));
    }

    /// Validate and start the port-forward request.
    fn confirm_port_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::PortForward {
            target,
            input,
            local,
            ..
        }) = self.dialog.as_ref()
        else {
            return;
        };
        let text = input.read(cx).text().to_owned();
        // Both fields are read before any error path writes to the dialog, so the borrow of the
        // dialog ends before the first `as_mut`.
        let local_text = local.read(cx).text().to_owned();
        let target = target.clone();
        let remote_port = match parse_port(&text) {
            Ok(port) => port,
            Err(error) => {
                if let Some(Dialog::PortForward { error: slot, .. }) = self.dialog.as_mut() {
                    *slot = Some(error.to_owned());
                }
                if let Some(input) = self.dialog_input() {
                    input.update(cx, |input, cx| input.set_invalid(true, cx));
                }
                self.dialog_focus = 0;
                self.focus_dialog_control(window, cx);
                cx.notify();
                return;
            }
        };
        // An empty local field asks for no port, which the forwards layer turns into a free one.
        let local_port = match parse_local_port(&local_text) {
            Ok(port) => port,
            Err(_) => {
                if let Some(Dialog::PortForward { local, .. }) = self.dialog.as_ref() {
                    let local = local.clone();
                    local.update(cx, |input, cx| input.set_invalid(true, cx));
                }
                self.dialog_focus = PORT_FORWARD_LOCAL_FOCUS;
                self.focus_dialog_control(window, cx);
                cx.notify();
                return;
            }
        };
        // `UI-SPEC` §14.2 wants the conflict caught before the submit, not discovered when the
        // forward fails to bind. Every entry point — the button, Enter, the pointer — lands here,
        // so this is the one place the check has to be authoritative.
        if let Some(port) = local_port {
            let ours = crate::panels::forwards::port_holders(
                &self.dock_panel.read(cx).forward_snapshots(),
            );
            if let Some(holder) = crate::panels::forwards::port_collision(
                port,
                crate::panels::forwards::BindAddress::Loopback,
                &ours,
            ) {
                if let Some(Dialog::PortForward { error: slot, .. }) = self.dialog.as_mut() {
                    *slot = Some(holder.sentence(port));
                }
                self.dialog_focus = PORT_FORWARD_LOCAL_FOCUS;
                self.focus_dialog_control(window, cx);
                cx.notify();
                return;
            }
        }
        let Some(services) = self.terminal_services.clone() else {
            return;
        };
        let request = ForwardRequest {
            context: services.context.clone(),
            namespace: target.namespace.clone(),
            name: target.name.clone(),
            remote_port,
            local_port,
        };
        let result = self
            .dock_panel
            .update(cx, |dock, cx| dock.start_forward(request, cx));
        match result {
            Ok(()) => {
                self.dialog = None;
                self.close_dialog_focus(window, cx);
                self.open_dock(window, cx);
                self.toast(
                    format!("Starting port forward to {}:{remote_port}.", target.name),
                    design::Severity::Info,
                    cx,
                );
            }
            Err(reason) => {
                // Keep the input open so the user can correct the port.
                //
                // `UI-SPEC.md` §4.15: the error appears where it happened, and a toast is not
                // where it happened. This used to do both — a field error *and* a notification
                // carrying the same sentence — so one refusal was reported twice, once at the
                // field the reader is looking at and once in a corner. The transport's own words
                // now go in the field, which is also what §14.2 asks for: a forward says which
                // step failed, and the step is the field.
                let message = if reason.trim().is_empty() {
                    PORT_FORWARD_RECOVERY.to_owned()
                } else {
                    format!("{PORT_FORWARD_RECOVERY} {reason}")
                };
                if let Some(Dialog::PortForward { error: slot, .. }) = self.dialog.as_mut() {
                    *slot = Some(message);
                }
                if let Some(input) = self.dialog_input() {
                    input.update(cx, |input, cx| input.set_invalid(true, cx));
                }
                self.dialog_focus = 0;
                self.focus_dialog_control(window, cx);
                cx.notify();
            }
        }
    }

    /// Store the current focus before opening a dialog.
    fn open_dialog_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A modal must not sit under a context menu: the menu is drawn in a later
        // layer, so it would cover the dialog the user has to answer.
        self.close_tab_context_menu(window, cx);
        self.close_tree_context_menu(window, cx);
        self.close_status_panel(window, cx);
        self.dialog_focus = 0;
        self.dialog_previous_focus = window
            .focused(cx)
            .filter(|handle| !self.is_dialog_focus(handle));
        self.focus_dialog_control(window, cx);
        cx.notify();
    }

    /// Close the dialog without running its action.
    fn cancel_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.take().is_some() {
            self.close_dialog_focus(window, cx);
        }
    }

    fn close_dialog_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_focus = 0;
        match self.dialog_previous_focus.take() {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus_handle, cx),
        }
        cx.notify();
    }

    /// Run the primary action for the current dialog.
    fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::HotbarRemove { .. }) => self.confirm_hotbar_remove(window, cx),
            Some(Dialog::ConfirmTabClose { request }) => {
                let request = *request;
                self.dialog = None;
                self.close_dialog_focus(window, cx);
                self.perform_tab_close(request, window, cx);
            }
            _ => self.confirm_delete(window, cx),
        }
    }

    /// Submit the confirmed delete request.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::ConfirmDelete {
            target,
            objects,
            view,
            ..
        }) = self.dialog.take()
        else {
            return;
        };
        self.close_dialog_focus(window, cx);
        // One confirmation covers every selected row, so each object is submitted from the
        // objects the dialog captured rather than from the selection, which may have moved.
        let objects = if objects.is_empty() {
            vec![target.object.clone()]
        } else {
            objects
        };
        view.update(cx, |view, cx| {
            for object in objects {
                view.request_delete_target(object, cx);
            }
        });
    }

    /// Validate and submit the replica count.
    fn confirm_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Scale { input, .. }) = self.dialog.as_ref() else {
            return;
        };
        let text = input.read(cx).text().to_owned();
        match parse_replicas(&text) {
            Ok(replicas) => {
                let Some(Dialog::Scale { target, view, .. }) = self.dialog.take() else {
                    return;
                };
                self.close_dialog_focus(window, cx);
                view.update(cx, |view, cx| {
                    view.request_scale_target(target.object, replicas, cx)
                });
            }
            Err(message) => {
                if let Some(Dialog::Scale { error, .. }) = self.dialog.as_mut() {
                    *error = Some(message);
                }
                if let Some(input) = self.dialog_input() {
                    input.update(cx, |input, cx| input.set_invalid(true, cx));
                }
                self.dialog_focus = 0;
                self.focus_dialog_control(window, cx);
                cx.notify();
            }
        }
    }

    fn on_update_overlay_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.update_state.phase != UpdatePhase::Unsupported || !self.update_strip_expanded {
            return;
        }
        let actions = panels::UPDATE_OVERLAY_ACTIONS;
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                self.update_overlay_action = (self.update_overlay_action + actions - 1) % actions;
            }
            "down" | "right" => {
                self.update_overlay_action = (self.update_overlay_action + 1) % actions;
            }
            "home" => self.update_overlay_action = 0,
            "end" => self.update_overlay_action = actions - 1,
            "enter" | "return" | "space" => match self.update_overlay_action {
                0 => self.run_update_check(cx),
                1 => self.run_update_retry(cx),
                _ => self.run_update_restart(cx),
            },
            "escape" => {
                self.close_update_overlay(window, cx);
                return;
            }
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }

    /// Route keys for the active dialog.
    fn on_dialog_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_focus_is_field()
            && (is_text_entry_keystroke(keystroke) || is_text_edit_shortcut(keystroke))
        {
            return;
        }
        cx.stop_propagation();
        if matches!(keystroke.key.as_str(), "tab" | "shift-tab") {
            let focus_count = self.dialog_control_count();
            if focus_count == 0 {
                return;
            }
            self.dialog_focus = if keystroke.modifiers.shift {
                (self.dialog_focus + focus_count - 1) % focus_count
            } else {
                (self.dialog_focus + 1) % focus_count
            };
            self.focus_dialog_control(window, cx);
            cx.notify();
            return;
        }
        if matches!(
            self.dialog,
            Some(
                Dialog::PortForward { .. }
                    | Dialog::Scale { .. }
                    | Dialog::HotbarBankName { .. }
                    | Dialog::HelmConfirm { input: Some(_), .. }
            )
        ) {
            // The port-forward pair spends its last two indexes on Cancel and the confirm button,
            // so its button indexes differ from the one-field dialogs.
            let pair = matches!(self.dialog, Some(Dialog::PortForward { .. }));
            let cancel_focus = if pair { PORT_FORWARD_CANCEL_FOCUS } else { 1 };
            let confirm_focus = if pair { PORT_FORWARD_CONFIRM_FOCUS } else { 2 };
            match keystroke.key.as_str() {
                "escape" => self.cancel_dialog(window, cx),
                "enter" | "return" if self.dialog_focus == cancel_focus => {
                    self.cancel_dialog(window, cx)
                }
                // Enter in a field confirms, which is what a one-key form expects.
                "enter" | "return"
                    if self.dialog_focus_is_field() || self.dialog_focus == confirm_focus =>
                {
                    match &self.dialog {
                        Some(Dialog::PortForward { .. }) => self.confirm_port_forward(window, cx),
                        Some(Dialog::Scale { .. }) => self.confirm_scale(window, cx),
                        Some(Dialog::HotbarBankName { .. }) => {
                            self.confirm_hotbar_bank_name(window, cx)
                        }
                        Some(Dialog::HelmConfirm { .. }) => self.confirm_helm(window, cx),
                        _ => {}
                    }
                }
                _ if !self.dialog_focus_is_field() => {}
                _ => {
                    if let Some(input) = self.dialog_field() {
                        input.update(cx, |input, cx| {
                            input.handle_keystroke(keystroke, window, cx)
                        });
                    }
                }
            }
            return;
        }
        if matches!(self.dialog, Some(Dialog::Exec { .. })) {
            let Some(Dialog::Exec { target, selected }) = self.dialog.as_ref() else {
                return;
            };
            let containers = target.containers.len();
            let target = target.clone();
            let selected = *selected;
            match keystroke.key.as_str() {
                "escape" => self.cancel_dialog(window, cx),
                "enter" | "return" if self.dialog_focus == 1 => self.cancel_dialog(window, cx),
                "enter" | "space" | "return" => {
                    let container = target.containers.get(selected).cloned();
                    self.dialog = None;
                    self.close_dialog_focus(window, cx);
                    self.start_exec(target, container, window, cx);
                }
                "up" | "down" if containers > 0 && self.dialog_focus == 0 => {
                    let next = if keystroke.key == "up" {
                        selected.checked_sub(1).unwrap_or(containers - 1)
                    } else {
                        (selected + 1) % containers
                    };
                    if let Some(Dialog::Exec { selected, .. }) = self.dialog.as_mut() {
                        *selected = next;
                    }
                    self.focus_dialog_control(window, cx);
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        if matches!(self.dialog, Some(Dialog::HelmConfirm { input: None, .. })) {
            match keystroke.key.as_str() {
                "escape" => self.cancel_dialog(window, cx),
                "enter" | "space" | "return" => {
                    if self.dialog_focus == 1 {
                        self.confirm_helm(window, cx);
                    } else {
                        self.cancel_dialog(window, cx);
                    }
                }
                "down" | "right" => {
                    self.dialog_focus = Self::two_button_dialog_focus(self.dialog_focus, true);
                    self.focus_dialog_control(window, cx);
                    cx.notify();
                }
                "up" | "left" => {
                    self.dialog_focus = Self::two_button_dialog_focus(self.dialog_focus, false);
                    self.focus_dialog_control(window, cx);
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        // Two-button dialogs keep focus on Cancel or the primary action.
        match keystroke.key.as_str() {
            "escape" => self.cancel_dialog(window, cx),
            "enter" | "space" | "return" => {
                if self.dialog_focus == 1 {
                    self.confirm_dialog(window, cx);
                } else {
                    self.cancel_dialog(window, cx);
                }
            }
            "down" | "right" => {
                self.dialog_focus = Self::two_button_dialog_focus(self.dialog_focus, true);
                self.focus_dialog_control(window, cx);
                cx.notify();
            }
            "up" | "left" => {
                self.dialog_focus = Self::two_button_dialog_focus(self.dialog_focus, false);
                self.focus_dialog_control(window, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Dispatch command actions after the current update.
    fn run_command(
        &mut self,
        command_id: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = self
            .palette_matches()
            .into_iter()
            .find(|command| command.id == command_id)
            .cloned();
        let Some(command) = command else {
            self.rebuild_commands();
            self.palette_note = Some(
                "That command is no longer available. Close the command palette and try again.",
            );
            self.sync_palette_selection();
            cx.notify();
            return;
        };
        let current = palette_command_is_current(&command);
        match command.run {
            // The row that names the value in use is not a failure, and the footer note it used
            // to raise replaced all three key hints to say nothing the checkmark had not already
            // said. The palette stays open and the note stays as it was.
            CommandRun::Unavailable { .. } if current => {
                cx.notify();
            }
            // Keep the palette open and show why the command cannot run.
            CommandRun::Unavailable { reason, .. } => {
                self.palette_note = Some(reason);
                cx.notify();
            }
            CommandRun::Action(make_action) => {
                self.close_palette(window, cx);
                self.pending_action = Some(make_action());
                cx.defer_in(window, |this, window, cx| {
                    if let Some(action) = this.pending_action.take() {
                        window.dispatch_action(action, cx);
                    }
                });
            }
            CommandRun::Shell(handler) => {
                self.close_palette(window, cx);
                handler(self, window, cx);
            }
            CommandRun::ShellFn(handler) => {
                self.close_palette(window, cx);
                handler(self, window, cx);
            }
        }
    }

    /// Open Describe for the selected resource.
    fn describe_selection(
        &mut self,
        _: &DescribeSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.active_preview() {
            if view.read(cx).selection().is_none() {
                self.toast(
                    "Select a row to describe it.".to_owned(),
                    design::Severity::Warning,
                    cx,
                );
                return;
            }
            view.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
            self.sync_active_inspector_selection(cx);
            let focus = view.read(cx).focus_handle();
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a resource view before describing a selection.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a row to describe it.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        self.open_inspector(window, cx);
        self.inspector
            .update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        self.sync_active_inspector_selection(cx);
        self.pending_action = Some(Box::new(OpenDetails));
        cx.defer_in(window, |this, window, cx| {
            if let Some(action) = this.pending_action.take() {
                window.dispatch_action(action, cx);
            }
        });
        cx.notify();
    }

    fn open_overview(&mut self, _: &OpenOverview, window: &mut Window, cx: &mut Context<Self>) {
        self.command_open_overview(window, cx);
    }

    fn open_forwards(&mut self, _: &OpenForwards, window: &mut Window, cx: &mut Context<Self>) {
        self.command_open_forwards(window, cx);
    }

    fn apply_yaml(&mut self, _: &ApplyYaml, window: &mut Window, cx: &mut Context<Self>) {
        self.command_apply_yaml(window, cx);
    }

    fn open_logs_action(&mut self, _: &OpenLogs, window: &mut Window, cx: &mut Context<Self>) {
        self.command_show_logs(window, cx);
    }

    fn open_events_action(&mut self, _: &OpenEvents, window: &mut Window, cx: &mut Context<Self>) {
        self.command_show_events(window, cx);
    }

    fn exec_selection(&mut self, _: &ExecSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.command_exec(window, cx);
    }

    fn port_forward_selection(
        &mut self,
        _: &PortForwardSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_forward_port(window, cx);
    }

    fn restart_selection(
        &mut self,
        _: &RestartSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_restart(window, cx);
    }

    fn scale_selection(&mut self, _: &ScaleSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.command_scale(window, cx);
    }

    fn reload_keymap(&mut self, _: &ReloadKeymap, window: &mut Window, cx: &mut Context<Self>) {
        self.command_reload_keymap(window, cx);
    }

    fn use_keymap_preset(
        &mut self,
        action: &UseKeymapPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action.preset.to_ascii_lowercase().as_str() {
            "lens" | "default" => self.command_use_default_keymap(window, cx),
            "vscode" => self.command_use_vscode_keymap(window, cx),
            _ => self.toast(
                format!(
                    "Keymap preset not found: {}. Use lens, default, or vscode.",
                    action.preset
                ),
                design::Severity::Warning,
                cx,
            ),
        }
    }

    /// Apply the current YAML changes to the one editor of the active tab.
    fn command_apply_yaml(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.editor_source() else {
            self.toast(
                "Open a resource view or a Preview tab to apply YAML.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let ready = {
            let panel = source.read(cx);
            panel.is_editing(cx) && panel.is_dirty(cx)
        };
        if !ready {
            self.toast(
                "There are no YAML changes to apply. Edit the YAML, then apply it.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        source.update(cx, |panel, cx| panel.apply(cx));
    }

    /// Show logs for the selected Pod.
    fn command_show_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a Pod view before streaming logs.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let Some(request) = view.read(cx).log_request(cx) else {
            self.toast(
                "Select a Pod to stream its logs.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        self.open_logs(request, window, cx);
    }

    /// Show events for the selected resource.
    fn command_show_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_preview() {
            if view.read(cx).selection().is_none() {
                self.toast(
                    "Select a row to see its events.".to_owned(),
                    design::Severity::Warning,
                    cx,
                );
                return;
            }
            view.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
            self.sync_active_inspector_selection(cx);
            let focus = view.read(cx).focus_handle();
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a resource view before showing events.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a row to see its events.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        self.open_inspector(window, cx);
        self.inspector
            .update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        self.sync_active_inspector_selection(cx);
        cx.notify();
    }

    fn command_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a Pod view before opening a shell.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a pod to open a shell.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        view.update(cx, |view, cx| view.request_exec(window, cx));
    }

    fn request_new_port_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pods.read(cx).selection_ref(cx).is_some() {
            self.pods
                .update(cx, |view, cx| view.request_port_forward(window, cx));
            return;
        }
        if self.activate_tab(0, cx) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
        self.toast(
            "Select a Pod before you start a port forward.".to_owned(),
            design::Severity::Info,
            cx,
        );
    }

    fn command_forward_port(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a Pod view before forwarding a port.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a Pod before you start a port forward.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        view.update(cx, |view, cx| view.request_port_forward(window, cx));
    }

    fn command_restart(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a workload view before restarting a resource.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a workload row to restart it.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        view.update(cx, |view, cx| view.request_restart(cx));
    }

    fn command_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a scalable workload view before changing the replica count.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if !view.read(cx).supports_scale() {
            self.toast(
                "Scale is only available for scalable workloads. Open a scalable workload, such as a Deployment."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        if view.read(cx).selection_ref(cx).is_none() {
            self.toast(
                "Select a workload row to scale it.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        view.update(cx, |view, cx| view.request_scale_dialog(window, cx));
    }

    /// Open the Dock and start the log stream.
    fn open_logs(&mut self, request: LogRequest, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dock(window, cx);
        self.dock_panel
            .update(cx, |panel, cx| panel.open_logs(request, cx));
        self.focus_dock(window, cx);
        cx.notify();
    }

    /// Puts the keyboard in the Dock. A Dock that opened while the resource table kept the focus
    /// reads as a shortcut that did nothing: the arrows still move the table behind it, and the
    /// logs arrive where the user is not looking.
    ///
    /// The focus is taken with the frame's other deferred work, so it lands on the log list rather
    /// than on a control the Dock picked for itself while it recovered from a terminal that could
    /// not start. The log list is also the surface that keeps the navigation keys off the table.
    fn focus_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.dock_panel.read(cx).focus_handle();
        // The Dock remembers where the focus came from, so closing it returns there. An open Dock
        // keeps the target it recorded, so this only fills it in the first time.
        Self::remember_panel_focus(window, cx, &mut self.dock_previous_focus);
        let Some(handle) = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next())
        else {
            return;
        };
        cx.defer(move |cx| {
            let _ = handle.update(cx, move |_, window, cx| window.focus(&focus, cx));
        });
    }

    fn ensure_clean_for_session_change(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.active_tab_dirty(cx) {
            return true;
        }
        self.notify(
            "Apply or revert the unsaved YAML before you change contexts or namespaces.".to_owned(),
            design::Severity::Warning,
            Some("The Inspector or Preview tab has unsaved changes.".to_owned()),
            cx,
        );
        false
    }

    fn reload_kubeconfigs(
        &mut self,
        _: &ReloadKubeconfigs,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        let path = self.reload_kubeconfig_path.clone();
        #[cfg(not(test))]
        let path = None;
        self.start_kubeconfig_reload(path, cx);
    }

    fn start_kubeconfig_reload(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.reload_in_progress {
            self.toast(
                "Kubeconfigs are already reloading.".to_owned(),
                design::Severity::Info,
                cx,
            );
            return;
        }
        if !self.ensure_clean_for_session_change(cx) {
            return;
        }
        if matches!(&self.startup_state, StartupState::Unavailable(_))
            && self
                .session
                .as_ref()
                .and_then(ClusterSession::registry)
                .is_none()
        {
            self.startup_state = StartupState::Loading;
            self.session_epoch = self.session_epoch.wrapping_add(1);
            self.search_open = false;
            self.search_epoch = None;
            self.search_previous_focus = None;
            self.search.update(cx, |search, cx| {
                search.close(cx);
                search.set_executor(None, cx);
            });
            self.reset_catalog_retry();
            self.connection = ConnectionState::Connecting;
            self.catalog_failure = None;
            self.catalog_state = CatalogState::Loading;
        }
        let handle = if let Some(handle) = self
            .session
            .as_ref()
            .and_then(ClusterSession::tokio_handle)
            .cloned()
            .or_else(|| {
                self.owned_runtime
                    .as_ref()
                    .map(|runtime| runtime.handle().clone())
            }) {
            handle
        } else {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => Arc::new(runtime),
                Err(error) => {
                    self.notify(
                        "The app did not reload kubeconfigs. The current session is unchanged. Check the kubeconfig files, then try again."
                            .to_owned(),
                        design::Severity::Error,
                        Some(error.to_string()),
                        cx,
                    );
                    self.mark_startup_unavailable(error.to_string(), cx);
                    return;
                }
            };
            self.owned_runtime = Some(Arc::clone(&runtime));
            runtime.handle().clone()
        };
        let preferred = self.session.as_ref().and_then(ClusterSession::cluster_id);
        let base_epoch = self.session_epoch;
        self.reload_in_progress = true;
        self.toast(
            "Reloading kubeconfigs…".to_owned(),
            design::Severity::Info,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let task = handle.spawn(async move { load_cluster_registry(path).await });
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(format!("Kubeconfig reload task failed: {error}")),
            };
            this.update(cx, |shell, cx| {
                shell.finish_kubeconfig_reload(result, preferred, base_epoch, cx);
            })
            .ok();
        })
        .detach();
    }

    fn mark_startup_unavailable(&mut self, reason: String, cx: &mut Context<Self>) {
        if !matches!(&self.startup_state, StartupState::Loading) {
            return;
        }
        self.startup_state = StartupState::Unavailable(reason.clone());
        if let Some(ClusterSession::Unavailable {
            reason: session_reason,
            ..
        }) = self.session.as_mut()
        {
            *session_reason = reason.clone();
        }
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.search_open = false;
        self.search_epoch = None;
        self.search_previous_focus = None;
        self.search.update(cx, |search, cx| {
            search.close(cx);
            search.set_executor(None, cx);
        });
        self.reset_catalog_retry();
        self.connection = ConnectionState::Failed(reason.clone());
        self.catalog_failure = Some(reason.clone());
        self.catalog_state = CatalogState::Failed(reason.clone());
        self.catalog = None;
        self.cache = None;
        self._catalog_task = None;
        if self
            .session
            .as_ref()
            .and_then(ClusterSession::catalog)
            .is_some()
        {
            self.schedule_catalog_retry(cx);
        }
        self.namespace_state = NamespaceState::Failed(reason.clone());
        self._namespace_task = None;
        self.health_probe = None;
        self._health_task = None;
        self._health_schedule_task = None;
        self.health_started_epoch = None;
        self.latency_tier = LatencyTier::Local;
        self.sync_resource_view_pauses(cx);
        self.latency_auto_paused.clear();
        self.reset_connection_controller();
        self.sync_connection_state(cx);
        cx.notify();
    }

    fn finish_kubeconfig_reload(
        &mut self,
        result: Result<Arc<ClusterRegistry>, String>,
        preferred: Option<ClusterId>,
        base_epoch: u64,
        cx: &mut Context<Self>,
    ) {
        self.reload_in_progress = false;
        if self.session_epoch != base_epoch {
            self.toast(
                "Kubeconfig reload stopped because the active context changed. Reload kubeconfigs again if needed."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        if !self.ensure_clean_for_session_change(cx) {
            return;
        }
        let registry = match result {
            Ok(result) => result,
            Err(reason) => {
                self.mark_startup_unavailable(reason.clone(), cx);
                self.notify(
                    "The app did not reload kubeconfigs. The current session is unchanged. Check the kubeconfig files, then try again."
                    .to_owned(),
                    design::Severity::Error,
                    Some(reason),
                    cx,
                );
                return;
            }
        };
        let Some(handle) = self
            .session
            .as_ref()
            .and_then(ClusterSession::tokio_handle)
            .cloned()
            .or_else(|| {
                self.owned_runtime
                    .as_ref()
                    .map(|runtime| runtime.handle().clone())
            })
        else {
            let reason = "No Tokio runtime is available for the reloaded session.".to_owned();
            self.mark_startup_unavailable(reason.clone(), cx);
            self.notify(
                "The app did not reload kubeconfigs. The current session is unchanged. Check the kubeconfig files, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some(reason),
                cx,
            );
            return;
        };
        let next = match reloaded_session(Arc::clone(&registry), handle, preferred) {
            Ok(session) => session,
            Err(reason) => {
                self.mark_startup_unavailable(reason.clone(), cx);
                self.notify(
                    "The app did not reload kubeconfigs. The current session is unchanged. Check the kubeconfig files, then try again."
                    .to_owned(),
                    design::Severity::Error,
                    Some(reason),
                    cx,
                );
                return;
            }
        };
        if !self.apply_session(next, cx) {
            return;
        }
        if let Some(callback) = &self.registry_reload_callback {
            callback(Arc::clone(&registry));
        }
        if self.kubeconfig_warning.is_some() {
            self.toast(
                "Kubeconfigs reloaded, but some sources were skipped. Fix the skipped sources, then reload again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
        } else {
            self.toast(
                "Kubeconfigs reloaded.".to_owned(),
                design::Severity::Success,
                cx,
            );
        }
    }

    // Keymap commands

    /// Show feedback for keymap status changes.
    fn on_keymap_status_changed(&mut self, cx: &mut Context<Self>) {
        let status = keymap::status(cx);
        if let Some(message) = status.toast_message() {
            self.toast(message, status.toast_severity(), cx);
        }
    }

    /// Create the keymap file if needed, then open it.
    fn command_create_or_show_keymap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = keymap::user_keymap_path() else {
            self.notify(
                "The configuration directory was not found. Check the user configuration, then try again."
                    .to_owned(),
                design::Severity::Error,
                None,
                cx,
            );
            return;
        };
        let created = match keymap::ensure_user_keymap_file(&path) {
            Ok(created) => created,
            Err(error) => {
                self.notify(
                    "The app did not create the keymap file. Check the configuration directory, then try again."
                        .to_owned(),
                    design::Severity::Error,
                    Some(error),
                    cx,
                );
                return;
            }
        };
        if let Err(error) = keymap::open_user_keymap_file(&path) {
            self.notify(
                "The system did not open the keymap file with the default program. Check the file association, then try again."
                    .to_owned(),
                design::Severity::Error,
                Some(error),
                cx,
            );
            return;
        }
        let message = if created {
            format!("Keymap file created and opened: {}", path.display())
        } else {
            format!("Keymap file opened: {}", path.display())
        };
        self.toast(message, design::Severity::Success, cx);
    }

    /// Reload the user keymap file.
    fn command_reload_keymap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        keymap::reload(cx).ok();
    }

    fn command_use_default_keymap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        keymap::set_preset(cx, KeymapPreset::Lens).ok();
    }

    fn command_use_vscode_keymap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        keymap::set_preset(cx, KeymapPreset::Vscode).ok();
    }

    fn focus_yaml_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preview = self.active_preview();
        let panel = self.editor_source().or(preview.clone());
        let Some(panel) = panel else {
            self.toast(
                "Open a resource view or a Preview tab to edit YAML.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        // "No document yet" is not "no object". Selecting a row no longer loads the
        // document eagerly — `UI-REDESIGN` D27 makes the YAML tab the fallback
        // rather than the landing view, so the first `⌘⇧Y` on a fresh row arrives
        // before the document does. Reporting that as "Select a row to show its
        // YAML" denies a step the reader already took, and the chord silently did
        // nothing. Only a panel with no selection has nothing to show.
        let has_document = panel.read(cx).has_yaml();
        if !has_document && panel.read(cx).selection().is_none() {
            panel.update(cx, |panel, cx| {
                panel.set_editable(false, window, cx);
            });
            self.toast(
                "Select a row to show its YAML.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        panel.update(cx, |panel, cx| {
            panel.show_tab(InspectorTab::Yaml, cx);
            panel.set_editable(true, window, cx);
        });
        if preview.is_some() {
            // The editor's element is not in the tree yet: `show_tab` records which
            // tab is current, and the YAML view is built when the tab is painted.
            // Focusing a handle whose element does not exist is a silent no-op, so
            // the chord changed the tab and left the caret behind. One deferred
            // frame is the point at which the view is in the tree.
            let panel = panel.clone();
            window.defer(cx, move |window, cx| {
                let focus = panel.read(cx).yaml_focus_handle(cx);
                window.focus(&focus, cx);
            });
        } else {
            self.open_inspector(window, cx);
        }
        self.sync_active_inspector_selection(cx);
        cx.notify();
    }

    fn focus_yaml(&mut self, _: &FocusYaml, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_yaml_content(window, cx);
    }

    fn focus_yaml_compat(&mut self, _: &EditYaml, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_yaml_content(window, cx);
    }

    /// Apply Inspector YAML and update the panel with the result.
    /// Sends the document under review to the API server for validation, without storing it.
    ///
    /// This is the question a local diff cannot answer. It never writes, so a failure here is
    /// information rather than a problem, and the review keeps the apply path open either way.
    fn check_from_inspector(
        &mut self,
        panel: Entity<InspectorPanel>,
        request: ApplyRequest,
        cx: &mut Context<Self>,
    ) {
        let identity = panel.read(cx).session_identity();
        if panel.read(cx).current_apply_request().as_ref() != Some(&request)
            || request.target.session != Some(identity)
        {
            return;
        }
        #[cfg(test)]
        let task = self
            .check_future
            .as_ref()
            .map(|future| future(request.clone()));
        #[cfg(not(test))]
        let task: Option<
            crate::panels::inspector_data::OpsFuture<k8s_core::ops::ApplyCheck>,
        > = None;
        let task = match task {
            Some(task) => task,
            None => {
                let Some(handle) = self.cluster_handle.clone() else {
                    panel.update(cx, |panel, cx| {
                        panel.apply_check_finished(
                            Err(
                                "Not connected to a context. Select a context, then check the \
                                 change."
                                    .to_owned(),
                            ),
                            cx,
                        );
                    });
                    return;
                };
                handle.check_apply_future(&request.target.object_ref(), request.yaml.clone())
            }
        };
        let epoch = self.session_epoch;
        let result_panel = panel.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this
                .update(cx, |shell, cx| {
                    // A verdict belongs to the document and the session that asked for it. A reply
                    // that arrives after either moved on is dropped rather than shown against the
                    // wrong object.
                    if shell.session_epoch != epoch
                        || result_panel.read(cx).session_identity() != identity
                        || result_panel.read(cx).current_apply_request().as_ref() != Some(&request)
                    {
                        return;
                    }
                    let verdict = result.map(|check| match check {
                        k8s_core::ops::ApplyCheck::Valid(_) => ApplyVerdict::Valid,
                        k8s_core::ops::ApplyCheck::Conflict { owners } => {
                            ApplyVerdict::Conflict { owners }
                        }
                    });
                    result_panel.update(cx, |panel, cx| {
                        panel.apply_check_finished(verdict, cx);
                    });
                })
                .ok();
        })
        .detach();
    }

    fn apply_from_inspector(
        &mut self,
        panel: Entity<InspectorPanel>,
        request: ApplyRequest,
        cx: &mut Context<Self>,
    ) {
        let identity = panel.read(cx).session_identity();
        if panel.read(cx).current_apply_request().as_ref() != Some(&request)
            || request.target.session != Some(identity)
        {
            return;
        }
        #[cfg(test)]
        let task = self
            .apply_future
            .as_ref()
            .map(|future| future(request.clone()));
        #[cfg(not(test))]
        let task: Option<
            crate::panels::inspector_data::OpsFuture<crate::panels::ApplyOutcome>,
        > = None;
        let task = match task {
            Some(task) => task,
            None => {
                let Some(handle) = self.cluster_handle.clone() else {
                    panel.update(cx, |panel, cx| {
                        panel.apply_finished_for(
                            request,
                            Err(
                                "Not connected to a context. Select a context, then apply the YAML again."
                                    .to_owned(),
                            ),
                            cx,
                        );
                    });
                    return;
                };
                handle.apply_future(&request.target.object_ref(), request.yaml.clone())
            }
        };
        let label = match &request.target.namespace {
            Some(namespace) => format!("{}/{}", namespace, request.target.name),
            None => request.target.name.clone(),
        };
        let epoch = self.session_epoch;
        let result_panel = panel.clone();
        let target = request.target.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |shell, cx| {
                if shell.session_epoch != epoch
                    || result_panel.read(cx).session_identity() != identity
                    || result_panel.read(cx).current_apply_request().as_ref() != Some(&request)
                {
                    return;
                }
                let applied = match &result {
                    Ok(crate::panels::ApplyOutcome::Applied(object)) => {
                        target.matches_object(object.as_ref())
                    }
                    _ => false,
                };
                let mismatch =
                    matches!(&result, Ok(crate::panels::ApplyOutcome::Applied(_))) && !applied;
                result_panel.update(cx, |panel, cx| {
                    panel.apply_finished_for(request, result, cx);
                });
                if mismatch {
                    shell.notify(
                        format!("The apply result for {label} did not match the selected object."),
                        design::Severity::Error,
                        Some(
                            "Refresh the object to see whether the server applied the change."
                                .to_owned(),
                        ),
                        cx,
                    );
                    return;
                }
                if applied {
                    if let Some(view) = shell.active_resource_view() {
                        view.update(cx, |view, cx| view.refresh(cx));
                    }
                    shell.toast(format!("Applied {label}"), design::Severity::Success, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Show transient feedback. Errors remain until dismissed.
    pub(super) fn toast(
        &mut self,
        message: String,
        severity: design::Severity,
        cx: &mut Context<Self>,
    ) {
        self.toast_with_detail(message, severity, None, cx);
    }

    /// Show feedback and retain its detail in the notification center.
    pub(super) fn notify(
        &mut self,
        message: String,
        severity: design::Severity,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.toast_with_detail(message, severity, detail, cx);
    }

    /// Show feedback that knows its own way out.
    ///
    /// The action is a closure over whatever the caller had in hand when it raised the message,
    /// so the recovery is the one that fits the failure instead of a generic retry.
    fn toast_with_action(
        &mut self,
        message: String,
        severity: design::Severity,
        action: ToastAction,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.push_notification(message.clone(), severity, detail, cx);
        self.toast = Some(Toast {
            message: message.into(),
            severity,
            action: Some(action),
        });
        self.schedule_toast_expiry(severity, cx);
        cx.notify();
    }

    fn toast_with_detail(
        &mut self,
        message: String,
        severity: design::Severity,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.push_notification(message.clone(), severity, detail, cx);
        self.toast = Some(Toast {
            message: message.into(),
            severity,
            action: None,
        });
        self.schedule_toast_expiry(severity, cx);
        cx.notify();
    }

    /// Take the toast away after `TOAST_DURATION`, unless it reports a failure.
    fn schedule_toast_expiry(&mut self, severity: design::Severity, cx: &mut Context<Self>) {
        // Every toast takes the epoch, so a timer left over from the previous one cannot come
        // back and clear this one. That includes the error case: the epoch moves first, and only
        // the timer is skipped.
        let epoch = self.toast_epoch.wrapping_add(1);
        self.toast_epoch = epoch;
        if severity == design::Severity::Error {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_DURATION).await;
            this.update(cx, |shell, cx| {
                if shell.toast_epoch == epoch {
                    shell.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Add feedback to the notification history and collapse immediate duplicates.
    fn push_notification(
        &mut self,
        message: String,
        severity: design::Severity,
        detail: Option<String>,
        _cx: &mut Context<Self>,
    ) {
        let duplicate = self.notifications.last().is_some_and(|last| {
            last.message.as_ref() == message
                && last.severity == severity
                && last.at.elapsed() < Duration::from_secs(2)
        });
        if duplicate {
            return;
        }
        self.notification_seq = self.notification_seq.wrapping_add(1);
        let notification = Notification {
            id: self.notification_seq,
            message: message.into(),
            severity,
            detail,
            at: std::time::Instant::now(),
            expanded: false,
        };
        self.notifications.push(notification);
        if self.notifications.len() > NOTIFICATION_CAPACITY {
            self.notifications.remove(0);
        }
    }

    /// Open the cluster overview.
    fn command_open_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_special_tab(TabContent::Overview, "Overview", IconName::Monitor, cx) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
    }

    fn command_open_forwards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_special_tab(
            TabContent::Forwards,
            "Port Forwards",
            IconName::ArrowRightLeft,
            cx,
        ) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
    }

    /// Open Helm releases.
    fn command_open_helm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.helm.is_none() {
            self.notify(
                self.helm_state
                    .reason()
                    .unwrap_or(crate::panels::helm::HELM_NOT_INSTALLED)
                    .to_owned(),
                design::Severity::Warning,
                None,
                cx,
            );
            return;
        }
        if self.open_special_tab(TabContent::Helm, "Helm", IconName::Archive, cx) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
    }

    /// Open the confirmation dialog for a Helm action.
    fn open_helm_confirm(
        &mut self,
        action: HelmAction,
        view: Entity<HelmView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&action, HelmAction::Upgrade { .. }) {
            let epoch = self.dialog_input_epoch.wrapping_add(1);
            self.dialog_input_epoch = epoch;
            let initial = match &action {
                HelmAction::Upgrade { chart, .. } => chart.clone(),
                _ => String::new(),
            };
            let input = self.new_dialog_input(
                DialogInputKind::HelmChartReference,
                initial,
                epoch,
                dialog_width(f32::from(window.viewport_size().width)),
                cx,
            );
            self.dialog = Some(Dialog::HelmConfirm {
                action,
                view,
                input: Some(input),
                error: None,
            });
        } else {
            self.dialog = Some(Dialog::HelmConfirm {
                action,
                view,
                input: None,
                error: None,
            });
        }
        self.open_dialog_focus(window, cx);
    }

    fn confirm_helm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::HelmConfirm {
            action,
            view,
            input,
            ..
        }) = self.dialog.as_ref()
        else {
            return;
        };
        let mut action = action.clone();
        if let HelmAction::Upgrade { chart, .. } = &mut action {
            let Some(input) = input else {
                return;
            };
            match parse_chart_reference(input.read(cx).text()) {
                Ok(reference) => *chart = reference,
                Err(message) => {
                    if let Some(Dialog::HelmConfirm { error, .. }) = self.dialog.as_mut() {
                        *error = Some(message.to_owned());
                    }
                    if let Some(input) = self.dialog_input() {
                        input.update(cx, |input, cx| input.set_invalid(true, cx));
                    }
                    self.dialog_focus = 0;
                    self.focus_dialog_control(window, cx);
                    cx.notify();
                    return;
                }
            }
        }
        let view = view.clone();
        self.dialog = None;
        self.close_dialog_focus(window, cx);
        view.update(cx, |view, cx| view.run_action(action, cx));
    }

    // External capability probes

    /// Refresh capability probes for the current session.
    fn refresh_capability_probes(&mut self, cx: &mut Context<Self>) {
        self.helm = None;
        self.helm_state = HelmCapability::Checking;
        self.helm_error = None;
        self.begin_metrics_probe(cx);
        self.rebuild_commands();
        let Some(session) = self.session.clone() else {
            self.finish_capability_probes(cx);
            return;
        };
        let Some(handle) = session.tokio_handle().cloned() else {
            self.finish_capability_probes(cx);
            return;
        };
        let epoch = self.session_epoch;

        let helm_handle = handle.clone();
        cx.spawn(async move |this, cx| {
            let task = helm_handle.spawn(async { Helm::detect().await });
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(HelmError::Io(std::io::Error::other(error))),
            };
            this.update(cx, |shell, cx| {
                if shell.session_epoch != epoch {
                    return;
                }
                shell.on_helm_probed(result, cx);
            })
            .ok();
        })
        .detach();

        let Some(metrics) = metrics_handle_for(&session) else {
            self.finish_metrics_probe(cx);
            return;
        };
        self.start_metrics_probe(metrics, cx);
    }

    fn begin_metrics_probe(&mut self, cx: &mut Context<Self>) {
        self._metrics_probe_task = None;
        self.metrics_probe_epoch = self.metrics_probe_epoch.wrapping_add(1);
        self.metrics_probe = MetricsProbeState::Checking;
        self.sync_metrics_probe_views(MetricsProbeState::Checking, cx);
        self.sync_settings_capabilities(cx);
        cx.notify();
    }

    fn start_metrics_probe(&mut self, metrics: MetricsHandle, cx: &mut Context<Self>) {
        let probe = metrics.probe_future();
        let session_epoch = self.session_epoch;
        let probe_epoch = self.metrics_probe_epoch;
        self._metrics_probe_task = Some(cx.spawn(async move |this, cx| {
            let state = MetricsProbeState::from_result(probe.await);
            this.update(cx, |shell, cx| {
                if shell.session_epoch == session_epoch && shell.metrics_probe_epoch == probe_epoch
                {
                    shell.on_metrics_probed(state, cx);
                }
            })
            .ok();
        }));
    }

    fn retry_metrics_probe(&mut self, cx: &mut Context<Self>) {
        self.begin_metrics_probe(cx);
        let Some(metrics) = self.session.as_ref().and_then(metrics_handle_for) else {
            self.finish_metrics_probe(cx);
            return;
        };
        self.start_metrics_probe(metrics, cx);
    }

    fn finish_metrics_probe(&mut self, cx: &mut Context<Self>) {
        self._metrics_probe_task = None;
        self.metrics_probe = MetricsProbeState::Missing;
        self.sync_metrics_probe_views(MetricsProbeState::Missing, cx);
        self.sync_settings_capabilities(cx);
        cx.notify();
    }

    fn finish_capability_probes(&mut self, cx: &mut Context<Self>) {
        self.helm = None;
        self.helm_state = HelmCapability::NotInstalled;
        self.helm_error = self.helm_state.reason().map(str::to_owned);
        self._metrics_probe_task = None;
        self.metrics_probe = MetricsProbeState::Missing;
        self.rebuild_commands();
        self.sync_metrics_probe_views(MetricsProbeState::Missing, cx);
        self.sync_settings_capabilities(cx);
        cx.notify();
    }

    fn sync_metrics_probe_views(&self, state: MetricsProbeState, cx: &mut Context<Self>) {
        self.inspector.update(cx, |panel, cx| {
            panel.set_metrics_probe_state(state.clone(), cx)
        });
        for slot in &self.views {
            if let Some(TabView::Preview(view)) = slot {
                view.update(cx, |panel, cx| {
                    panel.set_metrics_probe_state(state.clone(), cx)
                });
            }
        }
    }

    fn on_helm_probed(&mut self, result: Result<Helm, HelmError>, cx: &mut Context<Self>) {
        let (helm, capability, reason) = match result {
            Ok(helm) => (Some(helm), HelmCapability::Available, None),
            Err(error) => {
                let capability = HelmCapability::from_error(&error);
                eprintln!("k8s-gpui: helm capability probe failed: {error}");
                (None, capability, capability.reason().map(str::to_owned))
            }
        };
        self.helm = helm;
        self.helm_state = capability;
        self.helm_error = reason.clone();
        self.rebuild_commands();
        // Refresh the client for each open Helm view.
        let services = self.helm_services();
        for slot in &self.views {
            if let Some(TabView::Helm(view)) = slot {
                self.sync_helm_cluster(view, cx);
                let reason = reason.clone();
                view.update(cx, |view, cx| {
                    view.set_capability(capability, reason.clone(), cx);
                    view.set_client(services.bind(), services.handle.clone(), cx)
                });
            }
        }
        self.sync_settings_capabilities(cx);
        cx.notify();
    }

    fn on_metrics_probed(&mut self, state: MetricsProbeState, cx: &mut Context<Self>) {
        self.metrics_probe = state.clone();
        let available = state.is_available();
        self.sync_metrics_probe_views(state, cx);
        let overviews: Vec<Entity<OverviewView>> = self
            .views
            .iter()
            .filter_map(|slot| match slot.as_ref() {
                Some(TabView::Overview(view)) => Some(view.clone()),
                _ => None,
            })
            .collect();
        for view in overviews {
            view.update(cx, |view, cx| view.set_metrics_available(available, cx));
        }
        self.sync_settings_capabilities(cx);
        cx.notify();
    }

    fn metrics_capability(&self) -> Capability {
        match &self.metrics_probe {
            MetricsProbeState::Checking => Capability::Checking,
            MetricsProbeState::Available => Capability::Available,
            MetricsProbeState::Missing => Capability::Unavailable,
            MetricsProbeState::Forbidden { .. } | MetricsProbeState::Error { .. } => {
                Capability::Error
            }
        }
    }

    fn sync_settings_capabilities(&mut self, cx: &mut Context<Self>) {
        let helm = self.helm_state;
        let metrics = self.metrics_capability();
        let helm_error = self.helm_error.clone();
        let metrics_error = self.metrics_probe.reason().map(str::to_owned);
        for slot in &self.views {
            if let Some(TabView::Settings(view)) = slot {
                view.update(cx, |view, cx| {
                    view.set_capabilities(helm, metrics, cx);
                    view.set_capability_errors(helm_error.clone(), metrics_error.clone(), cx);
                });
            }
        }
    }

    fn current_kind(&self) -> Option<(SharedString, Option<GroupVersionKind>)> {
        self.tabs.get(self.active_tab).and_then(|tab| {
            (tab.content == TabContent::Resource).then(|| (tab.kind.clone(), tab.identity.clone()))
        })
    }

    fn palette_status_command(
        prefix: &'static str,
        state: &'static str,
        group: &'static str,
        label: String,
        badge: &'static str,
        reason: &'static str,
        icon: IconName,
    ) -> Command {
        Command {
            id: stable_command_id(prefix, state),
            label: SharedString::from(label),
            group: SharedString::from(group),
            icon,
            run: CommandRun::Unavailable { badge, reason },
            binding: None,
        }
    }

    fn current_palette_command(
        prefix: &str,
        value: &str,
        group: &'static str,
        label: String,
        icon: IconName,
    ) -> Command {
        Command {
            id: stable_command_id(prefix, value),
            label: SharedString::from(label),
            group: SharedString::from(group),
            icon,
            // Not a failure and not a no-op button: the row is the value in use, which the card
            // marks with its check. `badge` is how the renderer recognises that mark, and the
            // reason is only reached if the row is activated, which keeps the palette open and
            // silent.
            run: CommandRun::Unavailable {
                badge: PALETTE_CURRENT_BADGE,
                reason: "This is the value already in use.",
            },
            binding: None,
        }
    }

    /// Rows for the context switcher, each a bare context name.
    ///
    /// A verb and a subject in front of every row pushed the name a reader is
    /// looking for to the right, and gave the search box a word that matched every
    /// row, so a query narrowed nothing. The verb belongs to the palette title,
    /// and "which one is current" is the badge and the check the row already
    /// carries. The stable id keeps the verb, so the switcher's own query still
    /// matches.
    fn context_switch_commands(&self) -> Vec<Command> {
        let current = self.clusters.get(self.active_cluster);
        let mut commands = Vec::with_capacity(self.clusters.len() + 1);
        for (index, name) in self.clusters.iter().enumerate() {
            if current == Some(name) {
                commands.push(Self::current_palette_command(
                    "context.switch",
                    name.as_ref(),
                    "Context",
                    name.as_ref().into(),
                    IconName::Server,
                ));
                continue;
            }
            let handler_name = name.clone();
            commands.push(Command {
                id: stable_command_id("context.switch", name.as_ref()),
                label: name.as_ref().into(),
                group: SharedString::from("Context"),
                icon: IconName::Server,
                run: CommandRun::ShellFn(Rc::new(move |shell, window, cx| {
                    shell.switch_context_by_name(&handler_name, index, window, cx);
                })),
                binding: None,
            });
        }
        match &self.startup_state {
            StartupState::Loading => commands.push(Self::palette_status_command(
                "context.status",
                "loading",
                "Context",
                "Contexts Are Loading".to_owned(),
                "Loading",
                "Wait for kubeconfig contexts to finish loading.",
                IconName::Server,
            )),
            StartupState::Unavailable(_) => commands.push(Self::palette_status_command(
                "context.status",
                "failed",
                "Context",
                "Contexts Unavailable".to_owned(),
                "Unavailable",
                "Fix the kubeconfig files, then refresh kubeconfigs.",
                IconName::Server,
            )),
            StartupState::Ready if self.clusters.is_empty() => {
                commands.push(Self::palette_status_command(
                    "context.status",
                    "empty",
                    "Context",
                    "No Contexts Available".to_owned(),
                    "Empty",
                    "Add a context to your kubeconfig and refresh.",
                    IconName::Server,
                ))
            }
            StartupState::Ready if current.is_none() => {
                commands.push(Self::palette_status_command(
                    "context.status",
                    "empty",
                    "Context",
                    "No Active Context".to_owned(),
                    "Empty",
                    "Choose a context from this list to switch to it.",
                    IconName::Server,
                ))
            }
            StartupState::Ready => {}
        }
        commands
    }

    /// Rows for the namespace switcher, each a bare namespace name.
    ///
    /// A list is read as one set, so the current row is phrased exactly like its
    /// siblings and is marked by the badge the row already carries. The stable id
    /// keeps the verb, so the switcher's own query still matches.
    fn namespace_switch_commands(&self) -> Vec<Command> {
        let mut targets = vec![SharedString::from(ALL_NAMESPACES)];
        match &self.namespace_state {
            NamespaceState::Ready(names) => targets.extend(names.iter().cloned()),
            NamespaceState::Loading | NamespaceState::Failed(_) => {}
        }
        if self.namespace.as_ref() != ALL_NAMESPACES
            && !targets
                .iter()
                .any(|name| name.as_ref() == self.namespace.as_ref())
        {
            targets.push(self.namespace.clone());
        }
        let mut commands = targets
            .into_iter()
            .map(|name| {
                if name.as_ref() == self.namespace.as_ref() {
                    Self::current_palette_command(
                        "namespace.switch",
                        name.as_ref(),
                        "Namespace",
                        name.as_ref().to_owned(),
                        IconName::Folder,
                    )
                } else {
                    let target = name.clone();
                    Command {
                        id: stable_command_id("namespace.switch", name.as_ref()),
                        label: name,
                        group: SharedString::from("Namespace"),
                        icon: IconName::Folder,
                        run: CommandRun::ShellFn(Rc::new(move |shell, _, cx| {
                            shell.switch_namespace_by_name(&target, cx);
                        })),
                        binding: None,
                    }
                }
            })
            .collect::<Vec<_>>();
        match &self.namespace_state {
            NamespaceState::Loading => commands.push(Self::palette_status_command(
                "namespace.status",
                "loading",
                "Namespace",
                "Namespaces Are Loading".to_owned(),
                "Loading",
                "Wait for the namespace list to finish loading.",
                IconName::Folder,
            )),
            NamespaceState::Failed(_) => commands.push(Self::palette_status_command(
                "namespace.status",
                "failed",
                "Namespace",
                "Namespace List Unavailable".to_owned(),
                "Unavailable",
                "Refresh the namespace list, then try again.",
                IconName::Folder,
            )),
            NamespaceState::Ready(names) if names.is_empty() => {
                commands.push(Self::palette_status_command(
                    "namespace.status",
                    "empty",
                    "Namespace",
                    "No Namespaces Found".to_owned(),
                    "Empty",
                    "All namespaces remains available. Add namespaces to narrow the scope.",
                    IconName::Folder,
                ))
            }
            NamespaceState::Ready(_) => {}
        }
        commands
    }

    /// Rows for the resource-kind switcher, each a bare kind name.
    ///
    /// The name comes off the tree row, the surface that already humanizes a
    /// kind, so a picker row, the sidebar, and a search result cannot drift apart
    /// and a raw Kind cannot reach a reader. The stable id keeps the verb, so the
    /// switcher's own query still matches.
    fn kind_switch_commands(&self) -> Vec<Command> {
        let current = self.current_kind();
        let mut commands = Vec::new();
        let active_cluster = self.clusters.get(self.active_cluster);
        let rows: Vec<TreeRow> = match active_cluster {
            Some(name) => self.tree.rows_for_cluster(name.as_ref(), &HashSet::new()),
            None => Vec::new(),
        };
        let labels: HashMap<&str, &str> = rows
            .iter()
            .filter(|row| row.kind == TreeRowKind::Kind)
            .filter_map(|row| Some((row.resource_kind.as_deref()?, row.label.as_str())))
            .collect();
        if let Some((kind, identity)) = current.as_ref() {
            let identity_key = identity
                .as_ref()
                .map(|identity| {
                    format!("{}/{}/{}", identity.group, identity.version, identity.kind)
                })
                .unwrap_or_else(|| kind.to_string());
            commands.push(Self::current_palette_command(
                "kind.open",
                &identity_key,
                "Resource Kinds",
                labels
                    .get(kind.as_str())
                    .map_or_else(|| kind.to_string(), |label| (*label).to_owned()),
                design::kind_icon(kind.as_ref()),
            ));
        }
        if active_cluster.is_none() {
            commands.push(Self::palette_status_command(
                "kind.status",
                "empty",
                "Resource Kinds",
                "No Active Context".to_owned(),
                "Empty",
                "Select a context before opening a resource kind.",
                IconName::ListTree,
            ));
            return commands;
        }
        let mut kind_rows = 0;
        for row in rows.into_iter().filter(|row| row.kind == TreeRowKind::Kind) {
            let Some(kind) = row.resource_kind.clone() else {
                continue;
            };
            kind_rows += 1;
            let identity = row
                .resource_gvk
                .clone()
                .or_else(|| row.entry.as_ref().map(ResourceEntry::identity))
                .or_else(|| known_resource_identity(kind.as_ref()));
            if current
                .as_ref()
                .is_some_and(|(current_kind, current_identity)| {
                    current_kind == &kind
                        && (current_identity.is_none() || current_identity == &identity)
                })
            {
                continue;
            }
            let identity_key = identity
                .as_ref()
                .map(|identity| {
                    format!("{}/{}/{}", identity.group, identity.version, identity.kind)
                })
                .unwrap_or_else(|| kind.to_string());
            let target = kind.clone();
            let handler_identity = identity.clone();
            commands.push(Command {
                id: stable_command_id("kind.open", &identity_key),
                label: row.label,
                group: SharedString::from("Resource Kinds"),
                icon: design::kind_icon(kind.as_ref()),
                run: CommandRun::ShellFn(Rc::new(move |shell, window, cx| {
                    shell.switch_kind_by_name(&target, handler_identity.as_ref(), window, cx);
                })),
                binding: None,
            });
        }
        match &self.catalog_state {
            CatalogState::Loading => commands.push(Self::palette_status_command(
                "kind.status",
                "loading",
                "Resource Kinds",
                "Resource Kinds Are Loading".to_owned(),
                "Loading",
                "Wait for the resource catalog to finish loading.",
                IconName::ListTree,
            )),
            CatalogState::Failed(_) => commands.push(Self::palette_status_command(
                "kind.status",
                "failed",
                "Resource Kinds",
                "Resource Kinds Unavailable".to_owned(),
                "Unavailable",
                "Refresh resources, then try again.",
                IconName::ListTree,
            )),
            CatalogState::Ready if kind_rows == 0 => commands.push(Self::palette_status_command(
                "kind.status",
                "empty",
                "Resource Kinds",
                "No Resource Kinds Found".to_owned(),
                "Empty",
                "Refresh resources after installing a resource API.",
                IconName::ListTree,
            )),
            CatalogState::Ready => {}
        }
        commands
    }

    fn switch_context_by_name(
        &mut self,
        name: &SharedString,
        captured_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.clusters.iter().position(|cluster| cluster == name) else {
            self.toast(
                "That context is no longer available. Refresh kubeconfigs and try again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        if index != captured_index {
            self.rebuild_commands();
        }
        if self.switch_cluster(index, cx) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
    }

    fn switch_namespace_by_name(&mut self, name: &SharedString, cx: &mut Context<Self>) {
        let available =
            matches!(&self.namespace_state, NamespaceState::Ready(names) if names.contains(name));
        if !available {
            self.toast(
                "That namespace is no longer available. Refresh the namespace list and try again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        }
        self.set_namespace(name.clone(), cx);
    }

    fn switch_kind_by_name(
        &mut self,
        kind: &SharedString,
        identity: Option<&GroupVersionKind>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cluster) = self
            .clusters
            .get(self.active_cluster)
            .map(|name| name.as_ref())
        else {
            self.toast(
                "Select a context before opening a resource kind.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let row = self
            .tree
            .rows_for_cluster(cluster, &HashSet::new())
            .into_iter()
            .find(|row| {
                let row_identity = row
                    .resource_gvk
                    .clone()
                    .or_else(|| row.entry.as_ref().map(ResourceEntry::identity))
                    .or_else(|| known_resource_identity(kind.as_ref()));
                row.kind == TreeRowKind::Kind
                    && row.resource_kind.as_ref() == Some(kind)
                    && (identity.is_none_or(|expected| row_identity.as_ref() == Some(expected)))
            });
        let Some(row) = row else {
            self.toast(
                "That resource kind is no longer available. Refresh resources and try again."
                    .to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let current_identity = row
            .resource_gvk
            .clone()
            .or_else(|| row.entry.as_ref().map(ResourceEntry::identity))
            .or_else(|| known_resource_identity(kind.as_ref()));
        if self.open_resource_tab(
            kind.clone(),
            row.label.clone(),
            current_identity,
            row.entry.clone(),
            cx,
        ) {
            self.focus_active_view_and_clear_pending(window, cx);
        }
    }

    fn rebuild_commands(&mut self) {
        let mut commands =
            demo_commands_with_capabilities(self.helm_state, self.update_actions.is_some());
        commands.extend(self.context_switch_commands());
        commands.extend(self.namespace_switch_commands());
        commands.extend(self.kind_switch_commands());
        self.commands = commands;
        self.sync_palette_selection();
    }

    fn pause_updates(&mut self, _: &PauseUpdates, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a resource view before pausing updates.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        self.latency_auto_paused.remove(&self.active_tab);
        view.update(cx, |view, cx| view.pause(cx));
        cx.notify();
    }

    fn resume_updates(&mut self, _: &ResumeUpdates, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a resource view before resuming updates.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        self.latency_auto_paused.remove(&self.active_tab);
        view.update(cx, |view, cx| view.resume(cx));
        cx.notify();
    }

    fn copy_selected_pod_name(
        &mut self,
        _: &CopySelectedPodName,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.active_resource_view() else {
            self.toast(
                "Open a resource view before copying a name.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        let Some(name) = view.read(cx).selected_name(cx) else {
            self.toast(
                "Select a resource before copying its name.".to_owned(),
                design::Severity::Warning,
                cx,
            );
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
        // The clipboard is the one place a desktop app can act where the reader cannot see
        // the result. A copy with no word at all reads as a chord that did nothing, and the
        // reader presses it again — which is how a working shortcut gets abandoned. Naming
        // what was copied also makes the toast the receipt a paste elsewhere can be checked
        // against, so it is `Severity::Info` and it expires like every other.
        self.toast(format!("Copied {name}."), design::Severity::Info, cx);
    }

    // Resource catalog loading

    fn retry_catalog(&mut self, cx: &mut Context<Self>) {
        self.catalog_retry_attempt = 0;
        self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
        self.catalog_retry_task = None;
        self.catalog_retry_focus_tree = true;
        self.invalidate_catalog_task();
        self.start_catalog_load(cx);
    }

    fn schedule_catalog_retry(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.catalog_state, CatalogState::Failed(_)) {
            return;
        }
        let delay = catalog_retry_delay(self.catalog_retry_attempt);
        self.catalog_retry_attempt = self.catalog_retry_attempt.saturating_add(1);
        let generation = self.catalog_retry_generation.wrapping_add(1);
        self.catalog_retry_generation = generation;
        let epoch = self.session_epoch;
        let load_generation = self.catalog_load_generation;
        self.catalog_retry_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |shell, cx| {
                if shell.catalog_retry_generation != generation {
                    return;
                }
                if shell.session_epoch != epoch
                    || shell.catalog_load_generation != load_generation
                    || !matches!(shell.catalog_state, CatalogState::Failed(_))
                {
                    shell.catalog_retry_task = None;
                    return;
                }
                shell.catalog_retry_task = None;
                shell.start_catalog_load(cx);
            })
            .ok();
        }));
    }

    fn start_catalog_load(&mut self, cx: &mut Context<Self>) {
        if matches!(self.catalog_state, CatalogState::Loading) && self._catalog_task.is_some() {
            return;
        }
        self.catalog_retry_task = None;
        self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
        self.invalidate_catalog_task();
        let previous_failure = self.catalog_failure.clone();
        self.catalog_failure = None;
        let Some(loader) = self
            .catalog
            .clone()
            .or_else(|| self.session.as_ref().and_then(ClusterSession::catalog))
        else {
            self._catalog_task = None;
            self.catalog_state = if matches!(&self.startup_state, StartupState::Loading) {
                CatalogState::Loading
            } else {
                let reason = previous_failure.unwrap_or_else(|| {
                    "No context connection is available. Select a context, then try again."
                        .to_owned()
                });
                self.catalog_failure = Some(reason.clone());
                CatalogState::Failed(reason)
            };
            cx.notify();
            return;
        };
        self.catalog = Some(loader.clone());
        self.catalog_state = CatalogState::Loading;
        let epoch = self.session_epoch;
        let generation = self.catalog_load_generation;
        let cache = self.cache.clone();
        #[cfg(test)]
        let injected = self.catalog_future.as_ref().map(|future| future());
        self._catalog_task = Some(cx.spawn(async move |this, cx| {
            #[cfg(test)]
            let cached_hit = false;
            #[cfg(not(test))]
            let mut cached_hit = false;
            #[cfg(not(test))]
            if let Some(cache) = &cache
                && let Some(cached) = loader.spawn_cached(Arc::clone(cache)).await.unwrap_or(None)
            {
                cached_hit = true;
                let stale = cached.stale;
                this.update(cx, |shell, cx| {
                    if shell.session_epoch == epoch && shell.catalog_load_generation == generation {
                        shell.on_cached_catalog(cached.catalog, stale, cx);
                    }
                })
                .ok();
            }

            #[cfg(test)]
            let result = if let Some(injected) = injected {
                injected.await
            } else if cached_hit {
                loader
                    .spawn_refresh()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            } else {
                loader
                    .spawn_load()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            };
            #[cfg(not(test))]
            let result = if cached_hit {
                loader
                    .spawn_refresh()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            } else {
                loader
                    .spawn_load()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            };
            match result {
                Ok(catalog) => {
                    if let Some(cache) = cache {
                        loader.spawn_cache_save(cache, catalog.clone());
                    }
                    this.update(cx, |shell, cx| {
                        if shell.session_epoch == epoch
                            && shell.catalog_load_generation == generation
                        {
                            shell.on_catalog_loaded_for(epoch, generation, Ok(catalog), cx);
                        }
                    })
                    .ok();
                }
                Err(reason) => {
                    this.update(cx, |shell, cx| {
                        if shell.session_epoch != epoch
                            || shell.catalog_load_generation != generation
                        {
                            return;
                        }
                        if cached_hit {
                            shell.notify(
                                CATALOG_REFRESH_RECOVERY.to_owned(),
                                design::Severity::Warning,
                                Some(reason),
                                cx,
                            );
                        } else {
                            shell.on_catalog_loaded_for(epoch, generation, Err(reason), cx);
                        }
                    })
                    .ok();
                }
            }
        }));
        cx.notify();
    }

    /// Show a cached catalog while a refresh runs.
    #[cfg(not(test))]
    fn on_cached_catalog(&mut self, catalog: ResourceCatalog, stale: bool, cx: &mut Context<Self>) {
        self.on_catalog_loaded(Ok(catalog), cx);
        self.catalog_stale = Some(stale);
        cx.notify();
    }

    #[cfg(test)]
    fn on_catalog_loaded_at(
        &mut self,
        epoch: u64,
        result: Result<ResourceCatalog, String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_epoch != epoch {
            return;
        }
        self.on_catalog_loaded(result, cx);
    }

    fn on_catalog_loaded_for(
        &mut self,
        epoch: u64,
        generation: u64,
        result: Result<ResourceCatalog, String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_epoch != epoch || self.catalog_load_generation != generation {
            return;
        }
        self.on_catalog_loaded(result, cx);
    }

    fn bind_catalog_to_tabs(&mut self, catalog: &ResourceCatalog) {
        for index in 0..self.tabs.len() {
            if self.tabs[index].content != TabContent::Resource {
                continue;
            }
            let entry = self.tabs[index]
                .identity
                .as_ref()
                .and_then(|identity| catalog.by_gvk(identity))
                .cloned();
            let changed = self.tabs[index].entry != entry;
            self.tabs[index].entry = entry.clone();
            self.tabs[index].resource = entry.as_ref().map(ResourceEntry::to_api_resource);
            if changed && index != 0 {
                self.views[index] = None;
            }
        }
    }

    fn on_catalog_loaded(
        &mut self,
        result: Result<ResourceCatalog, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(catalog) => {
                let recovered = matches!(self.catalog_state, CatalogState::Failed(_))
                    || self.catalog_retry_attempt > 0
                    || self.catalog_retry_focus_tree;
                let focus_tree = self.catalog_retry_focus_tree;
                let previous_collapsed = self.collapsed.clone();
                let cursor_id = self
                    .visible_tree_rows()
                    .get(self.tree_cursor.unwrap_or(0))
                    .map(|row| row.id.clone());
                let cluster = self
                    .clusters
                    .get(self.active_cluster)
                    .cloned()
                    .unwrap_or_else(|| SharedString::from("cluster"));
                self.tree = ResourceTree::from_catalog(&catalog, &cluster);
                // A recovery keeps the tree exactly as it was, so a retry cannot move the cursor
                // out from under the reader. Otherwise the expansion is only seeded the first
                // time this cluster is seen, which is what stops the cached catalog on startup
                // and the live refresh behind it from closing the tree twice.
                let collapsed = if recovered {
                    previous_collapsed
                } else {
                    self.collapsed_for_new_catalog(&cluster)
                };
                self.collapsed = collapsed;
                self.bind_catalog_to_tabs(&catalog);
                self.search
                    .update(cx, |search, cx| search.set_catalog(&catalog, cx));
                self.catalog_state = CatalogState::Ready;
                self.catalog_failure = None;
                self.catalog_stale = None;
                self.catalog_retry_attempt = 0;
                self.catalog_retry_task = None;
                self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
                let rows = self.visible_tree_rows();
                self.tree_cursor = cursor_id
                    .as_ref()
                    .and_then(|id| rows.iter().position(|row| row.id == *id))
                    .or_else(|| (!rows.is_empty()).then_some(0));
                if let Some(cursor) = self.tree_cursor {
                    self.tree_scroll.scroll_to_item(cursor);
                }
                self.sync_tree_selection();
                self.ensure_tab_view(self.active_tab, cx);
                self.rebuild_commands();
                self.catalog_retry_focus_tree = false;
                if recovered {
                    if focus_tree {
                        self.defer_focus_tree(cx);
                    } else {
                        self.focus_active_view_pending = true;
                        self.defer_focus_active_view(cx);
                    }
                    self.toast(
                        "Resources loaded.".to_owned(),
                        design::Severity::Success,
                        cx,
                    );
                } else {
                    self.focus_active_view_pending = true;
                    self.defer_focus_active_view(cx);
                }
            }
            Err(reason) => {
                self.catalog_failure = Some(reason.clone());
                self.catalog_state = CatalogState::Failed(reason);
                self.schedule_catalog_retry(cx);
            }
        }
        cx.notify();
    }

    /// Refresh the active view and resource catalog.
    fn refresh_view(&mut self, _: &RefreshView, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_resource_view() {
            view.update(cx, |view, cx| view.refresh(cx));
        }
        // Refresh the Overview without opening another watch.
        let overview = self
            .tabs
            .get(self.active_tab)
            .is_some_and(|tab| tab.content == TabContent::Overview)
            .then(|| self.views.get(self.active_tab).and_then(Option::as_ref))
            .flatten()
            .and_then(|slot| match slot {
                TabView::Overview(view) => Some(view.clone()),
                _ => None,
            });
        if let Some(view) = overview {
            view.update(cx, |view, cx| view.refresh(cx));
        }
        self.start_namespace_load(cx);
        self.refresh_catalog(cx);
    }

    /// Refresh the catalog and rebuild the tree while preserving expansion state.
    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(loader) = self
            .catalog
            .clone()
            .or_else(|| self.session.as_ref().and_then(ClusterSession::catalog))
        else {
            return;
        };
        self.catalog = Some(loader.clone());
        self.catalog_retry_task = None;
        self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
        self.invalidate_catalog_task();
        let epoch = self.session_epoch;
        let generation = self.catalog_load_generation;
        let cache = self.cache.clone();
        #[cfg(test)]
        let injected = self.catalog_future.as_ref().map(|future| future());
        self._catalog_task = Some(cx.spawn(async move |this, cx| {
            #[cfg(test)]
            let result = if let Some(injected) = injected {
                injected.await
            } else {
                loader
                    .spawn_refresh()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            };
            #[cfg(not(test))]
            let result = loader
                .spawn_refresh()
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            this.update(cx, |shell, cx| {
                if shell.session_epoch != epoch || shell.catalog_load_generation != generation {
                    return;
                }
                match result {
                    Ok(catalog) => {
                        if let Some(cache) = cache.clone() {
                            loader.spawn_cache_save(cache, catalog.clone());
                        }
                        shell.on_catalog_refreshed(catalog, cx);
                    }
                    Err(reason) => {
                        if matches!(shell.catalog_state, CatalogState::Failed(_)) {
                            shell.on_catalog_loaded_for(epoch, generation, Err(reason), cx);
                        } else {
                            shell.notify(
                                CATALOG_REFRESH_RECOVERY.to_owned(),
                                design::Severity::Error,
                                Some(reason),
                                cx,
                            );
                        }
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_catalog_refreshed(&mut self, catalog: ResourceCatalog, cx: &mut Context<Self>) {
        let recovered = matches!(self.catalog_state, CatalogState::Failed(_));
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .cloned()
            .unwrap_or_else(|| SharedString::from("cluster"));
        self.tree = ResourceTree::from_catalog(&catalog, &cluster);
        // Name-based collapse IDs remain valid after the tree rebuilds.
        self.bind_catalog_to_tabs(&catalog);
        self.search
            .update(cx, |search, cx| search.set_catalog(&catalog, cx));
        self.catalog_state = CatalogState::Ready;
        self.catalog_failure = None;
        self.catalog_stale = None;
        self.catalog_retry_attempt = 0;
        self.catalog_retry_task = None;
        self.catalog_retry_generation = self.catalog_retry_generation.wrapping_add(1);
        self.sync_tree_selection();
        self.ensure_tab_view(self.active_tab, cx);
        self.rebuild_commands();
        if recovered {
            self.toast(
                "Resources loaded.".to_owned(),
                design::Severity::Success,
                cx,
            );
        }
        cx.notify();
    }

    /// Move focus to the next control.
    ///
    /// A modal surface keeps focus: leaving it would strand the keyboard user in the
    /// background that the modal just covered.
    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        if self.trap_modal_focus(window, cx, false) {
            return;
        }
        match self.status_panel {
            StatusPanel::Notifications => self.focus_notification_control(window, cx, false),
            StatusPanel::None => window.focus_next(cx),
        }
    }

    fn focus_previous(&mut self, _: &FocusPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if self.trap_modal_focus(window, cx, true) {
            return;
        }
        match self.status_panel {
            StatusPanel::Notifications => self.focus_notification_control(window, cx, true),
            StatusPanel::None => window.focus_prev(cx),
        }
    }

    /// Keep Tab inside the open dialog, palette, or search surface.
    ///
    /// Returns true when focus was held, so the caller does not walk the window.
    fn trap_modal_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reverse: bool,
    ) -> bool {
        if self.dialog.is_some() {
            let count = self.dialog_control_count();
            if count == 0 {
                return false;
            }
            self.dialog_focus = if reverse {
                (self.dialog_focus + count - 1) % count
            } else {
                (self.dialog_focus + 1) % count
            };
            self.focus_dialog_control(window, cx);
            cx.notify();
            return true;
        }
        if self.palette_open {
            // The palette is one input plus a list; focus stays on the input.
            let focus = self.palette_input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return true;
        }
        if self.search_open {
            let focus = self.search.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return true;
        }
        false
    }

    // Resource tree keyboard navigation

    fn move_tree_cursor(&mut self, index: usize, rows: usize, cx: &mut Context<Self>) {
        if rows == 0 {
            return;
        }
        self.tree_cursor = Some(index.min(rows - 1));
        self.tree_scroll
            .scroll_to_item(self.tree_cursor.unwrap_or(0));
        cx.notify();
    }

    /// Handle tree navigation and activation keys.
    fn on_tree_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_focus_handle.contains_focused(window, cx) {
            return;
        }
        let rows = self.visible_tree_rows();
        if rows.is_empty() {
            return;
        }
        let current = self.tree_cursor.unwrap_or(0).min(rows.len() - 1);
        match event.keystroke.key.as_str() {
            "up" => self.move_tree_cursor(current.saturating_sub(1), rows.len(), cx),
            "down" => self.move_tree_cursor((current + 1).min(rows.len() - 1), rows.len(), cx),
            "left" => {
                let row = rows[current].clone();
                if row.expandable() && row.expanded {
                    if self.tree_filter_active() {
                        if let Some(parent) =
                            rows[..current].iter().rposition(|r| r.depth < row.depth)
                        {
                            self.move_tree_cursor(parent, rows.len(), cx);
                        }
                    } else {
                        self.toggle_row(row.id, cx);
                        cx.notify();
                    }
                } else if let Some(parent) =
                    rows[..current].iter().rposition(|r| r.depth < row.depth)
                {
                    self.move_tree_cursor(parent, rows.len(), cx);
                }
            }
            "right" => {
                let row = rows[current].clone();
                if row.expandable() && !row.expanded && !self.tree_filter_active() {
                    self.toggle_row(row.id, cx);
                    cx.notify();
                } else if rows
                    .get(current + 1)
                    .is_some_and(|next| next.depth > row.depth)
                {
                    self.move_tree_cursor(current + 1, rows.len(), cx);
                }
            }
            "enter" | "space" => {
                let row = rows[current].clone();
                self.on_tree_click(row, cx);
            }
            // Home and End move to the ends of what is *on screen*, which is not the model. A
            // collapsed `core/v1` hides its kinds, and "the last row" of a tree the reader cannot
            // see the end of is a row they then have to scroll up from. The two tab strips already
            // take both keys, and three lists with three keyboard models is the finding.
            "home" => self.move_tree_cursor(0, rows.len(), cx),
            "end" => self.move_tree_cursor(rows.len() - 1, rows.len(), cx),
            "contextmenu" | "menu" => {
                self.open_tree_context_menu(current, None, window, cx);
                cx.stop_propagation();
            }
            "f10" if event.keystroke.modifiers.shift => {
                self.open_tree_context_menu(current, None, window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // Connection state

    fn reset_connection_controller(&mut self) {
        let (effects_tx, effects_rx) = unbounded_channel();
        self.connection_machine = CoreConnectionMachine::new(effects_tx).state_machine();
        self.connection_effects = effects_rx;
    }

    fn sync_connection_state(&mut self, cx: &mut Context<Self>) {
        let mapped = if self.session.is_none() {
            ConnectionState::Live
        } else {
            map_connection_state(self.connection_machine.state(), &self.startup_state)
        };
        // A machine that still says `Connecting` has learned nothing new, and the shell
        // knows something the machine does not: ten seconds have passed. Without this the
        // ten-second notice blinks off the next time any unrelated probe tick arrives, and
        // the reader is back to a silent dot.
        let next = match (&self.connection, &mapped) {
            (ConnectionState::Reconnecting(reason), ConnectionState::Connecting)
                if reason == SLOW_CONNECTION_REASON =>
            {
                ConnectionState::Reconnecting(SLOW_CONNECTION_REASON.to_owned())
            }
            _ => mapped,
        };
        if self.connection != next {
            self.connection = next;
            self.arm_slow_notice(cx);
            cx.notify();
        }
    }

    /// Restarts the ten-second clock for a connection that is still being waited on.
    ///
    /// Dropping the task is the cancel: a second call replaces the first, and the replaced
    /// one can no longer write because the generation it captured is no longer the current
    /// one. The session epoch is the outer guard, so a cluster switch cancels a notice the
    /// old cluster had already started.
    fn arm_slow_notice(&mut self, cx: &mut Context<Self>) {
        self._slow_task = None;
        if !self.connection.is_waiting() {
            return;
        }
        let generation = self.slow_generation.wrapping_add(1);
        self.slow_generation = generation;
        let epoch = self.session_epoch;
        self._slow_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLOW_CONNECTION_AFTER).await;
            this.update(cx, |shell, cx| {
                if shell.slow_generation != generation || shell.session_epoch != epoch {
                    return;
                }
                shell._slow_task = None;
                if !matches!(shell.connection, ConnectionState::Connecting) {
                    return;
                }
                shell.connection = ConnectionState::Reconnecting(SLOW_CONNECTION_REASON.to_owned());
                cx.notify();
            })
            .ok();
        }));
    }

    fn dispatch_connection_event(&mut self, event: CoreConnectionEvent, cx: &mut Context<Self>) {
        self.connection_machine.handle(&event);
        self.sync_connection_state(cx);
        while let Ok(effect) = self.connection_effects.try_recv() {
            self.run_connection_effect(effect, cx);
        }
    }

    fn run_connection_effect(&mut self, effect: CoreConnectionEffect, cx: &mut Context<Self>) {
        match effect {
            CoreConnectionEffect::CheckHealth | CoreConnectionEffect::ScheduleRetry { .. } => {}
            CoreConnectionEffect::Notify => self.sync_connection_state(cx),
        }
    }

    fn on_latency_tier(&mut self, tier: LatencyTier, cx: &mut Context<Self>) {
        if self.latency_tier == tier {
            return;
        }
        self.latency_tier = tier;
        self.sync_resource_view_pauses(cx);
    }

    fn on_health_result(&mut self, epoch: u64, health: Health, cx: &mut Context<Self>) {
        if self.session_epoch != epoch {
            return;
        }
        let event = match health {
            Health::Ready => CoreConnectionEvent::HealthOk,
            Health::NotReady(reason) => CoreConnectionEvent::HealthFail { reason },
            Health::Unknown => return,
        };
        self.dispatch_connection_event(event, cx);
    }

    fn start_health_probe(&mut self, cx: &mut Context<Self>) {
        let epoch = self.session_epoch;
        if self.health_started_epoch == Some(epoch) {
            return;
        }
        self.health_started_epoch = Some(epoch);
        self._health_task = None;
        self._health_schedule_task = None;
        self.reset_connection_controller();
        self.sync_connection_state(cx);
        // The startup state was set before the shell had a way to say time had passed, so
        // the first probe is the first moment the ten-second clock can start.
        self.arm_slow_notice(cx);
        let Some(probe) = self.health_probe.clone() else {
            return;
        };
        self.dispatch_connection_event(CoreConnectionEvent::Connect, cx);
        let mut receiver = probe.receiver;
        let mut latency = probe.latency;
        self._health_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let health = receiver.borrow_and_update().clone();
                let tier = latency.borrow_and_update().tier();
                let valid = this
                    .update(cx, |shell, cx| {
                        if shell.session_epoch != epoch {
                            return false;
                        }
                        shell.on_health_result(epoch, health, cx);
                        shell.on_latency_tier(tier, cx);
                        true
                    })
                    .unwrap_or(false);
                if !valid {
                    break;
                }
                tokio::select! {
                    result = receiver.changed() => {
                        if result.is_err() {
                            break;
                        }
                    }
                    result = latency.changed() => {
                        if result.is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }

    fn palette_matches(&self) -> Vec<&Command> {
        filter_commands_for_scope(&self.commands, &self.palette_query, self.palette_scope)
    }

    #[allow(dead_code)]
    fn filtered_command_count(&self) -> usize {
        self.palette_matches().len()
    }

    fn selected_palette_index(&self) -> Option<usize> {
        let selection = self.palette_selection.as_ref()?;
        self.palette_matches()
            .iter()
            .position(|command| &command.id == selection)
    }

    fn sync_palette_selection(&mut self) {
        let selection = self.palette_selection.clone();
        let matches = self.palette_matches();
        let selected = selection
            .as_ref()
            .and_then(|selection| matches.iter().position(|command| &command.id == selection));
        if selected.is_some() {
            return;
        }
        self.palette_selection = matches.first().map(|command| command.id.clone());
    }

    fn palette_child_index(&self) -> usize {
        let Some(selected) = self.selected_palette_index() else {
            return 0;
        };
        let matches = self.palette_matches();
        let mut child_index = 0;
        let mut group = None;
        for (index, command) in matches.iter().enumerate() {
            if group != Some(command.group.as_ref()) {
                group = Some(command.group.as_ref());
                child_index += 1;
            }
            if index == selected {
                return child_index;
            }
            child_index += 1;
        }
        0
    }

    fn reveal_palette_selection(&self) {
        self.palette_scroll
            .scroll_to_item(self.palette_child_index());
    }

    /// Route keys for the command palette.
    fn on_palette_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keep the palette shortcut toggleable while the palette is open.
        if keystroke.modifiers.secondary() && keystroke.modifiers.shift && keystroke.key == "p" {
            self.close_palette(window, cx);
            return;
        }
        match keystroke.key.as_str() {
            "enter" | "return" => {
                self.sync_palette_selection();
                if let Some(id) = self.palette_selection.clone() {
                    self.run_command(id, window, cx);
                }
            }
            "escape" => self.close_palette(window, cx),
            "tab" | "shift-tab" => {
                let input = self.palette_input.read(cx).focus_handle(cx);
                window.focus(&input, cx);
            }
            "up" | "down" => {
                self.sync_palette_selection();
                let matches = self.palette_matches();
                if matches.is_empty() {
                    return;
                }
                let count = matches.len();
                let current = self.selected_palette_index().unwrap_or(0);
                let step = if keystroke.key == "up" { count - 1 } else { 1 };
                let next = (current + step) % count;
                self.palette_selection = matches.get(next).map(|command| command.id.clone());
                self.reveal_palette_selection();
                cx.notify();
            }
            _ => {}
        }
    }

    /// Route keys for the active overlay.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            self.on_dialog_keystroke(&event.keystroke, window, cx);
            return;
        }
        if self.search_open {
            self.search.update(cx, |search, cx| {
                search.handle_keystroke(&event.keystroke, window, cx)
            });
            return;
        }
        if self.palette_open {
            self.on_palette_keystroke(&event.keystroke, window, cx);
            return;
        }
        self.on_type_ahead(&event.keystroke, window, cx);
    }

    /// Moves the sidebar's cursor to the row a typed prefix names.
    ///
    /// The shell's root is the last thing a keystroke reaches, so this runs only when nothing
    /// under it wanted the character: a field has its own context, the table has its own
    /// type-ahead, and a printable character with no modifier is a letter neither of them
    /// claims. That is the whole test — the reader has to be looking at the sidebar, not
    /// typing into it.
    ///
    /// The table is not reached from here. It answers for itself in `table_view::view`, with
    /// its own prefix and its own window, and a second implementation of the same behaviour
    /// in the shell would be two rules for a reader to learn.
    fn on_type_ahead(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_focus_handle.contains_focused(window, cx) {
            return;
        }
        if keystroke.modifiers.control
            || keystroke.modifiers.platform
            || keystroke.modifiers.alt
            || keystroke.modifiers.shift
        {
            return;
        }
        let Some(character) = keystroke.key.as_str().chars().next() else {
            return;
        };
        if !character.is_alphanumeric() && !matches!(character, '-' | '_' | '.') {
            return;
        }
        self.type_ahead
            .push(keystroke.key.as_str(), cx.background_executor().now());
        let rows = self.visible_tree_rows();
        if rows.is_empty() {
            return;
        }
        let current = self.tree_cursor.unwrap_or(0).min(rows.len() - 1);
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_ref()).collect();
        let Some(index) = type_ahead_index(&labels, self.type_ahead.prefix(), current) else {
            return;
        };
        self.move_tree_cursor(index, rows.len(), cx);
    }

    fn panel_geometry(&self, viewport_width: f32, viewport_height: f32) -> PanelGeometry {
        let (sidebar_visible, _) = self.panel_visibility(viewport_width);
        let hotbar_visible = self.hotbar_visible_for_layout(sidebar_visible);
        PanelGeometry::new(
            viewport_width,
            viewport_height,
            status_bar_height(),
            if hotbar_visible { hotbar_width() } else { 0.0 },
            self.left_width,
            self.right_width,
            self.dock_height,
        )
    }

    fn on_root_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        if !event.dragging() {
            self.drag = None;
            return;
        }
        let viewport = window.viewport_size();
        self.viewport_width = f32::from(viewport.width);
        self.viewport_height = f32::from(viewport.height);
        let (sidebar_visible, inspector_visible) = self.panel_visibility(self.viewport_width);
        let hotbar_visible = self.hotbar_visible_for_layout(sidebar_visible);
        let geometry = self.panel_geometry(self.viewport_width, self.viewport_height);
        let panel_size = geometry.dragged_panel_size(
            drag.target,
            PanelGeometry::pointer_coordinate(drag.target, event.position),
            drag.grab,
        );
        match drag.target {
            DragTarget::Left => {
                self.left_width = panel_size.clamp(
                    left_width_min(),
                    left_width_max_with_hotbar(
                        self.viewport_width,
                        self.right_width,
                        inspector_visible,
                        hotbar_visible,
                    ),
                );
            }
            DragTarget::Right => {
                self.right_width_chosen = panel_size.clamp(
                    right_width_min(),
                    right_width_max_with_hotbar(
                        self.viewport_width,
                        self.left_width,
                        sidebar_visible,
                        hotbar_visible,
                    ),
                );
            }
            DragTarget::Dock => {
                let update_strip_visible = self.update_state.shows_strip()
                    && self.update_state.phase != UpdatePhase::Unsupported;
                self.dock_height = panel_size.clamp(
                    dock_height_min(),
                    dock_height_max_with_strips(
                        self.viewport_height,
                        self.kubeconfig_warning.is_some(),
                        update_strip_visible,
                    ),
                );
            }
        }
        self.constrain_layout(self.viewport_width, self.viewport_height);
        self.layout_changed();
        cx.notify();
    }

    fn divider_focus_handle(&self, target: DragTarget) -> FocusHandle {
        match target {
            DragTarget::Left => self.left_divider_focus.clone(),
            DragTarget::Right => self.right_divider_focus.clone(),
            DragTarget::Dock => self.dock_divider_focus.clone(),
        }
    }

    fn resize_panels_with_key(
        &mut self,
        target: DragTarget,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let width = self.viewport_width;
        let height = self.viewport_height;
        let (sidebar_visible, inspector_visible) = self.panel_visibility(width);
        let hotbar_visible = self.hotbar_visible_for_layout(sidebar_visible);
        let left_max =
            left_width_max_with_hotbar(width, self.right_width, inspector_visible, hotbar_visible);
        let right_max =
            right_width_max_with_hotbar(width, self.left_width, sidebar_visible, hotbar_visible)
                .min(inspector_width_ceiling(width).unwrap_or(right_width_limit()));
        let update_strip_visible =
            self.update_state.shows_strip() && self.update_state.phase != UpdatePhase::Unsupported;
        let dock_max = dock_height_max_with_strips(
            height,
            self.kubeconfig_warning.is_some(),
            update_strip_visible,
        );

        match (target, key) {
            (DragTarget::Left, "left") => {
                self.left_width =
                    (self.left_width - DIVIDER_KEY_STEP).clamp(left_width_min(), left_max);
            }
            (DragTarget::Left, "right") => {
                self.left_width =
                    (self.left_width + DIVIDER_KEY_STEP).clamp(left_width_min(), left_max);
            }
            (DragTarget::Left, "home") => self.left_width = left_width_min(),
            (DragTarget::Left, "end") => self.left_width = left_max,
            (DragTarget::Right, "left") => {
                self.right_width_chosen = (self.right_width_chosen + DIVIDER_KEY_STEP)
                    .clamp(right_width_min(), right_max);
            }
            (DragTarget::Right, "right") => {
                self.right_width_chosen = (self.right_width_chosen - DIVIDER_KEY_STEP)
                    .clamp(right_width_min(), right_max);
            }
            (DragTarget::Right, "home") => self.right_width_chosen = right_width_min(),
            (DragTarget::Right, "end") => self.right_width_chosen = right_max,
            (DragTarget::Dock, "up") => {
                self.dock_height =
                    (self.dock_height + DIVIDER_KEY_STEP).clamp(dock_height_min(), dock_max);
            }
            (DragTarget::Dock, "down") => {
                self.dock_height =
                    (self.dock_height - DIVIDER_KEY_STEP).clamp(dock_height_min(), dock_max);
            }
            (DragTarget::Dock, "home") => self.dock_height = dock_height_min(),
            (DragTarget::Dock, "end") => self.dock_height = dock_max,
            _ => return false,
        }
        self.constrain_layout(width, height);
        self.layout_changed();
        cx.notify();
        true
    }

    /// Render the center work surface, or the empty state when nothing is open.
    fn render_center_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.open_tabs.is_empty() {
            return self.render_center(window, cx).into_any_element();
        }
        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .relative()
            .overflow_hidden()
            .debug_selector(|| "resource-center".to_owned())
            .bg(design::role::surface_app(cx).alpha(1.0))
            .child(self.render_center_empty_state(cx))
            .into_any_element()
    }

    /// State of the center after every view is closed.
    ///
    /// It names the situation and offers the two ways back in, instead of leaving a blank
    /// canvas that only the mouse can explain.
    fn render_center_empty_state(&self, cx: &Context<Self>) -> AnyElement {
        let title = shell_label(design::text::BODY, "No open view");
        let hint = shell_label(design::text::CAPTION, EMPTY_VIEW_HINT)
            .text_color(design::colors(cx).text_muted);
        let palette_chord = keymap::binding_for_context(PALETTE_ACTION, PALETTE_CONTEXT, cx);
        let palette_tooltip = empty_state_tooltip("Open the Command Palette", palette_chord);
        // Both actions run the same path as the keymap, so the button and its chord agree.
        let palette_action = div()
            .id("center-empty-palette-control")
            .debug_selector(|| "center-empty-palette".to_owned())
            .tooltip(palette_tooltip)
            .child(
                Button::new("center-empty-palette")
                    .label("Command Palette")
                    .primary()
                    .with_size(Size::Medium)
                    .tab_index(0isize)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch(ToggleCommandPalette, window, cx);
                    })),
            );
        let pods_action = div()
            .id("center-empty-pods-control")
            .debug_selector(|| "center-empty-pods".to_owned())
            .child(
                Button::new("center-empty-pods")
                    .label("Show Pods")
                    .ghost()
                    .with_size(Size::Medium)
                    .tab_index(1isize)
                    .tooltip("Open the Pods view.")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.activate_tab(0, cx);
                        cx.notify();
                    })),
            );
        // gpui-kit's `Empty` draws the icon frame, the title, and the hint, so the shell only
        // describes the situation and the two ways back into a view.
        let empty = Empty::new()
            .header(
                EmptyHeader::new()
                    .media(
                        EmptyMedia::new()
                            .with_variant(EmptyMediaVariant::Icon)
                            .child(Icon::new(IconName::BookOpen)),
                    )
                    .title(EmptyTitle::new().child(title))
                    .description(EmptyDescription::new().child(hint)),
            )
            .child(
                h_flex()
                    .id("center-empty-actions")
                    .gap(design::space::MD)
                    .child(palette_action)
                    .child(pods_action),
            );
        div()
            .id("center-empty-state")
            .debug_selector(|| "center-empty-state".to_owned())
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .items_center()
            .justify_center()
            .px(design::space::XL)
            .role(Role::Region)
            .aria_label("No open view")
            .aria_description(EMPTY_VIEW_HINT)
            .child(empty)
            .into_any_element()
    }

    /// Render a resizable divider with a visible focus state.
    fn render_divider(
        &self,
        target: DragTarget,
        cursor: CursorStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The splitter's line and its rail are product-theme roles, not gpui-kit ones: the
        // design tokens decide how much a resting hairline differs from a focused rail.
        let colors = design::colors(cx);
        let id = match target {
            DragTarget::Left => "divider-left",
            DragTarget::Right => "divider-right",
            DragTarget::Dock => "divider-dock",
        };
        let vertical = target != DragTarget::Dock;
        let active = self.drag.is_some_and(|drag| drag.target == target);
        let focus = self.divider_focus_handle(target);
        let focused = focus.is_focused(window);
        let geometry = self.panel_geometry(
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        );
        let state = match (active, focused) {
            (true, _) => DividerPaint::Drag,
            (false, true) => DividerPaint::Focus,
            (false, false) => DividerPaint::Rest,
        };
        let line = if state == DividerPaint::Rest {
            colors.border
        } else {
            colors.border_focused
        };
        let highlight = colors.border_focused;
        // The visible line and the 20px hit area stay separate. At rest the splitter is the
        // 1px structural line, and hover, drag, and keyboard focus each widen that same line
        // into an accent rail, so a bare hairline is never the only drag affordance. The four
        // states are four pairs: focus and hover used to be the same pair, so three states were
        // delivered out of the four the state matrix asks for.
        let rail = state.rail();
        let wash = state.wash(highlight);
        // Hover is a paint-time state, so the rail width is a group style on the line.
        let hover = DividerPaint::Hover;
        let (label, value) = match target {
            DragTarget::Left => ("Resize Sidebar", self.left_width),
            DragTarget::Right => ("Resize Inspector", self.right_width),
            DragTarget::Dock => ("Resize Dock", self.dock_height),
        };
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            // The line is a child, so the splitter owns the hover group it refines.
            .group(id)
            .role(Role::Splitter)
            .aria_label(label)
            .aria_numeric_value(f64::from(value))
            .track_focus(&focus)
            .tab_index(0isize)
            .focus_visible(move |this| this.bg(hover.wash(highlight)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if this.resize_panels_with_key(target, &event.keystroke.key, cx) {
                    cx.stop_propagation();
                }
            }))
            .flex_none()
            .cursor(cursor)
            .flex()
            .justify_center()
            .items_center()
            .bg(wash)
            .hover(move |this| this.bg(hover.wash(highlight)))
            .active(move |this| this.bg(DividerPaint::Drag.wash(highlight)))
            .when(vertical, |this| {
                this.w(design::border::HIT).h_full().child(
                    div()
                        .w(rail)
                        .h_full()
                        .bg(line)
                        .group_hover(id, |s| s.w(hover.rail()).bg(highlight)),
                )
            })
            .when(!vertical, |this| {
                this.h(design::border::HIT).w_full().child(
                    div()
                        .h(rail)
                        .w_full()
                        .bg(line)
                        .group_hover(id, |s| s.h(hover.rail()).bg(highlight)),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let grab = PanelGeometry::pointer_coordinate(target, event.position)
                        - geometry.edge(target);
                    window.focus(&focus, cx);
                    this.drag = Some(DividerDrag { target, grab });
                    cx.notify();
                }),
            )
    }

    /// Dispatch panel actions from pointer and keyboard input.
    fn dispatch(&self, action: impl Action, window: &mut Window, cx: &mut Context<Self>) {
        let action: Box<dyn Action> = Box::new(action);
        window.defer(cx, move |window, cx| {
            window.dispatch_action(action, cx);
        });
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            self.start_health_probe(cx);
            self.refresh_capability_probes(cx);
            // Demo mode has no session and keeps its populated tree.
            if self.session.is_some() {
                self.start_catalog_load(cx);
            }
            // Report the initial keymap status before the observer receives changes.
            let status = keymap::status(cx);
            if status.has_issues()
                && let Some(message) = status.toast_message()
            {
                self.toast(message, status.toast_severity(), cx);
            }
            // Report a Hotbar load failure after startup.
            if let Some(reason) = self.hotbar_load_error.take() {
                self.notify(
                    "The Hotbar failed to load. Check the Hotbar file, then try again.".to_owned(),
                    design::Severity::Warning,
                    Some(reason),
                    cx,
                );
            }
            if window.focused(cx).is_none() {
                window.focus(&self.focus_handle, cx);
            }
            // Restore shell focus if the active content unmounts.
            self._focus_lost = Some(cx.on_focus_lost(window, |this, window, cx| {
                if this.focus_active_view_pending && this.focus_active_view(window, cx) {
                    return;
                }
                this.focus_active_view_pending = false;
                window.focus(&this.focus_handle, cx);
            }));

            // Intercept modal keys before actions reach the parent view.
            let shell = cx.weak_entity();
            self._palette_interceptor = Some(cx.intercept_keystrokes(move |event, window, cx| {
                let Some(shell) = shell.upgrade() else {
                    return;
                };
                let (
                    palette_open,
                    search_open,
                    dialog_open,
                    status_panel,
                    palette_empty,
                    palette_input,
                    search,
                ) = {
                    let shell = shell.read(cx);
                    (
                        shell.palette_open,
                        shell.search_open,
                        shell.dialog.is_some(),
                        shell.status_panel,
                        shell.palette_query.is_empty(),
                        shell.palette_input.clone(),
                        shell.search.clone(),
                    )
                };
                if !palette_open
                    && !search_open
                    && !dialog_open
                    && status_panel == StatusPanel::None
                {
                    return;
                }

                let keystroke = &event.keystroke;
                if matches!(
                    keystroke.key.as_str(),
                    "shift" | "control" | "alt" | "platform" | "function"
                ) {
                    return;
                }
                let text_input_focused = {
                    let shell = shell.read(cx);
                    if palette_open {
                        shell
                            .palette_input
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window)
                    } else if search_open {
                        search.read(cx).focus_handle(cx).is_focused(window)
                    } else if dialog_open {
                        shell
                            .dialog_input()
                            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
                    } else {
                        false
                    }
                };
                let search_prefix = !search_open
                    && !dialog_open
                    && status_panel == StatusPanel::None
                    && palette_empty
                    && !keystroke.modifiers.secondary()
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt
                    && !keystroke.modifiers.platform
                    && keystroke.key_char.as_deref() == Some(">");
                if text_input_focused
                    && (is_text_entry_keystroke(keystroke) || is_text_edit_shortcut(keystroke))
                    && !search_prefix
                {
                    return;
                }
                match status_panel {
                    StatusPanel::Notifications => {
                        let handled = match keystroke.key.as_str() {
                            "escape" => {
                                shell.update(cx, |shell, cx| {
                                    shell.close_notifications(window, cx);
                                });
                                true
                            }
                            "tab" => {
                                shell.update(cx, |shell, cx| {
                                    shell.focus_notification_control(
                                        window,
                                        cx,
                                        keystroke.modifiers.shift,
                                    );
                                });
                                true
                            }
                            "enter" | "return" | "space" => {
                                shell.update(cx, |shell, cx| {
                                    shell.activate_notification_control(window, cx);
                                });
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            cx.stop_propagation();
                        }
                        return;
                    }
                    StatusPanel::None => {}
                }
                if dialog_open {
                    shell.update(cx, |shell, cx| {
                        shell.on_dialog_keystroke(keystroke, window, cx);
                    });
                } else if search_open {
                    let handled = matches!(
                        keystroke.key.as_str(),
                        "tab"
                            | "escape"
                            | "up"
                            | "down"
                            | "home"
                            | "end"
                            | "pageup"
                            | "pagedown"
                            | "enter"
                            | "return"
                            | "space"
                    );
                    if handled {
                        search.update(cx, |search, cx| {
                            search.handle_keystroke(keystroke, window, cx)
                        });
                        cx.stop_propagation();
                    }
                } else {
                    let palette_key = matches!(
                        keystroke.key.as_str(),
                        "enter" | "return" | "escape" | "up" | "down" | "tab" | "shift-tab"
                    ) || (keystroke.modifiers.secondary()
                        && keystroke.modifiers.shift
                        && keystroke.key.eq_ignore_ascii_case("p"));
                    if search_prefix {
                        let mut opened = false;
                        shell.update(cx, |shell, cx| {
                            opened = shell.open_search(window, cx);
                        });
                        if !opened && is_text_entry_keystroke(keystroke) {
                            return;
                        }
                        if opened {
                            cx.stop_propagation();
                        }
                    } else if palette_key {
                        shell.update(cx, |shell, cx| {
                            shell.on_palette_keystroke(keystroke, window, cx);
                        });
                        cx.stop_propagation();
                    } else if is_text_entry_keystroke(keystroke) {
                        palette_input.update(cx, |input, cx| {
                            input.handle_keystroke(keystroke, window, cx);
                        });
                        cx.stop_propagation();
                    } else {
                        return;
                    }
                }
                if dialog_open {
                    cx.stop_propagation();
                }
            }));
        }
        // The window title follows the visible pane, so it is synced before the title is built.
        self.sync_settings_tab_title(cx);
        let cluster = self
            .clusters
            .get(self.active_cluster)
            .map_or("No Context", |name| name.as_ref());
        let title = format!("{} — {cluster}", self.active_view_title());
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let viewport = window.viewport_size();
        self.viewport_width = f32::from(viewport.width);
        self.viewport_height = f32::from(viewport.height);
        self.constrain_layout(self.viewport_width, self.viewport_height);
        self.record_window_geometry(window, cx);
        self.flush_layout(cx);
        let (sidebar_visible, inspector_layout) = self.panel_layout(self.viewport_width);
        let inspector_visible = inspector_layout != InspectorLayout::Closed;
        // Update metrics sampling after Inspector visibility changes.
        if inspector_visible != self.inspector_rendered {
            self.inspector_rendered = inspector_visible;
            let inspector = self.inspector.clone();
            window.defer(cx, move |_, cx| {
                inspector.update(cx, |panel, cx| {
                    panel.set_metrics_visible(inspector_visible, cx)
                });
            });
        }
        let colors = design::colors(cx);
        div()
            .id("shell-root")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(design::role::surface_app(cx).alpha(1.0))
            .text_color(colors.text)
            .track_focus(&self.focus_handle)
            .key_context("Shell")
            .on_key_down(cx.listener(Self::on_key_down))
            // `OpenSettings` is deliberately NOT handled here. `UI-SPEC` §9.3 wants
            // Settings in its own window, and the app-level handler opens it. Every
            // entry point — the gear, the macOS menu, the palette, `secondary-,` —
            // dispatches the same action, so one handler serves all four; handling
            // it here as well would open a window and a centre tab at the same time.
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::open_context_switcher))
            .on_action(cx.listener(Self::open_namespace_switcher))
            .on_action(cx.listener(Self::open_resource_kind_switcher))
            .on_action(cx.listener(Self::open_overview))
            .on_action(cx.listener(Self::open_forwards))
            .on_action(cx.listener(Self::apply_yaml))
            .on_action(cx.listener(Self::open_logs_action))
            .on_action(cx.listener(Self::open_events_action))
            .on_action(cx.listener(Self::exec_selection))
            .on_action(cx.listener(Self::port_forward_selection))
            .on_action(cx.listener(Self::restart_selection))
            .on_action(cx.listener(Self::scale_selection))
            .on_action(cx.listener(Self::reload_keymap))
            .on_action(cx.listener(Self::use_keymap_preset))
            .on_action(cx.listener(Self::toggle_left_panel))
            .on_action(cx.listener(Self::toggle_right_panel))
            .on_action(cx.listener(Self::toggle_dock))
            .on_action(cx.listener(Self::toggle_notifications))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::toggle_pin_tab))
            .on_action(cx.listener(Self::move_tab_left))
            .on_action(cx.listener(Self::move_tab_right))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::switch_tab))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .on_action(cx.listener(Self::describe_selection))
            .on_action(cx.listener(Self::open_service_account))
            .on_action(cx.listener(Self::focus_yaml))
            .on_action(cx.listener(Self::focus_yaml_compat))
            .on_action(cx.listener(Self::pause_updates))
            .on_action(cx.listener(Self::resume_updates))
            .on_action(cx.listener(Self::copy_selected_pod_name))
            .on_action(cx.listener(Self::toggle_hotbar))
            .on_action(cx.listener(Self::refresh_view))
            .on_action(cx.listener(Self::check_for_updates))
            .on_action(cx.listener(Self::restart_to_update))
            .on_action(cx.listener(Self::on_hotbar_switch_cluster))
            .on_action(cx.listener(Self::on_hotbar_switch_bank))
            .on_action(cx.listener(Self::search_resources))
            .on_action(cx.listener(Self::reload_kubeconfigs))
            .on_mouse_move(cx.listener(Self::on_root_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.drag = None;
                    this.clear_center_tab_drag(cx);
                }),
            )
            .child(self.render_top_bar(window, cx))
            .when_some(self.render_kubeconfig_warning(), |this, warning| {
                this.child(warning)
            })
            .when_some(self.render_update_strip(cx), |this, strip| {
                this.child(strip)
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    // The floating Inspector is a child of this row and not of the shell, so
                    // it is bounded by the row rather than by the window: it must not reach
                    // over the title bar or under the status bar to become an overlay.
                    .relative()
                    .when(sidebar_visible, |this| {
                        this.when(self.hotbar_open, |this| {
                            this.child(self.render_hotbar_rail(window, cx))
                        })
                        .child(self.render_tree_with_filter(window, cx))
                        .child(self.render_divider(
                            DragTarget::Left,
                            CursorStyle::ResizeLeftRight,
                            window,
                            cx,
                        ))
                    })
                    .child(self.render_center_workspace(window, cx))
                    .when(inspector_layout == InspectorLayout::Docked, |this| {
                        this.child(self.render_divider(
                            DragTarget::Right,
                            CursorStyle::ResizeLeftRight,
                            window,
                            cx,
                        ))
                        .child(self.render_inspector(cx))
                    })
                    // Floating: the centre keeps every pixel it had, so the table does not
                    // lose two of its seven columns to a panel the reader only wants
                    // sometimes, and the selection still previews behind the panel.
                    .when(inspector_layout == InspectorLayout::Floating, |this| {
                        this.child(
                            div()
                                .id("inspector-float")
                                .debug_selector(|| "inspector-float".to_owned())
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right_0()
                                .w(px(self.right_width))
                                .min_w(px(right_width_min()))
                                .child(self.render_inspector(cx)),
                        )
                    }),
            )
            .when(self.dock_open, |this| {
                this.child(self.render_divider(
                    DragTarget::Dock,
                    CursorStyle::ResizeUpDown,
                    window,
                    cx,
                ))
            })
            .child(self.render_dock(window, cx))
            .child(self.render_status_bar(window, cx))
            .when(self.status_panel == StatusPanel::Notifications, |this| {
                this.child(self.render_notification_backdrop(cx))
            })
            .when(self.status_panel == StatusPanel::Notifications, |this| {
                this.child(self.render_notification_center(window, cx))
            })
            // Transient feedback sits above the popovers so its dismiss button keeps
            // working while a status panel is open.
            .when_some(self.toast.clone(), |this, toast| {
                this.child(self.render_toast(&toast, cx))
            })
            // The update popover is anchored to the toolbar, so it has to paint above the
            // status backdrop that would otherwise swallow its clicks.
            .when_some(self.render_update_overlay(window, cx), |this, overlay| {
                this.child(overlay)
            })
            .when(self.palette_open, |this| {
                this.child(self.render_command_palette(cx))
            })
            // The resource search is a modal like the palette and the dialog, so it is mounted
            // here and not inside the centre column. Its own `size_full()` then resolves to the
            // window, and `modality.md` gets what it asks for: a modal obscures the context it
            // came from. The centre still measures its own bounds for the card, and the search is
            // reachable with every tab closed, which is why it does not need a view to hang off.
            .when(self.search_open, |this| this.child(self.search.clone()))
            .when_some(self.render_tab_context_menu(cx), |this, menu| {
                this.child(menu)
            })
            .when_some(self.render_tree_context_menu(cx), |this, menu| {
                this.child(menu)
            })
            .when(self.dialog.is_some(), |this| {
                this.child(self.render_dialog(window, cx))
            })
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use gpui_kit::{Modifiers, TestAppContext};

    fn row(name: &str, uid: &str) -> Row {
        Row {
            obj: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": name,
                        "namespace": "default",
                        "uid": uid,
                    },
                }))
                .expect("test pod"),
            ),
            cells: Vec::new(),
        }
    }

    /// Opening a row's details is an action, and it is reachable three ways:
    /// Enter, the row menu, and the kind switcher. None of them is a click.
    #[gpui_kit::test]
    fn opening_row_details_pins_one_tab_per_resource(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let spec = ResourceSpec::pods();
        let before = shell.read_with(cx, |shell, _| shell.tabs.len());
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("first", "uid-first"), spec.clone(), 0, cx);
        });
        let after_first = shell.read_with(cx, |shell, _| shell.tabs.len());
        assert_eq!(after_first, before + 1, "Open Details opens the preview");
        // The same resource again reuses its tab rather than stacking a second.
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("first", "uid-first"), spec.clone(), 0, cx);
        });
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.tabs.len()),
            after_first
        );
        // A different resource gets its own tab.
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("second", "uid-second"), spec, 0, cx);
        });
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.tabs.len()),
            after_first + 1
        );
    }

    /// Opening the same resource twice pins one tab, not two, and a second
    /// resource gets its own.
    #[gpui_kit::test]
    fn opening_the_same_resource_twice_pins_one_tab(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let spec = ResourceSpec::pods();
        let resource = row("pinned", "uid-pinned");
        let identity = PreviewIdentity::from_row(&spec, &resource).expect("identity");
        shell.update(cx, |shell, cx| {
            shell.open_row_details(resource.clone(), spec.clone(), 0, cx);
            shell.open_row_details(resource.clone(), spec.clone(), 0, cx);
            shell.open_row_details(resource, spec, 0, cx);
        });
        assert_eq!(shell.read_with(cx, |shell, _| shell.tabs.len()), 3);
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.tabs.iter().filter(|tab| {
                matches!(tab.preview.as_ref(), Some(PreviewState::Fixed(existing)) if existing == &identity)
            }).count()),
            1
        );
    }

    /// `Open Details` fixes a tab to that resource. It adopts the following
    /// preview if one is open rather than leaving a stale tab behind, and
    /// either way the tab stops tracking later selections.
    #[gpui_kit::test]
    fn opening_details_fixes_the_tab_and_it_stops_following(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let spec = ResourceSpec::pods();
        shell.update(cx, |shell, cx| {
            shell.open_preview(&row("followed", "uid-followed"), &spec, None, cx);
        });
        let preview = shell.read_with(cx, |shell, _| shell.active_tab);

        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("pinned", "uid-pinned"), spec.clone(), 0, cx);
        });
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.following_preview_index()),
            None,
            "the following preview was adopted, not left stale"
        );
        let pinned = shell.read_with(cx, |shell, _| shell.active_tab);
        assert_eq!(pinned, preview, "the adopted tab is reused");

        shell.update(cx, |shell, cx| {
            shell.on_row_selection(Some(row("next", "uid-next")), spec, 0, cx);
        });
        assert_eq!(
            shell.read_with(cx, |shell, cx| {
                shell.views[pinned].as_ref().and_then(|view| match view {
                    TabView::Preview(view) => view
                        .read(cx)
                        .selection()
                        .map(|selection| selection.name.clone()),
                    _ => None,
                })
            }),
            Some("pinned".to_owned()),
            "a fixed tab keeps its own resource"
        );
    }

    #[gpui_kit::test]
    fn closing_preview_falls_back_to_the_adjacent_tab(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("closing", "uid-closing"), ResourceSpec::pods(), 0, cx);
        });
        let preview = shell.read_with(cx, |shell, _| shell.active_tab);
        shell.update(cx, |shell, cx| shell.activate_tab(0, cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.close_tab_index(preview, window, cx));
        });
        assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 0);
        assert!(!shell.read_with(cx, |shell, _| shell.open_tabs.contains(&preview)));
        assert!(shell.read_with(cx, |shell, _| shell.views[preview].is_none()));
    }

    /// Opening details from the row must not re-enter the shell while it is
    /// already updating, and a plain click must not open anything at all.
    #[gpui_kit::test]
    fn opening_row_details_does_not_reenter_shell(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(30));
        cx.run_until_parked();
        let before = shell.read_with(cx, |shell, _| shell.tabs.len());
        let row = cx.debug_bounds("resource-row-0").expect("first row");
        cx.simulate_click(row.center(), Modifiers::none());
        cx.simulate_click(row.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.tabs.len()),
            before,
            "clicking a row opened a tab"
        );
    }

    #[gpui_kit::test]
    fn preview_keeps_session_identity_and_apply_target(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("apply", "uid-apply"), ResourceSpec::pods(), 0, cx);
        });
        let preview = shell.read_with(cx, |shell, _| shell.active_tab);
        let panel = shell.read_with(cx, |shell, _| {
            shell.views[preview]
                .as_ref()
                .and_then(|view| match view {
                    TabView::Preview(view) => Some(view.clone()),
                    _ => None,
                })
                .expect("preview view")
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.session_identity(), InspectorSession::default());
            assert!(panel.has_apply_handler());
            assert_eq!(panel.apply_target().unwrap().uid, "uid-apply");
        });
    }

    /// The global inspector mirrors whichever preview tab is active, and a
    /// pinned tab keeps its own resource across tab switches.
    #[gpui_kit::test]
    fn the_global_inspector_mirrors_the_active_preview_tab(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let spec = ResourceSpec::pods();
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("first", "uid-first"), spec.clone(), 0, cx);
        });
        let first = shell.read_with(cx, |shell, _| shell.active_tab);
        let first_panel = shell.read_with(cx, |shell, _| {
            shell.views[first]
                .as_ref()
                .and_then(|view| match view {
                    TabView::Preview(view) => Some(view.clone()),
                    _ => None,
                })
                .expect("preview view")
        });
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("second", "uid-second"), spec.clone(), 0, cx);
        });
        let second = shell.read_with(cx, |shell, _| shell.active_tab);
        assert_ne!(first, second);

        assert_eq!(
            shell.read_with(cx, |shell, cx| {
                shell.inspector.read(cx).selection().map(|s| s.uid.clone())
            }),
            Some("uid-second".to_owned()),
            "the inspector follows the active tab"
        );
        assert_eq!(
            first_panel.read_with(cx, |panel, _| panel.selection().map(|s| s.uid.clone())),
            Some("uid-first".to_owned()),
            "the other preview keeps its own resource"
        );
    }

    #[gpui_kit::test]
    fn preview_focus_actions_target_the_preview_inspector(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.open_row_details(row("focus", "uid-focus"), ResourceSpec::pods(), 0, cx);
        });
        let preview = shell.read_with(cx, |shell, _| shell.active_tab);
        let panel = shell.read_with(cx, |shell, _| {
            shell.views[preview]
                .as_ref()
                .and_then(|view| match view {
                    TabView::Preview(view) => Some(view.clone()),
                    _ => None,
                })
                .expect("preview view")
        });
        let content_focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                assert!(shell.focus_active_view(window, cx));
            });
        });
        assert!(cx.update(|window, _| content_focus.is_focused(window)));
        let toast_before = shell.read_with(cx, |shell, _| shell.toast.is_some());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.focus_yaml_content(window, cx));
        });
        // The chord lands on the document and says nothing. Both halves matter:
        // switching the tab is the feature, and the toast is the regression —
        // selecting a row no longer loads the document eagerly (`UI-REDESIGN` D27
        // makes YAML the fallback rather than the landing view), so a chord that
        // asked "have you got YAML yet?" answered "no" on a perfectly good row and
        // told the reader to select one they had already selected.
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            InspectorTab::Yaml,
            "`\u{2318}\u{21e7}Y` puts the caret in the document"
        );
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.toast.is_some()),
            toast_before,
            "the chord reached a selected row, so it has nothing to report"
        );
        // The caret itself is not asserted: the editor's element only enters the
        // tree when the panel paints, and this harness does not paint between the
        // chord and the check. The focus request is made (a deferred one, for that
        // reason) and the tab is switched, which is everything this harness can see.
    }
}

#[cfg(test)]
mod search_tests {
    use gpui_kit::{Keystroke, TestAppContext};

    use super::*;

    fn hit(name: &str) -> SearchHit {
        SearchHit {
            resource: ResourceEntry {
                group: String::new(),
                version: "v1".to_owned(),
                kind: "Pod".to_owned(),
                plural: "pods".to_owned(),
                scope: k8s_core::discovery::ResourceScope::Namespaced,
                verbs: vec!["list".to_owned()],
            },
            object: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": name,
                        "namespace": "default",
                        "uid": format!("uid-{name}"),
                    },
                }))
                .expect("valid pod"),
            ),
        }
    }

    #[gpui_kit::test]
    fn search_resources_opens_and_escape_restores_table_focus(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.run_until_parked();
        let table = shell.read_with(cx, |shell, cx| shell.pods.read(cx).table_focus_handle(cx));
        cx.update(|window, cx| window.focus(&table, cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.command_search_resources(window, cx));
        });
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.search_open));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!shell.read_with(cx, |shell, _| shell.search_open));
        assert!(cx.update(|window, _| table.is_focused(window)));
    }

    #[gpui_kit::test]
    fn search_prefix_switches_command_palette_to_resource_search(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| keymap::install_target_default(cx).expect("keymap"));
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let table = shell.read_with(cx, |shell, cx| shell.pods.read(cx).table_focus_handle(cx));
        let palette_input = shell.read_with(cx, |shell, cx| {
            shell.palette_input.read(cx).focus_handle(cx)
        });
        cx.update(|window, cx| window.focus(&table, cx));
        cx.simulate_keystrokes("secondary-shift-p");
        cx.simulate_keystrokes(">");
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.search_open));
        assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.update(|window, _| table.is_focused(window)));
        assert!(!cx.update(|window, _| palette_input.is_focused(window)));
    }

    #[gpui_kit::test]
    fn search_opens_from_a_non_resource_tab(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            assert!(shell.open_special_tab(TabContent::Overview, "Overview", IconName::Server, cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.command_search_resources(window, cx));
        });
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.search_open));
    }

    #[gpui_kit::test]
    fn search_result_uses_existing_preview_open_path(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.open_search(window, cx));
        });
        let hit = hit("searched");
        shell.update(cx, |shell, cx| {
            shell.search.update(cx, |search, _| {
                search.inject_results_for_test(vec![hit]);
            });
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.search.update(cx, |search, cx| {
                    search.handle_keystroke(
                        &Keystroke {
                            modifiers: Default::default(),
                            key: "enter".into(),
                            key_char: None,
                        },
                        window,
                        cx,
                    )
                });
            });
        });
        cx.run_until_parked();
        assert!(!shell.read_with(cx, |shell, _| shell.search_open));
        assert!(shell.read_with(cx, |shell, _| {
            shell
                .tabs
                .iter()
                .any(|tab| matches!(tab.content, TabContent::Preview))
        }));
        let preview_focus = shell.read_with(cx, |shell, cx| {
            shell
                .active_preview()
                .expect("preview view")
                .read(cx)
                .focus_handle()
        });
        assert!(cx.update(|window, _| preview_focus.is_focused(window)));
        assert!(shell.read_with(cx, |shell, _| {
            shell.tabs.iter().any(|tab| {
                matches!(tab.content, TabContent::Preview)
                    && matches!(tab.preview, Some(PreviewState::Fixed(_)))
            })
        }));
    }
}

#[cfg(test)]
mod empty_center_tests {
    use gpui_kit::TestAppContext;

    use super::*;

    /// Close All leaves the shell on an explicit empty surface, not on a substitute tab.
    #[gpui_kit::test]
    fn closing_every_tab_leaves_an_empty_center(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.close_all_center_tabs(window, cx));
        });
        cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
        cx.run_until_parked();

        assert!(shell.read_with(cx, |shell, _| shell.open_tabs.is_empty()));
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.active_view_title().to_string()),
            "No Open View"
        );
        assert!(shell.read_with(cx, |shell, _| {
            !shell
                .tabs
                .iter()
                .any(|tab| matches!(tab.content, TabContent::Overview))
        }));
        assert!(cx.debug_bounds("center-tab-panel").is_none());
        let empty = cx
            .debug_bounds("center-empty-state")
            .expect("the empty center surface");
        assert!(empty.size.width > px(0.0));
        assert!(empty.size.height > px(0.0));
    }

    /// The empty surface keeps a direct way back into a view.
    #[gpui_kit::test]
    fn empty_center_keeps_a_way_back_into_a_view(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.close_all_center_tabs(window, cx));
        });
        cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
        cx.run_until_parked();
        let pods = cx
            .debug_bounds("center-empty-pods")
            .expect("the empty state's action");
        cx.simulate_click(pods.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
            vec![0]
        );
    }

    /// The empty surface does not trap the keyboard: a tab chord still opens a view.
    #[gpui_kit::test]
    fn empty_center_keeps_the_tab_chords_working(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| keymap::install_target_default(cx).expect("keymap"));
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_keystrokes("secondary-shift-w");
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.open_tabs.is_empty()));

        // `secondary-2` is SwitchTab(1), the Deployments tab.
        cx.simulate_keystrokes("secondary-2");
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
            vec![1]
        );
        assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 1);
    }
}

#[cfg(test)]
mod dialog_focus_tests {
    use gpui_kit::TestAppContext;

    use super::*;

    /// Open the two-button alert whose trailing button removes a Hotbar bank for good.
    fn open_remove_bank(cx: &mut gpui_kit::VisualTestContext, shell: &gpui_kit::Entity<Shell>) {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.dialog = Some(Dialog::HotbarRemove {
                    index: 0,
                    name: "work".into(),
                });
                shell.open_dialog_focus(window, cx);
            });
        });
        cx.run_until_parked();
    }

    fn focus_index(cx: &gpui_kit::VisualTestContext, shell: &gpui_kit::Entity<Shell>) -> usize {
        shell.read_with(cx, |shell, _| shell.dialog_focus)
    }
    /// A backwards arrow has nowhere to go from the leading button, so the default holds.
    /// A forwards arrow reaches the action and then stops, instead of wrapping back to Cancel.
    /// A stray arrow in a destructive alert cannot answer with the destructive button.
    ///
    /// The alert opens on Cancel, the leading button of the row, so Up and Left have no earlier
    /// control to move to. Wrapping would have put the destructive button under the Return key.
    #[gpui_kit::test]
    fn an_arrow_keeps_a_destructive_dialog_on_cancel(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
        open_remove_bank(cx, &shell);

        assert_eq!(focus_index(cx, &shell), 0, "Cancel is the default");
        let cancel = shell.read_with(cx, |shell, _| shell.dialog_button_focus(0));
        assert!(cx.update(|window, _| cancel.is_focused(window)));

        for key in ["up", "left", "up", "left"] {
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            assert_eq!(
                focus_index(cx, &shell),
                0,
                "{key} moved focus off the Cancel default"
            );
            assert!(
                cx.update(|window, _| cancel.is_focused(window)),
                "{key} moved focus onto the destructive button"
            );
            assert!(shell.read_with(cx, |shell, _| shell.dialog.is_some()));
        }

        // So Return still cancels instead of removing the bank.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    }
}

#[cfg(test)]
mod dock_focus_tests {
    use gpui_kit::TestAppContext;

    use super::*;

    fn log_request() -> LogRequest {
        LogRequest {
            namespace: Some("default".into()),
            name: "web-0".into(),
            containers: vec!["app".into()],
        }
    }

    /// Open the Dock for a log target and start the stream.
    fn open_logs(cx: &mut gpui_kit::VisualTestContext, shell: &gpui_kit::Entity<Shell>) {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_logs(log_request(), window, cx);
            });
        });
        cx.run_until_parked();
    }

    /// The focus target before the Dock opens, so the test can tell the Dock took it.
    fn shell_focus(
        cx: &gpui_kit::VisualTestContext,
        shell: &gpui_kit::Entity<Shell>,
    ) -> FocusHandle {
        shell.read_with(cx, |shell, _| shell.focus_handle.clone())
    }

    /// Focuses the shell itself, which is where the focus sits before a panel opens.
    fn focus_shell(
        cx: &mut gpui_kit::VisualTestContext,
        shell: &gpui_kit::Entity<Shell>,
    ) -> FocusHandle {
        let focus = shell_focus(cx, shell);
        cx.update(|window, cx| window.focus(&focus, cx));
        focus
    }

    /// Opening logs has to put the keyboard in the Dock. A Dock that opened while the resource
    /// table kept the focus reads as a shortcut that did nothing: the arrows still move the table
    /// behind it, and the lines arrive where the user is not looking.
    #[gpui_kit::test]
    fn open_logs_moves_the_focus_into_the_dock(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        focus_shell(cx, &shell);

        open_logs(cx, &shell);

        let dock_focus = shell.read_with(cx, |shell, cx| shell.dock_panel.read(cx).focus_handle());
        assert!(
            cx.update(|window, _| dock_focus.is_focused(window)),
            "the log list holds the keyboard after the Dock opens"
        );
    }

    /// Closing the Dock returns the focus where it came from, so taking the focus for the Dock
    /// cannot strand it: the shell focus the user left is still recorded.
    #[gpui_kit::test]
    fn closing_the_dock_returns_the_focus_it_took(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let before = focus_shell(cx, &shell);

        open_logs(cx, &shell);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
        });
        cx.run_until_parked();

        assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
        assert!(
            cx.update(|window, _| before.is_focused(window)),
            "the focus returns to where the Dock found it"
        );
    }

    /// A terminal that could not start still took the Dock, so the Dock keeps the keyboard.
    /// Otherwise the arrows go on driving the resource table behind a Dock the user just opened.
    ///
    /// The services are present and the factory refuses, so the Dock has a control of its own to
    /// fall back to. The log list is still the better target: it is the surface the user asked
    /// for, and it is the one that keeps the navigation keys off the table.
    #[gpui_kit::test]
    fn a_failed_terminal_leaves_the_focus_in_the_dock(cx: &mut TestAppContext) {
        use crate::panels::terminal::{PortForwardFactory, TerminalFactory, TerminalServices};

        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        let terminals: TerminalFactory = Rc::new(|_request, _sink, _cx| {
            Err("Exec is not available in this test build.".to_owned())
        });
        let forwards: PortForwardFactory = Rc::new(|_request, _cx| {
            Err("Port forwarding is not available in this test build.".to_owned())
        });
        shell.update(cx, |shell, cx| {
            shell.set_terminal_services(
                Some(TerminalServices {
                    terminals,
                    forwards,
                    context: Some("kind-k8s-gpui-dev".to_owned()),
                    namespace: None,
                }),
                cx,
            );
        });
        focus_shell(cx, &shell);

        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.start_exec(
                    ExecTarget {
                        namespace: Some("default".to_owned()),
                        name: "web-0".into(),
                        containers: vec!["app".into()],
                    },
                    Some("app".into()),
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();

        assert!(
            shell.read_with(cx, |shell, _| shell.dock_open),
            "the Dock opened for the terminal"
        );
        let dock_focus = shell.read_with(cx, |shell, cx| shell.dock_panel.read(cx).focus_handle());
        assert!(
            cx.update(|window, _| dock_focus.is_focused(window)),
            "a terminal that could not start must not hand the keyboard back to the table"
        );
    }
}

#[cfg(test)]
mod command_row_copy_tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use k8s_core::hotbar::{Bank, Slot};

    fn labels(commands: &[Command]) -> Vec<String> {
        commands
            .iter()
            .map(|command| command.label.to_string())
            .collect()
    }

    /// Every row of every switcher is a noun. A verb and a subject in front of
    /// each row pushed the value a reader is scanning for to the right, and made
    /// a query built from the shared words match the whole list.
    #[gpui_kit::test]
    fn every_switcher_row_is_a_noun(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.run_until_parked();

        let rows = shell.read_with(cx, |shell, _| {
            [
                shell.context_switch_commands(),
                shell.namespace_switch_commands(),
                shell.kind_switch_commands(),
            ]
            .concat()
        });
        assert!(!rows.is_empty(), "a demo shell still lists switch targets");
        for label in labels(&rows) {
            assert!(
                !label.contains(':'),
                "{label:?} still spells out the row's verb and subject"
            );
        }
    }

    /// The current row is phrased like its siblings, and "current" comes from the
    /// badge the row already carries rather than from a second label shape.
    #[gpui_kit::test]
    fn the_current_namespace_row_reads_like_the_rest_of_its_list(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            let _ = &cx;
            shell.namespace = SharedString::from("perf");
            shell.namespace_state =
                NamespaceState::Ready(vec!["kube-system".into(), "perf".into()]);
        });
        cx.run_until_parked();

        let rows = shell.read_with(cx, |shell, _| shell.namespace_switch_commands());
        assert_eq!(labels(&rows), ["All namespaces", "kube-system", "perf"]);
        let current = rows
            .iter()
            .find(|command| command.label.as_str() == "perf")
            .expect("the current namespace is listed");
        assert!(
            matches!(&current.run, CommandRun::Unavailable { badge, .. } if *badge == "Current"),
            "the current row is marked by its badge, not by its label"
        );
    }

    /// A kind is named the way the tree names it, so the sidebar, the picker, and
    /// a search result do not each spell it differently.
    #[gpui_kit::test]
    fn a_kind_row_uses_the_tree_label_and_the_current_row_matches_it(cx: &mut TestAppContext) {
        init_app(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        shell.update(cx, |shell, cx| {
            shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        });
        cx.run_until_parked();

        let rows = shell.read_with(cx, |shell, _| shell.kind_switch_commands());
        let rows = labels(&rows);
        assert!(
            rows.contains(&"Config Maps".to_owned()),
            "the current kind is spelled the way the tree spells it, not as a raw Kind: {rows:?}"
        );
        for label in &rows {
            assert!(
                !label.contains("Resource View") && !label.contains(':'),
                "{label:?} still spells out the row's verb and subject"
            );
        }
    }

    /// `writing.md > Best practices` asks for one capitalization style per element
    /// type, applied throughout. The scope label is both the identity the shell
    /// compares against and the text a reader sees, so the search panel and the
    /// pickers have to say it the same way.
    /// `modality.md > Best practices` asks a modal view for a title that names its
    /// task, and a title that also carries a breadcrumb can only truncate.
    /// A Hotbar slot is a convenience, so a slot that names a cluster the registry
    /// has lost loses its own claim instead of every context.
    /// The same rule at the seam the app starts through: a stale slot leaves the
    /// registry's own current context as the cluster the session opens.
    #[gpui_kit::test]
    fn a_stale_hotbar_slot_leaves_the_current_context_loading(cx: &mut TestAppContext) {
        init_app(cx);
        cx.dispatcher.allow_parking();
        // The registry load is plain async with no view behind it, so the test owns its own
        // runtime: the app's window-side tokio bridge is not a dependency of this claim.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let handle = runtime.handle().clone();
        let path =
            std::env::temp_dir().join(format!("k8s-gpui-stale-hotbar-{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: alpha-ctx
  context: { cluster: alpha, user: alpha-user }
users:
- name: alpha-user
  user: {}
current-context: alpha-ctx
"#,
        )
        .expect("write kubeconfig");
        let registry = Arc::new(
            handle
                .block_on(ClusterRegistry::load(&path))
                .expect("load kubeconfig"),
        );
        let _ = std::fs::remove_file(&path);

        let mut bank = Bank::new("first");
        bank.slots.push(Slot::new(
            ClusterId::from_bits(0xbd6b_74f6_561b_e50e),
            "alpha-ctx",
        ));
        let hotbar = Hotbar {
            banks: vec![bank],
            active: 0,
        };
        let selected = preferred_cluster(
            &registry,
            hotbar_cluster(&hotbar, |id| registry.get(id).is_some()),
        );
        let session = ClusterSession::from_registry_with_cluster(registry, handle, selected);

        assert!(
            matches!(session, ClusterSession::Ready { .. }),
            "one stale slot must not cost the app every context"
        );
    }
}
