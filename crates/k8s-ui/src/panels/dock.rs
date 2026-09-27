//! Logs and terminal sessions in the bottom Dock.
//!
//! Log storage is virtualized and capped at 10,000 lines.
//! Interrupted log streams reconnect after an increasing delay.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, ClipboardItem, Context, CursorStyle, Entity, FocusHandle,
    Font, InteractiveElement, IntoElement, KeyDownEvent, KeybindingKeystroke, Keystroke,
    ListAlignment, ListHorizontalSizingBehavior, ListOffset, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, UniformListScrollHandle, Window, div,
    list, px, relative, uniform_list,
};
use k8s_actions::Copy as CopyAction;
use k8s_core::{
    atomic_file::{create_private_dir_all, write_atomic},
    ops::LogOptions,
    paths::{config_file, download_dir},
};
use ui::prelude::*;
use ui::{
    ContextMenu, ContextMenuEntry, KeyBinding, KeyBindingStyle, ListItem, ListItemSpacing,
    PopoverMenu, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
};

use crate::design::{self, Severity, severity_icon, space};
use crate::session::{LOG_EVENT_MAX_BYTES, TextInput};
use crate::settings::DataTypography;

use super::common::{
    TabSpec, buffer_font, empty_state, empty_state_with_action, label_small, label_text,
    status_message,
};
use super::logs::{
    LOG_SEVERITY_COLUMNS, LogBuffer, LogEvent, LogFactory, LogLevelScope, LogLine, LogPhase,
    LogRequest, LogSubscription, RING_CAPACITY, TailLines,
};
use super::terminal::{
    ALL_NAMESPACES, ForwardBinding, ForwardHandle, ForwardRequest, StartedForward, TerminalEvent,
    TerminalEventSink, TerminalInstance, TerminalKind, TerminalRequest, TerminalServices,
};

const TABS: [TabSpec; 2] = [
    TabSpec {
        label: "Logs",
        icon: IconName::Reader,
    },
    TabSpec {
        // `Terminal` is a solid rounded square: 96.9% of its box is ink, so the
        // *unselected* tab owned the only solid shape in the strip and outweighed
        // the selected one by 1.6x. `TerminalAlt` is the same frame drawn as a
        // 1.2px outline, so it reads as a peer of `Reader` and the state, not the
        // shape, carries the weight.
        label: "Terminal",
        icon: IconName::TerminalAlt,
    },
];

/// Restarts the terminal session the Dock is showing.
///
/// `no_register`: the only trigger is the `Restart` control in the session's own
/// state panel, and that panel is implemented in `k8s-app`, which cannot name a
/// Dock method. An action is the one channel that crosses the crate boundary
/// without either side reaching into the other, and leaving it out of the
/// keymap keeps the Dock from advertising a chord it does not own.
#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = k8s_dock, no_register)]
pub struct RestartTerminalSession;

/// Names the two tab groups and their panels, so a screen reader announces where focus is.
const DOCK_TAB_LIST_LABEL: &str = "Dock views";
const TERMINAL_SESSION_TAB_LIST_LABEL: &str = "Terminal sessions";
/// Spoken state for the visible "follow paused" note in the Logs toolbar.
const FOLLOW_PAUSED_DESCRIPTION: &str = "Follow paused. New log lines keep arriving but the view stays where you scrolled to. \
     Select Follow to jump back to the newest line.";
/// Label of the divider between the two terminal panes.
const TERMINAL_SPLIT_LABEL: &str = "Resize Terminal Split";
/// Label of the Dock header control that hides the Dock.
const DOCK_CLOSE_LABEL: &str = "Close Dock";
/// Key context the close control sits on, for reading its chord off the live keymap.
///
/// This is the context the Dock itself declares, and naming one that no view declares is how the
/// hint came to advertise a key on a surface the keymap never described. It is the Dock rather
/// than the window root because the button is inside the Dock's own focus path.
const DOCK_CLOSE_CONTEXT: &str = "Dock";
/// Key context that takes the Dock's keys away, and is declared by the terminal the Dock hosts.
const DOCK_TERMINAL_CONTEXT: &str = "Terminal";
/// Action the close control runs, shared with the `ToggleDock` key.
const DOCK_CLOSE_ACTION: &str = "k8s_shell::ToggleDock";
/// Focus order of the `Open Logs` control in the log body's empty state. It lives inside the
/// tab panel, so it follows the panel's own stop.
const OPEN_LOGS_TAB_INDEX: isize = 21;
/// Why the log body has nothing to show, and what moves it forward.
///
/// `writing.md > Best practices` asks for a control where one is possible, and the old copy
/// ("Open Logs from a Pod menu.") named a menu that is not on screen: in the capture the active
/// tab was Settings, so the sentence pointed at something the reader could not reach. The hint
/// is now built from what the Dock actually holds, and the control runs the same action the
/// command palette and the keymap run, so it either opens the stream or reports why it cannot.
const LOGS_NO_TARGET_TITLE: &str = "No log target";
const LOGS_NO_TARGET_HINT: &str = "Select a Pod, then open Logs to stream it.";
const LOGS_NO_SOURCE_HINT: &str =
    "A log target is selected, but no log source is connected. Connect a cluster, then retry.";

/// Name of the panel the active tab shows, with the session count for the Terminal tab.
fn tab_panel_label(active_tab: usize, sessions: usize) -> SharedString {
    match TABS.get(active_tab) {
        Some(_) if active_tab == 1 => terminal_tab_label(sessions),
        Some(tab) => SharedString::from(tab.label),
        None => SharedString::from("Dock"),
    }
}

/// Largest history the buffer keeps. The Tail menu, Load More History, and the cap notice all read
/// the top step of the ladder in `TailLines`, so they cannot disagree with what is retained.
fn history_cap() -> i64 {
    TailLines::cap().value()
}

/// One log row's height at the reader's configured data font size.
///
/// `design::text::DATA_LINE_HEIGHT` is the default, and a `const` cannot read a runtime setting,
/// so the constant kept 18px rows while the glyphs in them grew: a taller glyph in a shorter box,
/// which is the case the "Data font size" help text says cannot happen. The log grid is the
/// densest data surface in the app, so its row is the configured data line rather than
/// `design::size::ROW`; what matters is that the line the reader configures is the line the row,
/// the uniform list and the scroll arithmetic all measure with.
fn log_row_height(cx: &App) -> Pixels {
    crate::settings::data_typography(cx).line_height
}

/// Width of `columns` data characters at the reader's configured size.
///
/// `common::mono_columns` measures the product default, which is what a caller reserving room
/// before it holds a `DataTypography` has to use. A log row holds one, and a column measured at
/// 12px around a 16px glyph is a column that ellipsises every row it draws.
fn log_columns(typography: &DataTypography, columns: usize) -> Pixels {
    typography.columns(columns as f32)
}
const LOG_COPY_COLUMN_THRESHOLD: usize = 80;
const LOG_MESSAGE_MIN_COLUMNS: usize = 20;
/// Largest number of buffer lines one filter pass scores. A burst of 100,000 lines then costs
/// one bounded pass per frame instead of a full rescan of the buffer for every batch.
const FILTER_SCORING_BUDGET: usize = 1024;
/// Frames one filter pass may chain by asking for the next one. Twelve budgets cover the whole
/// ring, so a filter over retained history always finishes on its own. A live stream grows the
/// buffer faster than any budget can score it, so the pass stops asking past this budget and
/// waits for the next batch instead of spinning frames that never catch up.
const FILTER_PASS_FRAME_BUDGET: u32 = 12;
/// Fixed chrome above the log body: the tab bar, the toolbar, and the status banner row. The
/// Dock cannot shrink below it, because those rows do not scroll. `Pixels` arithmetic is not
/// `const`, so the sum is a function rather than a constant.
fn dock_chrome_height() -> Pixels {
    design::size::TAB_BAR + design::size::TOOLBAR + design::size::ROW
}
/// The terminal split cannot swallow either pane, so the divider stays inside these bounds.
const TERMINAL_SPLIT_MIN_RATIO: f32 = 0.2;
const TERMINAL_SPLIT_MAX_RATIO: f32 = 0.8;
const TERMINAL_SPLIT_KEY_STEP: f32 = 0.05;
/// The reconnect delay grows from 1 second to 10 seconds. The app stops after five failed attempts.
const RECONNECT_BASE: Duration = Duration::from_millis(1_000);
const RECONNECT_MAX: Duration = Duration::from_millis(10_000);
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// Wait for the first line after a reconnect.
const RECONNECT_WAIT: Duration = Duration::from_secs(10);
const LOG_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
const LOG_EVENT_BUFFER: usize = LOG_QUEUE_MAX_BYTES / LOG_EVENT_MAX_BYTES;
const LOG_BUFFER_MAX_BYTES: usize = LOG_QUEUE_MAX_BYTES;
/// Port forward rows stay readable without pushing the terminal out of the Dock.
const FORWARDS_VISIBLE_ROWS: f32 = 4.0;

/// Sends panel notices to the Shell toast handler.
type NoticeHandler = Box<dyn Fn(String, Severity, &mut App)>;

/// One log failure, in the words the Dock can show.
///
/// The banner and the empty state read this one record, so a state is never stated twice with two
/// different sentences, and the header chip and the status bar read the phase instead of the copy.
struct LogFailureNotice {
    /// The state name, for a surface that has a heading.
    title: &'static str,
    /// What happened and what to do next, in one sentence.
    guidance: SharedString,
    severity: Severity,
    icon: IconName,
    /// The raw reason, disclosed under the sentence rather than printed with it.
    detail: Option<String>,
    /// Whether a retry can move the request forward.
    retry: bool,
}

/// An open terminal session.
struct TerminalEntry {
    id: u64,
    request: TerminalRequest,
    instance: TerminalInstance,
    focus_handle: Option<FocusHandle>,
    title: Option<SharedString>,
    /// Process exit status.
    exit: Option<SharedString>,
    /// Second pane of a split. It shares its target with the pane it was split from, so it keeps
    /// the target label instead of adopting the OSC title both sessions report.
    split_peer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ForwardId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardPhase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardSnapshot {
    pub id: ForwardId,
    pub phase: ForwardPhase,
    pub request: ForwardRequest,
    pub label: SharedString,
    pub remote_port: u16,
    pub local_port: Option<u16>,
    pub error: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForwardSummary {
    pub active: usize,
    pub failed: usize,
    pub pending: usize,
    pub stopped: usize,
}

/// An active port forward.
struct ForwardEntry {
    id: ForwardId,
    request: ForwardRequest,
    label: SharedString,
    remote_port: u16,
    local_port: Option<u16>,
    handle: Option<Box<dyn ForwardHandle>>,
    phase: ForwardPhase,
    error: Option<SharedString>,
    runtime_id: u64,
}

impl ForwardEntry {
    fn snapshot(&self) -> ForwardSnapshot {
        ForwardSnapshot {
            id: self.id,
            phase: self.phase,
            request: self.request.clone(),
            label: self.label.clone(),
            remote_port: self.remote_port,
            local_port: self.local_port,
            error: self.error.clone(),
        }
    }
}

impl Drop for ForwardEntry {
    fn drop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.stop();
        }
    }
}

/// Who gives up the keyboard when a session cannot take it.
#[derive(Clone, Debug)]
enum TerminalFocusRecovery {
    /// The session never opened, so focus moves to the surviving control.
    Always,
    /// The session held the keyboard when it ended. Anything else keeps it: an async exit must
    /// not swallow the keyboard from the filter, the table, or another session.
    OnlyIfHeld(FocusHandle),
}

fn context_label(context: Option<&str>) -> &str {
    context
        .filter(|value| !value.is_empty())
        .unwrap_or("context")
}

fn namespace_label(namespace: Option<&str>) -> &str {
    namespace
        .filter(|value| !value.is_empty())
        .unwrap_or(ALL_NAMESPACES)
}

fn terminal_target_label(request: &TerminalRequest, context: Option<&str>) -> SharedString {
    let cluster = context_label(request.context.as_deref().or(context));
    match &request.kind {
        TerminalKind::Local => request
            .kind
            .title_for_namespace(Some(cluster), request.namespace.as_deref()),
        TerminalKind::Exec {
            namespace,
            pod,
            container,
        } => {
            let suffix = container
                .as_ref()
                .map(|container| format!(":{container}"))
                .unwrap_or_default();
            format!(
                "{cluster}/{}/Pod/{pod}{suffix}",
                namespace_label(Some(namespace.as_str()))
            )
            .into()
        }
    }
}

/// Names a terminal request for a diagnostic line: the same label the session chip shows, so a
/// log line and the window name the same session. The request carries no resource body, only the
/// namespace, Pod, and container names, so nothing secret can reach the line.
fn terminal_request_label(request: &TerminalRequest) -> String {
    terminal_target_label(request, None).to_string()
}

fn forward_target_label(request: &ForwardRequest, context: Option<&str>) -> SharedString {
    let cluster = context_label(request.context.as_deref().or(context));
    format!(
        "{cluster}/{}/Pod/{}",
        namespace_label(request.namespace.as_deref()),
        request.name
    )
    .into()
}

fn terminal_entry_title(entry: &TerminalEntry, context: Option<&str>) -> SharedString {
    match &entry.request.kind {
        TerminalKind::Local => terminal_target_label(&entry.request, context),
        TerminalKind::Exec { .. } => entry
            .title
            .clone()
            .filter(|_| !entry.split_peer)
            .unwrap_or_else(|| terminal_target_label(&entry.request, context)),
    }
}

/// The verdict a session chip carries, or `None` while the session has not given one.
///
/// `DESIGN.md` §4 Terminal keeps "not connected", "no sessions", "connecting", "session failed",
/// and "exited" as separate boundaries, and the chip was the one place in the strip that could
/// not tell any two of them apart: three sessions with #1 and #2 exited were drawn identically,
/// so the only way to find out was to switch to #2 and read the toolbar.
///
/// A reported exit is the one verdict the Dock owns. A session that has not exited has *no*
/// verdict rather than a healthy one, because the Dock does not observe the process: it would be
/// claiming an answer the app has not fetched, which is the mistake `DESIGN.md` §4 calls out by
/// name. The slot stays reserved and empty in that case, so the chip does not shift when a
/// verdict arrives.
fn session_verdict(exit: Option<&SharedString>) -> Option<Severity> {
    exit.map(|_| Severity::Warning)
}

/// The word a chip's verdict stands for, for the chip's accessible name.
fn session_verdict_label(exit: Option<&SharedString>) -> &'static str {
    match session_verdict(exit) {
        Some(severity) => design::health_label(severity),
        None => "Session open",
    }
}

/// The Terminal tab's label, with the session count once there is more than one.
///
/// The tab label was the constant `Terminal` in every state, so the strip said nothing about how
/// many sessions existed while the log body counted them. Above one, the count is the difference
/// between "one shell" and "three, and two of them ended".
fn terminal_tab_label(sessions: usize) -> SharedString {
    if sessions > 1 {
        format!("{} · {}", TABS[1].label, design::format::count(sessions)).into()
    } else {
        SharedString::from(TABS[1].label)
    }
}

/// Local port a forward is still listening on. A failed forward dropped its handle, so the port
/// it used to bind is no longer an address the user can reach.
fn forward_live_port(entry: &ForwardEntry) -> Option<u16> {
    match entry.phase {
        ForwardPhase::Running | ForwardPhase::Stopping => entry.local_port,
        ForwardPhase::Starting | ForwardPhase::Stopped | ForwardPhase::Failed => None,
    }
}

/// `localhost:8080 → ns/Pod/pod:8080` while a port is live, otherwise the target alone.
fn forward_row_target(entry: &ForwardEntry) -> String {
    match forward_live_port(entry) {
        Some(local_port) => format!(
            "localhost:{local_port} → {}:{}",
            entry.label, entry.remote_port
        ),
        None => format!("{}:{}", entry.label, entry.remote_port),
    }
}

/// Spoken text for a forward row: the target, the state, the next step, and the failure reason,
/// which a tooltip alone does not reach.
fn forward_row_aria(entry: &ForwardEntry, state: &'static str, next_step: &'static str) -> String {
    let target = forward_row_target(entry).replace(" → ", " to ");
    let mut aria = format!("{target}. Port forward {state}. {next_step}");
    if let Some(reason) = entry
        .error
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        aria.push_str(". ");
        aria.push_str(reason);
    }
    aria
}

fn timestamp_column_width(columns: usize, typography: &DataTypography) -> Pixels {
    log_columns(typography, columns)
}

/// Width of the severity cell a log row reserves.
///
/// The glyph is `design::size::STATUS_MARKER`, not the 12px it used to be, so the cell opens
/// with the marker token instead of the 8px severity dot. A fixed cell that the glyph outgrows
/// would push the level label past the row's measured width, and a no-wrap row scrolls a row
/// wider than the content it draws.
fn severity_column_width(typography: &DataTypography) -> Pixels {
    design::size::STATUS_MARKER
        + space::XS
        + log_columns(typography, LOG_SEVERITY_COLUMNS)
        + space::XS
}

/// Compact rows drop the level label, so the cell is the glyph and its gap.
fn compact_severity_column_width() -> Pixels {
    design::size::STATUS_MARKER + space::XS
}

/// Timestamp cell width for one row. The reserve keeps every message in the same column, and a
/// longer token still gets its own width so the row can show it in full.
fn log_timestamp_width(line: &LogLine, reserve: usize, typography: &DataTypography) -> Pixels {
    match line.timestamp_columns() {
        0 if reserve == 0 => px(0.),
        columns => timestamp_column_width(columns.max(reserve), typography),
    }
}

/// Columns a row spends on the marker, timestamp, and severity cells before the message. A compact
/// row drops the severity label, so it reserves less.
fn log_row_fixed_width(
    timestamp_width: Pixels,
    compact: bool,
    typography: &DataTypography,
) -> Pixels {
    space::SM
        + design::size::STATUS_DOT
        + space::SM
        + timestamp_width
        + space::SM
        + if compact {
            compact_severity_column_width()
        } else {
            severity_column_width(typography)
        }
        + space::SM
        + space::SM
}

/// Width a no-wrap row reserves. It must match the cells `log_row` draws, or the uniform list
/// scrolls a row that is wider than its content.
fn log_row_width(
    line: &LogLine,
    compact: bool,
    timestamp_reserve: usize,
    typography: &DataTypography,
) -> Pixels {
    let columns = line.display_columns();
    // Every row draws the copy control, so every row reserves it. A short line could be
    // selected with the keyboard, so hiding the control by line width left it uncopyable.
    log_row_fixed_width(
        log_timestamp_width(line, timestamp_reserve, typography),
        compact,
        typography,
    ) + log_columns(typography, columns)
        + design::size::CONTROL
        + space::SM
}

/// Smallest Dock height the shell should offer, derived from the same tokens the Dock lays out
/// with, so a test can hold `design::size::DOCK_MIN` to it.
pub fn dock_min_recommended(cx: &App) -> Pixels {
    dock_chrome_height() + log_row_height(cx) * 3.0
}

fn uniform_list_at_bottom(offset: Pixels, max_offset: Pixels) -> bool {
    max_offset <= px(0.) || -offset >= max_offset - space::XS
}

/// Records what the wrapped list reports when it scrolls.
///
/// The list calls its handler while it holds its own scroll state, so the handler cannot read
/// the state back and must not call into the panel: a re-entrant update would mutate the panel
/// in the middle of the list's own pass. Writing one cell and letting the panel pick it up at
/// its next render keeps the whole handoff one-way.
fn install_list_scroll_report(list_state: &ListState, report: &Rc<Cell<Option<bool>>>) {
    let report = Rc::clone(report);
    list_state.set_scroll_handler(move |event, _, _| report.set(Some(event.is_following_tail)));
}

fn longest_log_row(buffer: &LogBuffer, visible: &[usize]) -> usize {
    visible
        .iter()
        .enumerate()
        .filter_map(|(row, index)| {
            buffer
                .line(*index)
                .map(|line| (row, line.display_columns()))
        })
        .max_by_key(|(_, columns)| *columns)
        .map(|(row, _)| row)
        .unwrap_or(0)
}

fn command_tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    Tooltip::element(move |_, cx| div().text_ui_xs(cx).child(text.clone()).into_any_element())
}

/// The chord that closes the Dock from the surface that currently holds the keyboard, or `None`.
///
/// `default-linux.json` binds `secondary-j` to `ToggleDock` everywhere except the command
/// palette, and `Terminal` unbinds it. Reading the binding for a fixed context cannot see that,
/// because the Dock hosts the very terminal that took the key away: the hint said
/// `Close Dock Ctrl-J` while the reader was typing into a shell, and pressing Ctrl-J there sends
/// `^J` to readline's reverse history search. This is the one place in the app where the tooltip
/// says the reader is operating the app while they are operating a shell, so the question is
/// asked of the live focus stack instead of a constant.
fn dock_close_chord(contexts: &[gpui::KeyContext], cx: &App) -> Option<String> {
    if contexts.iter().any(|context| {
        context
            .primary()
            .is_some_and(|entry| entry.key.as_ref() == DOCK_TERMINAL_CONTEXT)
    }) {
        return None;
    }
    crate::keymap::binding_for_context(DOCK_CLOSE_ACTION, DOCK_CLOSE_CONTEXT, cx)
}

/// Tooltip for a Dock control that also has a key: the label, then the chord that reaches the
/// same action, so the shortcut is discoverable from the surface that owns it.
fn dock_control_tooltip(
    label: impl Into<SharedString>,
    chord: Option<String>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    let keystrokes = chord
        .as_deref()
        .and_then(|chord| Keystroke::parse(chord).ok())
        .map(|keystroke| vec![KeybindingKeystroke::from_keystroke(keystroke)]);
    Tooltip::element(move |_, cx| {
        let mut tooltip = h_flex().gap(space::SM).items_center();
        tooltip = tooltip.child(div().text_ui_xs(cx).child(label.clone()));
        if let Some(keystrokes) = keystrokes.clone() {
            tooltip = tooltip.child(
                KeyBinding::from_keystrokes(keystrokes.into(), false).style(KeyBindingStyle::Label),
            );
        }
        tooltip.into_any_element()
    })
}

/// Keyboard caret and range selection over the visible log rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LogSelection {
    /// Row the range starts at.
    anchor: usize,
    /// Row the caret sits at.
    head: usize,
}

impl LogSelection {
    /// Inclusive bounds of the range, ordered so the anchor may sit after the head.
    fn bounds(self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    fn contains(self, row: usize) -> bool {
        let (start, end) = self.bounds();
        (start..=end).contains(&row)
    }

    fn is_range(self) -> bool {
        self.anchor != self.head
    }
}

/// True when the keystroke is the editing chord that copies the log selection. Control+C is
/// free here: no child process shares the keyboard with the log list, and the filter input owns
/// its own focus while it has text.
///
/// Shift is not part of it: Control+Shift+C is a Shell command outside a session, and a log list
/// that swallowed the key would take it away from the rest of the app. The key itself is compared
/// without regard to case, so CapsLock, which XKB folds into the keysym, still reaches the chord.
fn is_log_copy_chord(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    if !modifiers.control || modifiers.shift || modifiers.alt || modifiers.platform {
        return false;
    }
    keystroke.key.eq_ignore_ascii_case("c")
}

/// True for the keys that move or extend the log view. The list claims them whether or not it has
/// rows, so an empty or failed stream cannot pass them on to the resource table behind the Dock.
fn is_log_navigation_key(key: &str) -> bool {
    matches!(key, "up" | "down" | "pageup" | "pagedown" | "home" | "end")
}

/// Names a log target for a diagnostic line: the namespace, the Pod, and the container. Those are
/// the three names a user would type to reproduce the request, and nothing else about the request
/// is written down.
fn log_target_label(request: Option<&LogRequest>, container: Option<&str>) -> String {
    let Some(request) = request else {
        return "no log target".to_owned();
    };
    let namespace = request.namespace.as_deref().unwrap_or("default");
    match container {
        Some(container) => format!("{namespace}/{}/{}", request.name, container),
        None => format!("{namespace}/{}", request.name),
    }
}

pub struct DockPanel {
    active_tab: usize,
    tab_focus_handles: [FocusHandle; 2],
    focus_handle: FocusHandle,
    /// Focusable close control in the Dock header. The Dock has no other way to say "hide me",
    /// and the toggle is unbound while a session owns the keyboard.
    close_focus: FocusHandle,
    /// Whether `Window::context_stack` can be asked yet.
    ///
    /// That accessor walks the rendered frame's focus tree and asserts the tree
    /// exists, so it is not callable from a render that has not painted. A reader
    /// cannot hover this button before there is a frame to hover in, so the hint
    /// has nothing to say until then; it starts blank and becomes correct on the
    /// first key event, which is the first thing that can only happen after a
    /// paint.
    context_stack_ready: Cell<bool>,

    request: Option<LogRequest>,
    container: Option<SharedString>,
    /// Requested history size. Load More History and the Tail menu walk the same ladder, and the
    /// choice survives switching log targets.
    history_lines: i64,
    timestamps: bool,
    wrap: bool,
    /// Tracks whether the view follows the tail.
    follow: bool,
    following: bool,
    paused: bool,
    phase: LogPhase,
    buffer: LogBuffer,
    buffer_bytes: usize,
    log_filter_input: Entity<TextInput>,
    log_filter: String,
    /// Next buffer index the filter has not scored yet. A pass only reads the lines that
    /// arrived since the last one, so a live stream never rescans the whole buffer.
    filter_cursor: usize,
    /// Frames the running filter pass may still ask for. The counter is what stops a live
    /// stream from turning the pass into an endless chain of `cx.notify()` calls.
    filter_frames_left: u32,
    /// True when a pass stopped on its frame budget with lines still unscored. The next log
    /// batch resumes the pass, so this records a throttled pass, never a stalled one.
    filter_pass_deferred: bool,
    /// Severity scope applied on top of the free-text filter.
    log_level: LogLevelScope,
    /// Keyboard caret and range selection over the visible log rows. `None` until the list
    /// takes focus.
    log_selection: Option<LogSelection>,
    /// Lines the ring buffer dropped since the current target was opened.
    dropped_lines: u64,
    visible_log_indices: Vec<usize>,
    longest_visible_log_row: usize,
    /// Number of rows synchronized with list_state.
    synced: usize,
    list_state: ListState,
    /// Tail state the wrapped list reported the last time it scrolled. The list owns its scroll
    /// state and runs its scroll handler while that state is borrowed, so the handler only
    /// records the report here and the panel reads it back at a point that holds no borrow.
    /// `None` means the list has not scrolled since the panel last looked.
    list_follow_report: Rc<Cell<Option<bool>>>,
    scroll_handle: UniformListScrollHandle,
    /// Tracks the Dock's own bounds, so the compact breakpoint follows the panel instead of
    /// the window. The Dock is `w_full()`, so the window width is only a first-frame guess.
    width_handle: ScrollHandle,
    /// Tracks the log body, so a Dock squeezed below a readable height says so instead of
    /// showing half a row of text.
    log_body_handle: ScrollHandle,

    factory: Option<LogFactory>,
    subscription: Option<Box<dyn LogSubscription>>,
    stream_task: Option<Task<()>>,
    reconnect_task: Option<Task<()>>,
    epoch: u64,
    lines_received: u64,
    attempts: u32,

    /// Terminal and port forward factories. No cluster connection hides the entry points.
    terminal_services: Option<TerminalServices>,
    terminals: Vec<TerminalEntry>,
    terminal_focus_handles: Vec<FocusHandle>,
    terminal_add_focus: FocusHandle,
    terminal_session_scroll: ScrollHandle,
    terminal_split: bool,
    /// Share of the split body the active pane takes. The divider owns it, pointer or keyboard.
    terminal_split_ratio: f32,
    /// Pointer position of the split divider, as a share of the body width, while dragging.
    terminal_split_drag: Option<f32>,
    /// Focusable divider between the two terminal panes.
    terminal_split_focus: FocusHandle,
    terminal_maximized: bool,
    active_terminal: usize,
    next_terminal_id: u64,
    forwards: Vec<ForwardEntry>,
    forwards_scroll: ScrollHandle,
    /// Focusable region that scrolls the forward list without a pointer.
    forwards_focus: FocusHandle,
    next_forward_id: u64,
    next_forward_runtime_id: u64,

    /// Focus handle for the `Open Logs` control in the log body's empty state, so the recovery
    /// path is on the same keyboard path as the log list it replaces.
    open_logs_focus: FocusHandle,

    notice: Option<NoticeHandler>,
    pending_notice: Option<(String, Severity, String)>,
    log_filter_observation: Option<Subscription>,
}

impl DockPanel {
    pub fn new(cx: &mut App) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, design::size::DOCK_MAX);
        let list_follow_report: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        install_list_scroll_report(&list_state, &list_follow_report);
        let log_filter_input = cx.new(|cx| {
            TextInput::new("Filter log lines…", cx, move |_text, _cx| {}).with_accessibility(
                "Filter Logs",
                "Type text to match log lines. Press Escape to clear the filter.",
                "Clear Log Filter",
            )
        });
        Self {
            active_tab: 0,
            tab_focus_handles: std::array::from_fn(|index| {
                cx.focus_handle().tab_stop(true).tab_index(index as isize)
            }),
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(2isize),
            close_focus: cx.focus_handle().tab_stop(true).tab_index(2isize),
            context_stack_ready: Cell::new(false),
            request: None,
            container: None,
            history_lines: TailLines::FiveHundred.value(),
            timestamps: false,
            wrap: true,
            follow: true,
            following: true,
            paused: false,
            phase: LogPhase::Idle,
            buffer: LogBuffer::new(),
            buffer_bytes: 0,
            log_filter_input,
            log_filter: String::new(),
            filter_cursor: 0,
            filter_frames_left: FILTER_PASS_FRAME_BUDGET,
            filter_pass_deferred: false,
            log_level: LogLevelScope::All,
            log_selection: None,
            dropped_lines: 0,
            visible_log_indices: Vec::new(),
            longest_visible_log_row: 0,
            synced: 0,

            list_state,
            list_follow_report,
            scroll_handle: UniformListScrollHandle::new(),
            width_handle: ScrollHandle::new(),
            log_body_handle: ScrollHandle::new(),
            factory: None,
            subscription: None,
            stream_task: None,
            reconnect_task: None,
            epoch: 0,
            lines_received: 0,
            attempts: 0,
            terminal_services: None,
            terminals: Vec::new(),
            terminal_focus_handles: Vec::new(),
            terminal_add_focus: cx.focus_handle().tab_stop(true).tab_index(5isize),
            terminal_session_scroll: ScrollHandle::new(),
            terminal_split: false,
            terminal_split_ratio: 0.5,
            terminal_split_drag: None,
            terminal_split_focus: cx.focus_handle().tab_stop(false).tab_index(8isize),
            terminal_maximized: false,
            active_terminal: 0,
            next_terminal_id: 0,
            forwards: Vec::new(),
            forwards_scroll: ScrollHandle::new(),
            forwards_focus: cx.focus_handle().tab_stop(true).tab_index(10isize),
            next_forward_id: 0,
            next_forward_runtime_id: 0,
            open_logs_focus: cx
                .focus_handle()
                .tab_stop(false)
                .tab_index(OPEN_LOGS_TAB_INDEX),
            notice: None,
            pending_notice: None,
            log_filter_observation: None,
        }
    }

    fn sync_focus_handles(&mut self) {
        for (index, handle) in self.tab_focus_handles.iter_mut().enumerate() {
            *handle = handle
                .clone()
                .tab_stop(index == self.active_tab)
                .tab_index(index as isize);
        }
        // The close control follows the tab strip, so it is the last stop in the header group.
        self.close_focus = self.close_focus.clone().tab_stop(true).tab_index(2isize);
        // The divider is a control, so it takes a tab stop while it is on screen. Its element
        // asks for one too, but a tracked focus handle only joins the tab order when the handle
        // itself carries the flag.
        self.terminal_split_focus = self
            .terminal_split_focus
            .clone()
            .tab_stop(self.terminal_split && self.terminals.len() > 1)
            .tab_index(8isize);
        self.focus_handle = self
            .focus_handle
            .clone()
            .tab_stop(self.active_tab == 0 || self.terminals.is_empty())
            .tab_index(20isize);
        self.terminal_add_focus = self
            .terminal_add_focus
            .clone()
            .tab_stop(self.terminal_available())
            .tab_index(5isize);
        // The control is painted with the band and nowhere else, so one predicate decides both
        // what is on screen and whether it joins the tab order. A mounted element with no tab stop
        // is unreachable, and a tab stop with no element is a stop that goes nowhere.
        self.open_logs_focus = self
            .open_logs_focus
            .clone()
            .tab_stop(self.log_band_is_shell())
            .tab_index(OPEN_LOGS_TAB_INDEX);
    }

    /// True when the Logs tab draws the isobaric band instead of the log controls.
    ///
    /// The band and the `Open Logs` control it holds appear together, and the band is what keeps
    /// the content's top edge put when there is nothing to control.
    fn log_band_is_shell(&self) -> bool {
        self.active_tab == 0 && !(self.request.is_some() && self.factory.is_some())
    }

    fn request_uniform_follow(&mut self) {
        if !self.follow || !self.following {
            return;
        }
        if self.wrap {
            self.list_state.scroll_to_end();
        } else {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    /// Width the Dock itself occupies. The Dock is `w_full()`, so the window width is only a
    /// first-frame guess; the tracked bounds replace it as soon as the panel has been laid out.
    fn dock_width(&self, window: &Window) -> Pixels {
        let measured = self.width_handle.bounds().size.width;
        if measured > px(0.) {
            measured
        } else {
            window.viewport_size().width
        }
    }

    /// Compact layout breakpoint, derived from the Dock's own width. The window is at least
    /// `window_min_size` wide, so the window width would never cross it.
    fn dock_is_compact(&self, window: &Window) -> bool {
        f32::from(self.dock_width(window)) < f32::from(design::size::CENTER_MIN)
    }

    fn log_row_context(&self, cx: &Context<Self>) -> LogRowContext {
        LogRowContext {
            selection: self.log_selection,
            panel: cx.entity().downgrade(),
        }
    }

    fn reset_uniform_scroll(&mut self) {
        self.scroll_handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(gpui::point(px(0.), px(0.)));
        self.scroll_handle.0.borrow_mut().deferred_scroll_to_item = None;
        self.request_uniform_follow();
    }

    /// Replaces the log stream with static lines for previews and tests.
    pub fn set_lines(&mut self, lines: Vec<String>, cx: &mut Context<Self>) {
        self.cancel_stream();
        self.log_filter.clear();
        self.log_filter_input
            .update(cx, |input, cx| input.clear(cx));
        self.buffer.clear();
        self.buffer_bytes = 0;
        self.dropped_lines = 0;
        self.filter_cursor = 0;
        self.log_selection = None;
        self.append_log_lines(lines);
        self.follow = true;
        self.following = true;
        self.rebuild_log_view(log_row_height(cx));
        self.reset_uniform_scroll();
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        self.phase = LogPhase::Streaming;
        self.active_tab = 0;
        self.terminal_maximized = false;
        self.sync_focus_handles();
        cx.notify();
    }

    /// Sets the log source. None disables streaming.
    pub fn set_log_factory(&mut self, factory: Option<LogFactory>, cx: &mut Context<Self>) {
        self.factory = factory;
        if self.request.is_some() {
            self.start_stream(cx);
        } else {
            cx.notify();
        }
    }

    /// Routes panel notices to Shell toasts.
    pub fn set_notice_handler(&mut self, handler: impl Fn(String, Severity, &mut App) + 'static) {
        self.notice = Some(Box::new(handler));
    }

    /// Returns the panel focus handle.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Live, paused, or reconnecting state of the log stream, for surfaces outside the Dock such
    /// as the status bar, which stays on screen while the Dock is collapsed. `None` when no log
    /// target is open.
    ///
    /// The bar names the transport state and nothing more. The Dock owns the failure: it names the
    /// class, the next step, and the reason, so a failed stream is not read out again here.
    pub fn log_status_label(&self) -> Option<&'static str> {
        self.should_show_log_status().then(|| {
            if self.paused {
                "Paused"
            } else {
                self.phase.label()
            }
        })
    }

    /// Selects Logs, clears the buffer, and starts a new stream.
    pub fn open_logs(&mut self, request: LogRequest, cx: &mut Context<Self>) {
        self.active_tab = 0;
        self.terminal_maximized = false;
        // Multi-container Pods require an explicit container.
        // Default to the first container. The menu can change it.
        self.container = request.containers.first().cloned();
        self.request = Some(request);
        self.log_filter.clear();
        self.log_filter_input
            .update(cx, |input, cx| input.clear(cx));
        self.buffer.clear();
        self.buffer_bytes = 0;
        self.visible_log_indices.clear();
        self.longest_visible_log_row = 0;
        self.synced = 0;
        self.filter_cursor = 0;
        self.filter_frames_left = FILTER_PASS_FRAME_BUDGET;
        self.filter_pass_deferred = false;
        self.list_state.reset(0);
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        self.attempts = 0;
        self.paused = false;
        self.follow = true;
        self.following = true;
        self.reset_uniform_scroll();
        self.sync_focus_handles();
        eprintln!(
            "k8s-gpui: log target opened: {}, tail {}, timestamps {}, follow {}, wrap {}",
            log_target_label(self.request.as_ref(), self.container.as_deref()),
            self.history_lines,
            self.timestamps,
            self.follow,
            self.wrap,
        );
        self.start_stream(cx);
    }

    /// Selects Logs without restarting the stream.
    pub fn show_logs_tab(&mut self, cx: &mut Context<Self>) {
        let changed = self.active_tab != 0 || self.terminal_maximized;
        self.active_tab = 0;
        self.terminal_maximized = false;
        self.sync_focus_handles();
        if changed {
            cx.notify();
        }
    }

    // Terminal sessions and port forwards.

    /// Sets terminal and port forward services.
    pub fn set_terminal_services(
        &mut self,
        services: Option<TerminalServices>,
        cx: &mut Context<Self>,
    ) {
        self.terminal_services = services;
        cx.notify();
    }

    /// Selects Terminal.
    pub fn show_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.active_tab = 1;
        self.sync_focus_handles();
        cx.notify();
    }

    fn terminal_available(&self) -> bool {
        self.terminal_services.is_some()
    }

    fn terminal_context_label(&self) -> Option<SharedString> {
        self.terminal_services.as_ref().and_then(|services| {
            let context = services
                .context
                .as_deref()
                .filter(|value| !value.is_empty())?;
            let namespace = namespace_label(services.namespace.as_deref());
            Some(format!("{context}/{namespace}").into())
        })
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let index = index.min(TABS.len().saturating_sub(1));
        self.active_tab = index;
        if index == 0 {
            self.terminal_maximized = false;
        }
        self.sync_focus_handles();
        window.focus(&self.tab_focus_handles[index], cx);
        cx.notify();
    }

    /// Opens a local or exec terminal.
    pub fn open_terminal(
        &mut self,
        kind: TerminalKind,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.open_terminal_kind(kind, false, cx)
    }

    fn open_terminal_kind(
        &mut self,
        kind: TerminalKind,
        split_peer: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(services) = self.terminal_services.clone() else {
            let recovery_index =
                (self.active_terminal < self.terminals.len()).then_some(self.active_terminal);
            self.focus_terminal_recovery(recovery_index, TerminalFocusRecovery::Always, cx);
            return Err("No context connection. Select a context, then try again.".to_owned());
        };
        let request = services.terminal_request(kind);
        self.open_terminal_request(request, split_peer, cx)
    }

    fn open_terminal_request(
        &mut self,
        request: TerminalRequest,
        split_peer: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(services) = self.terminal_services.clone() else {
            let recovery_index =
                (self.active_terminal < self.terminals.len()).then_some(self.active_terminal);
            self.focus_terminal_recovery(recovery_index, TerminalFocusRecovery::Always, cx);
            eprintln!(
                "k8s-gpui: terminal open failed: no context connection, {} open, active {}",
                self.terminals.len(),
                self.active_terminal,
            );
            return Err("No context connection. Select a context, then try again.".to_owned());
        };
        let id = self.alloc_terminal_id();
        let instance = match self.spawn_instance(&services, &request, id, cx) {
            Ok(instance) => instance,
            Err(reason) => {
                eprintln!(
                    "k8s-gpui: terminal factory failed for session {id}, {}: {reason}",
                    terminal_request_label(&request),
                );
                let recovery_index =
                    (self.active_terminal < self.terminals.len()).then_some(self.active_terminal);
                self.focus_terminal_recovery(recovery_index, TerminalFocusRecovery::Always, cx);
                self.notify_with_detail(
                    "Terminal failed to open. Try again.",
                    Severity::Error,
                    &reason,
                    cx,
                );
                return Err("Terminal failed to open. Try again.".to_owned());
            }
        };
        eprintln!(
            "k8s-gpui: terminal opened: session {id}, {}, split {}, context {:?}, {} open",
            terminal_request_label(&request),
            split_peer,
            services.context.as_deref().unwrap_or("none"),
            self.terminals.len() + 1,
        );
        self.terminals.push(TerminalEntry {
            id,
            request,
            instance,
            focus_handle: None,
            title: None,
            exit: None,
            split_peer,
        });
        self.terminal_focus_handles
            .push(cx.focus_handle().tab_stop(true).tab_index(3isize));
        self.active_terminal = self.terminals.len() - 1;
        self.sync_terminal_focus_handles();
        self.show_terminal_tab(cx);
        Ok(())
    }

    /// Replaces a session with a new instance and clears its exit state.
    pub fn restart_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.terminals.get(index) else {
            return;
        };
        let request = entry.request.clone();
        let id = entry.id;
        let Some(services) = self.terminal_services.clone() else {
            self.focus_terminal_recovery(Some(index), TerminalFocusRecovery::Always, cx);
            self.notify(
                "Terminal restart is unavailable. Select a context, then try again.",
                Severity::Error,
                cx,
            );
            return;
        };
        match self.spawn_instance(&services, &request, id, cx) {
            Ok(instance) => {
                let entry = &mut self.terminals[index];
                entry.instance = instance;
                entry.focus_handle = None;
                entry.title = None;
                entry.exit = None;
                self.activate_terminal(index, window, cx);
            }
            Err(reason) => {
                eprintln!("k8s-gpui: terminal restart failed: {reason}");
                self.focus_terminal_recovery(Some(index), TerminalFocusRecovery::Always, cx);
                self.notify_with_detail(
                    "Terminal restart failed. Try again.",
                    Severity::Error,
                    &reason,
                    cx,
                );
            }
        }
    }

    fn alloc_terminal_id(&mut self) -> u64 {
        let id = self.next_terminal_id;
        self.next_terminal_id = self.next_terminal_id.wrapping_add(1);
        id
    }

    fn sync_terminal_focus_handles(&mut self) {
        for (index, handle) in self.terminal_focus_handles.iter_mut().enumerate() {
            *handle = handle
                .clone()
                .tab_stop(index == self.active_terminal)
                .tab_index(3isize);
        }
    }

    /// Moves focus off a session that is gone. The neighbour that took its place gets the chip, and
    /// the New Terminal action takes over when nothing is left.
    fn focus_terminal_recovery(
        &self,
        index: Option<usize>,
        policy: TerminalFocusRecovery,
        cx: &mut Context<Self>,
    ) {
        let focus = index
            .filter(|_| self.terminals.len() > 1)
            .and_then(|index| {
                index
                    .checked_add(1)
                    .filter(|next| *next < self.terminals.len())
                    .or_else(|| index.checked_sub(1))
            })
            .and_then(|index| self.terminal_focus_handles.get(index).cloned())
            .unwrap_or_else(|| {
                if self.terminal_available() {
                    self.terminal_add_focus.clone()
                } else {
                    self.focus_handle.clone()
                }
            });
        let Some(window) = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next())
        else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, move |_, window, cx| {
                let may_take_focus = match &policy {
                    TerminalFocusRecovery::Always => true,
                    TerminalFocusRecovery::OnlyIfHeld(dead) => {
                        window.focused(cx).is_none_or(|focused| &focused == dead)
                    }
                };
                if may_take_focus {
                    window.focus(&focus, cx);
                }
            });
        });
    }

    /// Creates a terminal and binds its event sink.
    fn spawn_instance(
        &self,
        services: &TerminalServices,
        request: &TerminalRequest,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Result<TerminalInstance, String> {
        let weak = cx.weak_entity();
        let sink: TerminalEventSink = Box::new(move |event, cx: &mut App| {
            if let Some(dock) = weak.upgrade() {
                dock.update(cx, |dock, cx| dock.on_terminal_event(id, event, cx));
            }
        });
        (services.terminals)(request.clone(), sink, cx)
    }

    fn on_terminal_event(&mut self, id: u64, event: TerminalEvent, cx: &mut Context<Self>) {
        let Some(index) = self.terminals.iter().position(|entry| entry.id == id) else {
            // A session that ended after its entry was closed. The id says which one, so the
            // dropped event is traceable instead of looking like a session that never existed.
            eprintln!(
                "k8s-gpui: terminal event for an unknown session {id}, {} open",
                self.terminals.len()
            );
            return;
        };
        // A session that ends while the user works elsewhere must not pull focus away from the
        // filter, the table, or another session. Only the pane that lost the keyboard gives it up,
        // and the recovery check runs against the focus handle the pane held.
        let recover_focus = self.active_terminal == index
            && self.active_tab == 1
            && matches!(&event, TerminalEvent::Exited { .. });
        let dead_focus = self.terminals[index].focus_handle.clone();
        let entry = &mut self.terminals[index];
        match event {
            TerminalEvent::Title(title) => {
                if entry.request.kind.is_exec() && !entry.split_peer {
                    entry.title = (!title.is_empty()).then(|| SharedString::from(title));
                }
            }
            TerminalEvent::Exited { .. } => {
                let exit = event.exit_label();
                eprintln!(
                    "k8s-gpui: terminal session {id} ended: {}, active {}, focus recovered {}",
                    exit.as_deref().unwrap_or("no status"),
                    self.active_terminal,
                    recover_focus,
                );
                entry.exit = exit.map(SharedString::from);
                entry.focus_handle = None;
            }
        }
        if let Some(dead_focus) = recover_focus.then_some(dead_focus).flatten() {
            self.focus_terminal_recovery(
                Some(index),
                TerminalFocusRecovery::OnlyIfHeld(dead_focus),
                cx,
            );
        }
        cx.notify();
    }

    pub fn close_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.terminals.len() {
            return;
        }
        eprintln!(
            "k8s-gpui: terminal session {} closed: {}, split {}, focused {}, {} open",
            self.terminals[index].id,
            terminal_request_label(&self.terminals[index].request),
            self.terminals[index].split_peer,
            self.terminals[index]
                .focus_handle
                .as_ref()
                .is_some_and(|focus| focus.contains_focused(window, cx)),
            self.terminals.len() - 1,
        );
        let had_terminal_focus = self.terminals[index]
            .focus_handle
            .as_ref()
            .is_some_and(|focus| focus.contains_focused(window, cx));
        if had_terminal_focus {
            window.blur(cx);
        }
        self.terminals.remove(index);
        if index < self.terminal_focus_handles.len() {
            self.terminal_focus_handles.remove(index);
        }
        self.active_terminal = self
            .active_terminal
            .min(self.terminals.len().saturating_sub(1));
        // A split view needs two panes, so closing either one leaves it. The divider must not
        // come back with the ratio the user dragged it to, or with a drag that never ended.
        if self.terminals.len() < 2 {
            self.close_terminal_split();
        }
        if self.terminals.is_empty() {
            self.terminal_maximized = false;
        }
        self.sync_terminal_focus_handles();
        if let Some(focus) = self.terminal_focus_handles.get(self.active_terminal) {
            window.focus(focus, cx);
        } else if self.terminal_available() {
            window.focus(&self.terminal_add_focus, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Selects a session and focuses its terminal.
    pub fn activate_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.terminals.len() {
            return;
        }
        self.active_terminal = index;
        self.sync_terminal_focus_handles();
        self.terminal_session_scroll.scroll_to_item(index);
        let previous = window.focused(cx);
        // A pane that already holds the keyboard keeps it. The instance takes nothing in that
        // case, which is the same "focus did not move" a connecting session produces.
        let pane_already_held = self.terminals[index]
            .focus_handle
            .as_ref()
            .is_some_and(|held| Some(held) == previous.as_ref());
        (self.terminals[index].instance.activate)(window, cx);
        let focused = window.focused(cx);
        if let Some(focused) = focused.filter(|focused| Some(focused.clone()) != previous) {
            self.terminals[index].focus_handle = Some(focused);
        } else if !pane_already_held {
            // A session that is still connecting has no view to focus yet, so the instance takes
            // nothing. The Dock has to take the keyboard anyway, or the arrows keep driving the
            // resource table behind a Dock the user just opened.
            let fallback = self.terminal_focus_fallback(index);
            window.focus(&fallback, cx);
            eprintln!(
                "k8s-gpui: terminal session {} has no focusable view yet, focus moved to the Dock",
                self.terminals[index].id,
            );
        }
        cx.notify();
    }

    /// Focus target for a terminal that cannot take the keyboard itself. The session chip is only
    /// rendered once there is more than one session, so the New Terminal control takes the Dock
    /// otherwise, and the panel focus handle is the last resort.
    fn terminal_focus_fallback(&self, index: usize) -> FocusHandle {
        if self.terminals.len() > 1
            && let Some(focus) = self.terminal_focus_handles.get(index)
        {
            return focus.clone();
        }
        if self.terminal_available() {
            return self.terminal_add_focus.clone();
        }
        self.focus_handle.clone()
    }

    fn split_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            self.notify(
                "Open a terminal before you split the view.",
                Severity::Info,
                cx,
            );
            return;
        }
        if self.terminals.len() == 1 {
            // The second pane keeps the scope of the pane it was split from. An exec session
            // splits into the same pod; anything else splits into a local shell.
            let request = self.terminals[self.active_terminal].request.split_request();
            let created = self.terminals.len();
            if let Err(reason) = self.open_terminal_request(request, true, cx) {
                eprintln!("k8s-gpui: terminal split failed: {reason}");
                self.focus_terminal_recovery(
                    Some(self.active_terminal),
                    TerminalFocusRecovery::Always,
                    cx,
                );
                self.notify(
                    "Second terminal failed to open. Try again.",
                    Severity::Error,
                    cx,
                );
                return;
            }
            // The new pane takes the keyboard: the user split in order to type into it.
            self.terminal_split = true;
            if self.terminals.len() > created {
                self.activate_terminal(created, window, cx);
            }
            return;
        }
        self.terminal_split = true;
        if self.terminals.len() > 1 {
            let target = (self.active_terminal + 1) % self.terminals.len();
            self.activate_terminal(target, window, cx);
        }
    }

    fn toggle_terminal_maximized(&mut self, cx: &mut Context<Self>) {
        if !self.terminals.is_empty() {
            self.terminal_maximized = !self.terminal_maximized;
            cx.notify();
        }
    }

    /// Leaves the split view and hands its keyboard back to the session chips.
    fn close_terminal_split(&mut self) {
        self.terminal_split = false;
        self.terminal_split_drag = None;
        self.terminal_split_ratio = 0.5;
    }

    /// Opens a local shell with a temporary kubeconfig for the current context.
    fn new_local_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.open_terminal(TerminalKind::Local, cx) {
            eprintln!("k8s-gpui: terminal open failed: {reason}");
            self.notify("Terminal failed to open. Try again.", Severity::Error, cx);
            return;
        }
        self.activate_terminal(self.active_terminal, window, cx);
    }

    pub fn terminal_count(&self) -> usize {
        self.terminals.len()
    }

    pub fn forward_count(&self) -> usize {
        self.forwards.len()
    }

    pub fn forward_errors(&self) -> Vec<Option<&str>> {
        self.forwards
            .iter()
            .map(|entry| entry.error.as_deref())
            .collect()
    }

    pub fn forward_snapshots(&self) -> Vec<ForwardSnapshot> {
        self.forwards.iter().map(ForwardEntry::snapshot).collect()
    }

    pub fn forward_summary(&self) -> ForwardSummary {
        let mut summary = ForwardSummary::default();
        for entry in &self.forwards {
            match entry.phase {
                ForwardPhase::Running => summary.active += 1,
                ForwardPhase::Failed => summary.failed += 1,
                ForwardPhase::Starting | ForwardPhase::Stopping => summary.pending += 1,
                ForwardPhase::Stopped => summary.stopped += 1,
            }
        }
        summary
    }

    fn alloc_forward_id(&mut self) -> ForwardId {
        let id = ForwardId(self.next_forward_id);
        self.next_forward_id = self.next_forward_id.wrapping_add(1);
        id
    }

    fn alloc_forward_runtime_id(&mut self) -> u64 {
        self.next_forward_runtime_id = self.next_forward_runtime_id.wrapping_add(1);
        self.next_forward_runtime_id
    }

    fn spawn_forward_callbacks(
        &mut self,
        id: ForwardId,
        runtime_id: u64,
        binding: ForwardBinding,
        mut errors: tokio::sync::mpsc::UnboundedReceiver<String>,
        cx: &mut Context<Self>,
    ) {
        let weak_binding = cx.weak_entity();
        cx.spawn(async move |_this, cx| match binding.await {
            Ok(Ok(local_port)) => {
                weak_binding
                    .update(cx, |dock, cx| {
                        dock.on_forward_bound(id, runtime_id, local_port, cx)
                    })
                    .ok();
            }
            Ok(Err(reason)) => {
                weak_binding
                    .update(cx, |dock, cx| {
                        dock.on_forward_failed(id, runtime_id, reason, cx)
                    })
                    .ok();
            }
            Err(_) => {
                weak_binding
                    .update(cx, |dock, cx| dock.on_forward_cancelled(id, runtime_id, cx))
                    .ok();
            }
        })
        .detach();

        let weak_errors = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            while let Some(reason) = errors.recv().await {
                if weak_errors
                    .update(cx, |dock, cx| {
                        dock.on_forward_error(id, runtime_id, reason, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
            weak_errors
                .update(cx, |dock, cx| dock.on_forward_ended(id, runtime_id, cx))
                .ok();
        })
        .detach();
    }

    fn start_forward_attempt(
        &mut self,
        id: ForwardId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(index) = self.forwards.iter().position(|entry| entry.id == id) else {
            return Err("The selected port forward no longer exists.".to_owned());
        };
        if !matches!(
            self.forwards[index].phase,
            ForwardPhase::Stopped | ForwardPhase::Failed
        ) {
            return Err(
                "The port forward is already starting or running. Select Stop to end it."
                    .to_owned(),
            );
        }
        let request = self.forwards[index].request.clone();
        let started = self
            .terminal_services
            .clone()
            .filter(TerminalServices::is_available)
            .ok_or_else(|| "No context connection. Select a context, then try again.".to_owned())
            .and_then(|services| (services.forwards)(request, cx));
        let StartedForward {
            handle,
            binding,
            errors,
        } = match started {
            Ok(started) => started,
            Err(reason) => {
                eprintln!("k8s-gpui: port forward factory failed: {reason}");
                let entry = &mut self.forwards[index];
                entry.phase = ForwardPhase::Failed;
                entry.local_port = None;
                entry.error = Some(reason.into());
                return Err(
                    "The port forward did not start. Check the port and connection, then try again."
                        .to_owned(),
                );
            }
        };
        let runtime_id = self.alloc_forward_runtime_id();
        let entry = &mut self.forwards[index];
        entry.runtime_id = runtime_id;
        entry.phase = ForwardPhase::Starting;
        entry.local_port = None;
        entry.error = None;
        entry.handle = Some(handle);
        self.spawn_forward_callbacks(id, runtime_id, binding, errors, cx);
        cx.notify();
        Ok(())
    }

    pub fn create_forward(
        &mut self,
        request: ForwardRequest,
        cx: &mut Context<Self>,
    ) -> Result<ForwardId, String> {
        let Some(services) = self
            .terminal_services
            .clone()
            .filter(TerminalServices::is_available)
        else {
            return Err("No context connection. Select a context, then try again.".to_owned());
        };
        let id = self.alloc_forward_id();
        let label = forward_target_label(&request, services.context.as_deref());
        let remote_port = request.remote_port;
        self.forwards.push(ForwardEntry {
            id,
            request,
            label,
            remote_port,
            local_port: None,
            handle: None,
            phase: ForwardPhase::Stopped,
            error: None,
            runtime_id: 0,
        });
        let result = self.start_forward_attempt(id, cx);
        if let Err(reason) = result {
            cx.notify();
            return Err(reason);
        }
        Ok(id)
    }

    /// Starts a port forward. The bound port arrives asynchronously.
    pub fn start_forward(
        &mut self,
        request: ForwardRequest,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let result = self.create_forward(request, cx);
        self.show_terminal_tab(cx);
        result.map(|_| ())
    }

    pub fn restart_forward(&mut self, id: ForwardId, cx: &mut Context<Self>) -> Result<(), String> {
        if !self
            .forwards
            .iter()
            .any(|entry| entry.id == id && entry.phase == ForwardPhase::Stopped)
        {
            return Err("Select Start to start a stopped port forward.".to_owned());
        }
        let result = self.start_forward_attempt(id, cx);
        if let Err(reason) = &result {
            eprintln!("k8s-gpui: port forward start failed: {reason}");
            self.notify(
                "The port forward did not start. Select Retry to reconnect.",
                Severity::Error,
                cx,
            );
            cx.notify();
        }
        result
    }

    pub fn retry_forward(&mut self, id: ForwardId, cx: &mut Context<Self>) -> Result<(), String> {
        if !self
            .forwards
            .iter()
            .any(|entry| entry.id == id && entry.phase == ForwardPhase::Failed)
        {
            return Err("Select Retry to retry a failed port forward.".to_owned());
        }
        let result = self.start_forward_attempt(id, cx);
        if let Err(reason) = &result {
            eprintln!("k8s-gpui: port forward retry failed: {reason}");
            self.notify(
                "The port forward did not start. Select Retry to reconnect.",
                Severity::Error,
                cx,
            );
            cx.notify();
        }
        result
    }

    fn on_forward_bound(
        &mut self,
        id: ForwardId,
        runtime_id: u64,
        local_port: u16,
        cx: &mut Context<Self>,
    ) {
        if local_port == 0 {
            self.on_forward_failed(
                id,
                runtime_id,
                "Port forward did not bind a local port. Select Retry to start a new forward."
                    .to_owned(),
                cx,
            );
            return;
        }
        let Some(entry) = self
            .forwards
            .iter_mut()
            .find(|entry| entry.id == id && entry.runtime_id == runtime_id)
        else {
            return;
        };
        if entry.phase != ForwardPhase::Starting {
            return;
        }
        let label = entry.label.clone();
        let remote_port = entry.remote_port;
        entry.local_port = Some(local_port);
        entry.phase = ForwardPhase::Running;
        self.notify(
            &format!("Forwarding localhost:{local_port} to {label}:{remote_port}"),
            Severity::Success,
            cx,
        );
        cx.notify();
    }

    fn on_forward_failed(
        &mut self,
        id: ForwardId,
        runtime_id: u64,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.finish_forward_failure(id, runtime_id, reason, "failed", cx);
    }

    fn on_forward_error(
        &mut self,
        id: ForwardId,
        runtime_id: u64,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.finish_forward_failure(id, runtime_id, reason, "stopped", cx);
    }

    fn on_forward_cancelled(&mut self, id: ForwardId, runtime_id: u64, cx: &mut Context<Self>) {
        self.finish_forward_failure(
            id,
            runtime_id,
            "Port forward was canceled. Select Retry to start a new forward.".to_owned(),
            "canceled",
            cx,
        );
    }

    fn on_forward_ended(&mut self, id: ForwardId, runtime_id: u64, cx: &mut Context<Self>) {
        let phase = self
            .forwards
            .iter()
            .find(|entry| entry.id == id && entry.runtime_id == runtime_id)
            .map(|entry| entry.phase);
        if phase == Some(ForwardPhase::Stopping) {
            if let Some(entry) = self
                .forwards
                .iter_mut()
                .find(|entry| entry.id == id && entry.runtime_id == runtime_id)
            {
                entry.phase = ForwardPhase::Stopped;
                entry.local_port = None;
                entry.error = None;
            }
            cx.notify();
            return;
        }
        if !matches!(phase, Some(ForwardPhase::Starting | ForwardPhase::Running)) {
            return;
        }
        self.finish_forward_failure(
            id,
            runtime_id,
            "Port forward ended unexpectedly. Select Retry to start a new forward.".to_owned(),
            "ended",
            cx,
        );
    }

    fn finish_forward_failure(
        &mut self,
        id: ForwardId,
        runtime_id: u64,
        reason: String,
        state: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .forwards
            .iter_mut()
            .find(|entry| entry.id == id && entry.runtime_id == runtime_id)
        else {
            return;
        };
        if !matches!(entry.phase, ForwardPhase::Starting | ForwardPhase::Running) {
            return;
        }
        let label = entry.label.clone();
        entry.phase = ForwardPhase::Failed;
        entry.error = Some(reason.clone().into());
        // The handle is gone, so the bound port is not an address the user can reach any more.
        entry.local_port = None;
        if let Some(mut handle) = entry.handle.take() {
            handle.stop();
        }
        eprintln!("k8s-gpui: port forward {state}: {reason}");
        self.notify_with_detail(
            &format!("Port forward for {label} {state}. Select Retry to reconnect."),
            Severity::Error,
            &reason,
            cx,
        );
        cx.notify();
    }

    pub fn stop_forward(&mut self, id: ForwardId, cx: &mut Context<Self>) {
        let Some(entry) = self.forwards.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if !matches!(entry.phase, ForwardPhase::Starting | ForwardPhase::Running) {
            return;
        }
        entry.phase = ForwardPhase::Stopping;
        entry.error = None;
        if let Some(mut handle) = entry.handle.take() {
            handle.stop();
        }
        cx.notify();
    }

    /// Removes sessions and forwards that belong to the old cluster.
    pub fn close_cluster_sessions(&mut self, cx: &mut Context<Self>) {
        if self.terminals.is_empty() && self.forwards.is_empty() {
            return;
        }
        self.terminals.clear();
        self.terminal_focus_handles.clear();
        self.forwards.clear();
        self.forwards_scroll = ScrollHandle::new();
        self.close_terminal_split();
        self.terminal_maximized = false;
        self.active_terminal = 0;
        self.sync_focus_handles();
        cx.notify();
    }

    pub fn phase(&self) -> &LogPhase {
        &self.phase
    }

    pub fn request(&self) -> Option<&LogRequest> {
        self.request.as_ref()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn is_following(&self) -> bool {
        self.following
    }

    pub fn history_lines(&self) -> i64 {
        self.history_lines
    }

    pub fn selected_container(&self) -> Option<&SharedString> {
        self.container.as_ref()
    }

    /// Restarts a failed log stream.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.attempts = 0;
        self.start_stream(cx);
    }

    fn log_options(&self) -> LogOptions {
        LogOptions {
            container: self.container.as_ref().map(ToString::to_string),
            follow: true,
            tail_lines: Some(self.history_lines),
            timestamps: self.timestamps,
            ..Default::default()
        }
    }

    fn start_stream(&mut self, cx: &mut Context<Self>) {
        self.cancel_stream();
        let Some(request) = self.request.clone() else {
            return;
        };
        let Some(factory) = self.factory.clone() else {
            self.phase = LogPhase::Unavailable(
                "No context connection. Select a context, then try again.".to_owned(),
            );
            self.trace_stream("unavailable");
            cx.notify();
            return;
        };
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let options = self.log_options();
        let (sink, mut receiver) = tokio::sync::mpsc::channel(LOG_EVENT_BUFFER);
        self.subscription = Some(factory(request, options, sink));
        self.lines_received = 0;
        self.phase = if self.paused {
            LogPhase::Streaming
        } else {
            LogPhase::Connecting
        };
        self.trace_stream("start");

        self.stream_task = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = receiver.try_recv() {
                    batch.push(next);
                    if batch.len() >= LOG_EVENT_BUFFER {
                        break;
                    }
                }
                let ended = batch
                    .iter()
                    .any(|event| matches!(event, LogEvent::Ended(_)));
                if this
                    .update(cx, |panel, cx| panel.on_log_batch(epoch, batch, cx))
                    .is_err()
                {
                    return;
                }
                if ended {
                    return;
                }
            }
        }));

        if self.attempts > 0 {
            let timeout_epoch = epoch;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RECONNECT_WAIT).await;
                this.update(cx, |panel, cx| {
                    if panel.epoch == timeout_epoch
                        && panel.lines_received == 0
                        && matches!(panel.phase, LogPhase::Connecting)
                    {
                        panel.on_stream_ended(
                            "The log stream stopped before it sent data.".to_owned(),
                            cx,
                        );
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn cancel_stream(&mut self) {
        if let Some(mut subscription) = self.subscription.take() {
            subscription.cancel();
        }
        self.stream_task = None;
        self.reconnect_task = None;
    }

    fn on_log_batch(&mut self, epoch: u64, batch: Vec<LogEvent>, cx: &mut Context<Self>) {
        if epoch != self.epoch {
            // A batch from a stream the Dock already replaced. The epoch is the only thing that
            // separates a late line from a current one, so say how many arrived late.
            eprintln!(
                "k8s-gpui: log stream dropped a stale batch: epoch {epoch}, current {}",
                self.epoch
            );
            return;
        }
        let mut raws = Vec::new();
        let mut ended = None;
        for event in batch {
            match event.bounded() {
                LogEvent::Line(raw) => raws.push(raw),
                LogEvent::Ended(reason) => ended = Some(reason),
            }
        }
        if !raws.is_empty() {
            let (appended, dropped, rebuilt) = self.append_log_lines(raws);
            if rebuilt {
                // The byte limit rebuilt the buffer, so every stored index moved.
                self.restart_filter(log_row_height(cx));
            } else {
                self.sync_list(appended, dropped, log_row_height(cx));
            }
            self.lines_received += appended as u64;
            self.dropped_lines = self.dropped_lines.saturating_add(dropped as u64);
            self.attempts = 0;
            if matches!(
                self.phase,
                LogPhase::Connecting | LogPhase::Reconnecting { .. }
            ) {
                self.phase = LogPhase::Streaming;
                self.trace_stream("first lines");
            }
            cx.notify();
        }
        if let Some(reason) = ended {
            self.on_stream_ended(reason, cx);
        }
    }

    fn append_log_lines(&mut self, raws: Vec<String>) -> (usize, usize, bool) {
        self.append_log_lines_with_limit(raws, LOG_BUFFER_MAX_BYTES)
    }

    fn append_log_lines_with_limit(
        &mut self,
        raws: Vec<String>,
        max_bytes: usize,
    ) -> (usize, usize, bool) {
        if raws.is_empty() {
            return (0, 0, false);
        }
        let appended = raws.len();
        let incoming_bytes = raws
            .iter()
            .fold(0usize, |total, raw| total.saturating_add(raw.len()));
        if self.buffer_bytes.saturating_add(incoming_bytes) <= max_bytes {
            let overflow = self
                .buffer
                .len()
                .saturating_add(appended)
                .saturating_sub(RING_CAPACITY);
            let evicted_bytes = (0..overflow)
                .filter_map(|index| self.buffer.line(index))
                .map(|line| line.raw.len())
                .sum::<usize>();
            let (appended, dropped) = self.buffer.push_many(raws);
            self.buffer_bytes = self
                .buffer_bytes
                .saturating_sub(evicted_bytes)
                .saturating_add(incoming_bytes);
            return (appended, dropped, false);
        }

        let mut combined = Vec::with_capacity(self.buffer.len() + appended);
        for index in 0..self.buffer.len() {
            if let Some(line) = self.buffer.line(index) {
                combined.push(line.raw.to_string());
            }
        }
        combined.extend(raws);
        let mut retained = Vec::with_capacity(RING_CAPACITY.min(combined.len()));
        let mut retained_bytes = 0usize;
        for raw in combined.into_iter().rev() {
            if retained.len() == RING_CAPACITY {
                break;
            }
            let next_bytes = retained_bytes.saturating_add(raw.len());
            if next_bytes <= max_bytes {
                retained_bytes = next_bytes;
                retained.push(raw);
            }
        }
        retained.reverse();
        self.buffer = LogBuffer::new();
        self.buffer.push_many(retained);
        self.buffer_bytes = retained_bytes;
        (appended, 0, true)
    }

    fn sync_list(&mut self, appended: usize, dropped: usize, row_height: Pixels) {
        if self.log_view_is_filtered() {
            // Ring eviction moved every stored index, so the existing match set is rebased
            // before the pass scores the lines that arrived since the last one.
            self.rebase_filter(dropped);
            self.run_filter_pass(row_height);
            return;
        }
        if dropped > 0 {
            self.list_state.splice(0..dropped, 0);
            self.synced = self.synced.saturating_sub(dropped);
        }
        if appended > 0 {
            let start = self.synced;
            self.list_state.splice(start..start, appended);
            self.synced = start + appended;
        }
        if self.log_filter.is_empty() {
            self.longest_visible_log_row = self.buffer.longest_message_index();
        }
        self.clamp_log_selection();
        self.request_uniform_follow();
    }

    /// Shifts the stored match set and the filter cursor after the ring dropped `dropped` lines
    /// from the front.
    fn rebase_filter(&mut self, dropped: usize) {
        if dropped == 0 {
            return;
        }
        debug_assert!(
            self.visible_log_indices
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "the stored match set is sorted before the rebase"
        );
        // A stored index below the drop bound belongs to a line the ring discarded, so there is
        // nothing to point at any more. Removing it, instead of subtracting and clamping to zero,
        // is what keeps the set strictly increasing: the survivors shift by the same amount the
        // ring shifted the buffer, so they stay sorted and stay inside it.
        self.visible_log_indices.retain(|index| *index >= dropped);
        for index in &mut self.visible_log_indices {
            *index -= dropped;
        }
        self.filter_cursor = self.filter_cursor.saturating_sub(dropped);
        self.clamp_log_selection();
    }

    /// Scores at most one budget of buffered lines and merges the matches into the visible set.
    /// The cursor only moves forward, so a live stream costs the lines that arrived since the
    /// last pass instead of the whole buffer.
    fn score_filter_budget(&mut self) {
        let start = self.filter_cursor.min(self.buffer.len());
        let end = (start + FILTER_SCORING_BUDGET).min(self.buffer.len());
        if start >= end {
            return;
        }
        let mut matches = self
            .buffer
            .matching_indices_in(start..end, &self.log_filter);
        matches.retain(|index| {
            self.buffer
                .line(*index)
                .is_some_and(|line| self.log_level.accepts(line.severity))
        });
        self.visible_log_indices.extend(matches);
        self.filter_cursor = end;
        self.clamp_log_selection();
        self.longest_visible_log_row = longest_log_row(&self.buffer, &self.visible_log_indices);
    }

    /// Continues the filter where it stopped. Returns true when another pass is still owed, which
    /// is the caller's cue to ask for the next frame.
    fn run_filter_pass(&mut self, row_height: Pixels) -> bool {
        if !self.log_view_is_filtered() || self.filter_cursor >= self.buffer.len() {
            return false;
        }
        self.score_filter_budget();
        self.refresh_filtered_list(row_height);
        self.filter_cursor < self.buffer.len()
    }

    /// Runs one budget of the filter pass from a frame and asks for the next one while the pass
    /// budget lasts. Returns true when it asked, so a caller can see that the pass is chaining
    /// frames.
    ///
    /// A live stream appends faster than a budget can score, so the cursor never catches up and
    /// an unbounded notify here would ask for a frame that asks for a frame, for as long as the
    /// stream runs. Past the budget the pass stops asking and waits for the next log batch,
    /// which scores the lines that actually arrived.
    fn continue_filter_pass(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.run_filter_pass(log_row_height(cx)) {
            if self.filter_pass_deferred {
                self.filter_pass_deferred = false;
                self.filter_frames_left = FILTER_PASS_FRAME_BUDGET;
                self.trace_filter("complete");
            }
            return false;
        }
        if self.filter_frames_left == 0 {
            if !self.filter_pass_deferred {
                self.filter_pass_deferred = true;
                self.trace_filter("deferred");
            }
            return false;
        }
        self.filter_frames_left -= 1;
        cx.notify();
        true
    }

    /// Lifecycle line for the filter pass. Reports how much of the buffer is scored, never the
    /// query or the lines themselves: a filter query is user text and the lines are Pod output.
    fn trace_filter(&self, state: &str) {
        eprintln!(
            "k8s-gpui: log filter pass {state}: {}/{} lines scored, {} visible, filter {} chars, level {:?}",
            self.filter_cursor,
            self.buffer.len(),
            self.visible_log_count(),
            self.log_filter.chars().count(),
            self.log_level,
        );
    }

    /// Applies the current match set to the list. Only the row count changes, so this stays
    /// cheap enough to run once per batch.
    fn refresh_filtered_list(&mut self, row_height: Pixels) {
        self.synced = self.buffer.len();
        self.list_state
            .reset_with_uniform_height(self.visible_log_count(), row_height);
        if !self.follow {
            self.following = false;
        }
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        if !self.following {
            self.list_state.pause_following_tail();
        }
        self.request_uniform_follow();
    }

    /// True when the free-text filter or the level scope hides part of the buffer.
    fn log_view_is_filtered(&self) -> bool {
        !self.log_filter.is_empty() || self.log_level != LogLevelScope::All
    }

    fn on_stream_ended(&mut self, reason: String, cx: &mut Context<Self>) {
        if let Some(mut subscription) = self.subscription.take() {
            subscription.cancel();
        }
        if self.lines_received > 0 {
            self.attempts = 0;
        }
        self.attempts += 1;
        if self.attempts > MAX_RECONNECT_ATTEMPTS {
            self.phase = LogPhase::Failed {
                reason: reason.clone(),
            };
            self.trace_reason("gave up", &reason);
            cx.notify();
            return;
        }
        let delay = reconnect_delay(self.attempts);
        self.phase = LogPhase::Reconnecting {
            attempt: self.attempts,
            reason: reason.clone(),
        };
        self.trace_reason(&format!("reconnect in {}ms", delay.as_millis()), &reason);
        let epoch = self.epoch;
        self.reconnect_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |panel, cx| {
                if panel.epoch == epoch && matches!(panel.phase, LogPhase::Reconnecting { .. }) {
                    panel.start_stream(cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Lifecycle line for the log stream. Names the target, the epoch, and the counts, never the
    /// lines: a Pod log is unfiltered output and must not reach a terminal someone else can read.
    fn trace_stream(&self, state: &str) {
        eprintln!(
            "k8s-gpui: log stream {state}: {}, epoch {}, phase {}, tail {}, buffered {} lines, \
             {} lines received, {} dropped, {} bytes, timestamps {}, wrap {}, follow {}",
            log_target_label(self.request.as_ref(), self.container.as_deref()),
            self.epoch,
            self.phase.label(),
            self.history_lines,
            self.buffer.len(),
            self.lines_received,
            self.dropped_lines,
            self.buffer_bytes,
            self.timestamps,
            self.wrap,
            self.follow,
        );
    }

    /// Lifecycle line for a stream failure, with the reason the cluster gave. The reason is the
    /// one string worth having in the log: it names what the request was rejected for.
    fn trace_reason(&self, state: &str, reason: &str) {
        eprintln!(
            "k8s-gpui: log stream {state}: {}, epoch {}, attempt {} of {}, reason: {reason}",
            log_target_label(self.request.as_ref(), self.container.as_deref()),
            self.epoch,
            self.attempts,
            MAX_RECONNECT_ATTEMPTS,
        );
    }

    /// Restarts the filter from the top of the buffer. Typing replaces any pass that is still
    /// owed, so a burst of keystrokes cannot queue one pass per character.
    fn set_log_filter(&mut self, query: &str, cx: &mut Context<Self>) {
        self.log_filter_input
            .update(cx, |input, cx| input.set_text(query, cx));
        if self.log_filter == query {
            return;
        }
        self.log_filter = query.to_owned();
        self.restart_filter(log_row_height(cx));
        cx.notify();
    }

    fn set_log_level(&mut self, level: LogLevelScope, cx: &mut Context<Self>) {
        if self.log_level == level {
            return;
        }
        self.log_level = level;
        self.restart_filter(log_row_height(cx));
        cx.notify();
    }

    /// Clears the stored match set and scores the first budget of the buffer. Any remaining work
    /// continues on the following frames.
    fn restart_filter(&mut self, row_height: Pixels) {
        self.visible_log_indices.clear();
        self.filter_cursor = 0;
        self.log_selection = None;
        // A new query owes a new pass, so it also gets a full frame budget. Without the reset
        // the first keystroke of the next filter would inherit the previous pass's exhaustion.
        self.filter_frames_left = FILTER_PASS_FRAME_BUDGET;
        self.filter_pass_deferred = false;
        if !self.log_view_is_filtered() {
            self.rebuild_log_view(row_height);
            return;
        }
        self.score_filter_budget();
        // The pass has nothing to do on an empty buffer, but the list must still shrink to the
        // rows that matched.
        self.refresh_filtered_list(row_height);
    }

    fn visible_log_count(&self) -> usize {
        if self.log_view_is_filtered() {
            self.visible_log_indices.len()
        } else {
            self.buffer.len()
        }
    }

    /// Resets the view when nothing is filtered: every buffered line is visible, so the list
    /// takes the buffer length and the stored match set is not needed.
    fn rebuild_log_view(&mut self, row_height: Pixels) {
        self.visible_log_indices.clear();
        self.filter_cursor = 0;
        self.longest_visible_log_row = self.buffer.longest_message_index();
        self.synced = self.buffer.len();
        self.list_state
            .reset_with_uniform_height(self.visible_log_count(), row_height);
        if !self.follow {
            self.following = false;
        }
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        if !self.following {
            self.list_state.pause_following_tail();
        }
        self.request_uniform_follow();
    }

    /// Whether the uniform list, the scroll owner in no-wrap mode, sits at the newest line.
    fn uniform_list_at_bottom(&self) -> bool {
        let state = self.scroll_handle.0.borrow();
        let at_bottom = state.deferred_scroll_to_item.is_some()
            || uniform_list_at_bottom(
                state.base_handle.offset().y,
                state.base_handle.max_offset().y,
            );
        drop(state);
        at_bottom
    }

    fn set_follow(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.follow = follow;
        self.following = follow;
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        if follow {
            self.request_uniform_follow();
        } else {
            self.list_state.pause_following_tail();
            self.scroll_handle.0.borrow_mut().deferred_scroll_to_item = None;
        }
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            &self.phase,
            LogPhase::Connecting | LogPhase::Streaming | LogPhase::Reconnecting { .. }
        ) {
            return;
        }
        if self.paused {
            self.paused = false;
            self.set_follow(true, cx);
            if matches!(self.phase, LogPhase::Connecting)
                || matches!(self.phase, LogPhase::Reconnecting { .. })
            {
                cx.notify();
            }
        } else {
            self.paused = true;
            self.set_follow(false, cx);
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.buffer.clear();
        self.buffer_bytes = 0;
        self.dropped_lines = 0;
        self.filter_cursor = 0;
        self.log_selection = None;
        self.rebuild_log_view(log_row_height(cx));
        self.reset_uniform_scroll();
        cx.notify();
    }

    fn set_tail(&mut self, tail: TailLines, cx: &mut Context<Self>) {
        if self.history_lines == tail.value() {
            return;
        }
        self.history_lines = tail.value();
        self.restart(cx);
    }

    /// Increases the requested history by one step of the shared ladder.
    fn load_earlier(&mut self, cx: &mut Context<Self>) {
        let Some(next) = TailLines::next_after(self.history_lines) else {
            self.notify(
                &format!(
                    "Log history is limited to {} lines.",
                    format_count(history_cap() as usize)
                ),
                Severity::Warning,
                cx,
            );
            return;
        };
        self.set_tail(next, cx);
        self.notify(
            &format!(
                "Loading the last {} lines…",
                format_count(self.history_lines as usize)
            ),
            Severity::Info,
            cx,
        );
    }

    fn set_container(&mut self, container: SharedString, cx: &mut Context<Self>) {
        if self.container.as_ref() == Some(&container) {
            return;
        }
        self.container = Some(container);
        self.restart(cx);
    }

    fn set_timestamps(&mut self, timestamps: bool, cx: &mut Context<Self>) {
        if self.timestamps == timestamps {
            return;
        }
        self.timestamps = timestamps;
        self.restart(cx);
    }

    /// Changing the source or request options restarts the stream and clears old lines.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.buffer.clear();
        self.buffer_bytes = 0;
        self.dropped_lines = 0;
        self.filter_cursor = 0;
        self.log_selection = None;
        self.visible_log_indices.clear();
        self.longest_visible_log_row = 0;
        self.synced = 0;
        self.list_state.reset(0);
        self.attempts = 0;
        self.following = self.follow;
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        if !self.follow {
            self.list_state.pause_following_tail();
        }
        self.reset_uniform_scroll();
        self.start_stream(cx);
    }

    fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        if !self.following {
            self.list_state.pause_following_tail();
        }
        self.request_uniform_follow();
        cx.notify();
    }

    /// Downloads buffered lines to the download directory.
    /// Uses the configuration or temporary directory when the download directory is unavailable.
    fn download(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_empty() {
            self.notify(
                "No log lines are available to download. Wait for log output, then select Download Logs.",
                Severity::Warning,
                cx,
            );
            return;
        }
        let lines = self.buffer.len();
        let text = self.buffer.text();
        let directory = download_dir()
            .filter(|path| path.is_dir())
            .or_else(|| config_file("logs"))
            .unwrap_or_else(std::env::temp_dir);
        let result = export_log_file(&directory, &self.suggested_filename(), text.as_bytes());
        match result {
            Ok(path) => self.notify(
                &format!("Downloaded {lines} log lines to {}", path.display()),
                Severity::Success,
                cx,
            ),
            Err(error) => {
                let detail = error.to_string();
                self.notify_with_detail(
                    "Download Logs failed. Check the destination folder, then try again.",
                    Severity::Error,
                    &detail,
                    cx,
                );
            }
        }
    }

    fn suggested_filename(&self) -> String {
        let name = self
            .request
            .as_ref()
            .map(|request| sanitize_filename_component(&request.name))
            .unwrap_or_else(|| "logs".to_owned());
        let timestamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
        format!("k8s-gpui-{name}-{timestamp}.log")
    }

    fn notify(&mut self, message: &str, severity: Severity, cx: &mut Context<Self>) {
        self.pending_notice = None;
        self.dispatch_notice(message, severity, cx);
    }

    fn notify_with_detail(
        &mut self,
        message: &str,
        severity: Severity,
        detail: &str,
        cx: &mut Context<Self>,
    ) {
        self.pending_notice =
            (!detail.trim().is_empty()).then(|| (message.to_owned(), severity, detail.to_owned()));
        self.dispatch_notice(message, severity, cx);
    }

    pub fn pending_notice_detail(&self, message: &str, severity: Severity) -> Option<String> {
        self.pending_notice
            .as_ref()
            .filter(|(pending_message, pending_severity, _)| {
                pending_message == message && *pending_severity == severity
            })
            .map(|(_, _, detail)| detail.clone())
    }

    fn dispatch_notice(&self, message: &str, severity: Severity, cx: &mut Context<Self>) {
        match &self.notice {
            Some(handler) => handler(message.to_owned(), severity, cx),
            None => eprintln!("[dock] {message}"),
        }
    }

    /// The stream state is worth showing on every Dock tab: the log stream keeps running while
    /// the Terminal tab is open, and a maximised terminal hides the tab bar altogether.
    fn should_show_log_status(&self) -> bool {
        self.request.is_some() && !matches!(self.phase, LogPhase::Idle)
    }

    /// The header chip is a Dock-local summary, so it steps aside while the log body names the
    /// same state. The body carries the class, the next step, and the raw reason; the chip is the
    /// word that remains when the user is on the Terminal tab instead.
    fn header_status_is_useful(&self) -> bool {
        self.should_show_log_status() && !self.log_body_reports_failure()
    }

    /// True when the log body is the surface that reports this state, because it has nothing else
    /// to show. The banner then stays away instead of repeating the empty state one row above it.
    fn log_body_carries_state(&self) -> bool {
        self.buffer.is_empty() && self.log_failure_notice().is_some()
    }

    /// True when the log body already names this failure, in the empty state or in the banner. The
    /// header chip then steps aside: a state the Dock names in one row is not named in the next.
    fn log_body_reports_failure(&self) -> bool {
        self.active_tab == 0 && self.log_failure_notice().is_some()
    }

    /// The log failure the Dock has to report, in one record. A failed stream names its class, so
    /// a Pod that is still Pending is not reported as a Pod that is gone.
    fn log_failure_notice(&self) -> Option<LogFailureNotice> {
        match &self.phase {
            LogPhase::Reconnecting { attempt, reason } => Some(LogFailureNotice {
                title: "Reconnecting",
                guidance: format!(
                    "Log connection lost. Reconnect attempt {attempt} is in progress…"
                )
                .into(),
                severity: Severity::Warning,
                icon: IconName::RotateCw,
                detail: Some(reason.clone()),
                retry: false,
            }),
            LogPhase::Failed { reason } => {
                let failure = self.phase.failure()?;
                Some(LogFailureNotice {
                    title: failure.word(),
                    guidance: failure.guidance().into(),
                    severity: failure.severity(),
                    icon: IconName::Warning,
                    detail: Some(reason.clone()),
                    retry: true,
                })
            }
            LogPhase::Unavailable(reason) => Some(LogFailureNotice {
                title: "No Log Source",
                guidance: "Select a context, then open Logs.".into(),
                severity: Severity::Muted,
                icon: IconName::Warning,
                detail: Some(reason.clone()),
                retry: false,
            }),
            _ => None,
        }
    }

    fn render_tabs(&mut self, window: &Window, cx: &Context<Self>) -> AnyElement {
        self.sync_focus_handles();
        let colors = cx.theme().colors();
        let status = self
            .header_status_is_useful()
            .then(|| self.render_status_chip(cx));
        let mut items = h_flex()
            .id("dock-tabs-items")
            .h_full()
            .flex_none()
            .items_center();
        for (index, tab) in TABS.into_iter().enumerate() {
            let selected = index == self.active_tab;
            // The Terminal tab counts its sessions once there is more than one, so the strip says
            // how many shells are open rather than repeating a constant word.
            let label: SharedString = match index {
                1 => terminal_tab_label(self.terminals.len()),
                _ => SharedString::from(tab.label),
            };
            let focus = self.tab_focus_handles[index]
                .clone()
                .tab_stop(selected)
                .tab_index(index as isize);
            let item = h_flex()
                .id(("dock-tab", index))
                .debug_selector(move || format!("dock-tab-{index}"))
                .relative()
                .h_full()
                .flex_none()
                .px(space::MD)
                .gap(space::XS)
                .items_center()
                .font_ui(cx)
                .text_size(rems_from_px(f32::from(design::text::BODY)))
                .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
                // The focus ring only ever draws a border, and the accent rail
                // below is absolutely positioned against the padding box, so a
                // full `border_1()` put a 1px strip of tab background between the
                // rail and the strip's own hairline. Only the sides are needed.
                .border_x_1()
                .border_color(colors.border_transparent)
                .cursor_pointer()
                .track_focus(&focus)
                .tab_index(index as isize)
                .tab_stop(selected)
                .role(Role::Tab)
                .aria_label(label.clone())
                .aria_selected(selected)
                .accessibility_id(format!("dock-tab-{index}"))
                .aria_keyshortcuts("Enter Space ArrowLeft ArrowRight Home End")
                // `DESIGN.md` §3.4 files the active tab under `raised_surface`,
                // and `design::surface::tab_active` is that role. The active tab
                // used to be a rail and a colour change over an identical
                // background, so the strip carried no surface hierarchy at all
                // and the strongest cue was the 2px accent under the tab.
                .when(selected, |this| this.bg(design::surface::tab_active(cx)))
                .hover(|this| this.bg(colors.element_hover))
                .active(|this| this.bg(colors.element_active))
                .focus_visible(|this| this.border_color(colors.border_focused))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_tab(index, window, cx);
                    cx.stop_propagation();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    this.context_stack_ready.set(true);
                    if event.keystroke.modifiers.control
                        || event.keystroke.modifiers.alt
                        || event.keystroke.modifiers.platform
                    {
                        return;
                    }
                    let target = match event.keystroke.key.as_str() {
                        "enter" | "return" | "space" => index,
                        "left" | "up" => (index + TABS.len() - 1) % TABS.len(),
                        "right" | "down" => (index + 1) % TABS.len(),
                        "home" => 0,
                        "end" => TABS.len() - 1,
                        _ => return,
                    };
                    this.active_tab = target;
                    this.sync_focus_handles();
                    if let Some(handle) = this.tab_focus_handles.get(target) {
                        window.focus(handle, cx);
                    }
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(
                    Icon::new(tab.icon)
                        .size(IconSize::XSmall)
                        .color(if selected {
                            Color::Default
                        } else {
                            Color::Muted
                        }),
                )
                .child(label_text(label).color(if selected {
                    Color::Default
                } else {
                    Color::Muted
                }))
                .when(selected, |this| {
                    this.child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(design::border::FOCUS_RAIL)
                            .bg(colors.text_accent),
                    )
                });
            items = items.child(item);
        }
        h_flex()
            .id("dock-tabs")
            .debug_selector(|| "dock-tabs-row".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TAB_BAR)
            .items_center()
            .tab_group()
            .bg(colors.tab_bar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border)
            // The close control is not a tab, so the tablist stops at the tab strip.
            .child(
                h_flex()
                    .id("dock-tab-list")
                    .role(Role::TabList)
                    .aria_label(DOCK_TAB_LIST_LABEL)
                    .h_full()
                    .flex_none()
                    .items_center()
                    .child(items),
            )
            .child(div().flex_1().min_w(px(0.)))
            .when_some(status, |this, status| this.child(status))
            .child(self.render_close_control(window, cx))
            .into_any_element()
    }

    /// The Dock header's only way to say "hide me". It runs the same action as the `ToggleDock`
    /// key, because that key is released on every surface that owns the keyboard, including the
    /// Terminal the Dock itself hosts.
    ///
    /// The hint is built against the surface that currently holds the keyboard, because that is
    /// the surface the chord would have to travel through. While a session is focused the Dock
    /// can only be closed with the pointer, and the tooltip says so by drawing no keycap.
    fn render_close_control(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let chord = self
            .context_stack_ready
            .get()
            .then(|| dock_close_chord(&window.context_stack(), cx))
            .flatten();
        div()
            .id("dock-close")
            .debug_selector(|| "dock-close".to_owned())
            .flex_none()
            .px(space::SM)
            .child(
                IconButton::new("dock-close-button", IconName::Close)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .tab_index(2isize)
                    .track_focus(&self.close_focus)
                    .tooltip(dock_control_tooltip(DOCK_CLOSE_LABEL, chord))
                    .aria_label(DOCK_CLOSE_LABEL)
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(crate::shell::ToggleDock), cx);
                    })),
            )
            .into_any_element()
    }

    /// The Dock-local state word. A failure shows its class name and nothing else: the reason and
    /// the next step belong to the log body, which is the one surface that reports the whole
    /// failure, so the chip carries no disclosure of its own.
    fn render_status_chip(&self, cx: &Context<Self>) -> AnyElement {
        let (label, severity) = if self.paused {
            ("Paused", Severity::Muted)
        } else {
            self.phase.failure().map_or_else(
                || (self.phase.label(), self.phase.severity()),
                |failure| (failure.word(), failure.severity()),
            )
        };
        h_flex()
            .id("dock-log-status")
            .debug_selector(|| "dock-log-status".to_owned())
            .flex_none()
            .gap(space::XS)
            .items_center()
            .px(space::MD)
            .child(status_message(severity, label, None, cx))
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let container = self.container.clone();
        let containers = self
            .request
            .as_ref()
            .map(|request| request.containers.clone())
            .unwrap_or_default();
        let paused = self.paused;
        let follow = self.following;
        // The stream is still running while the view stopped following it, so the state needs a
        // visible label: an unselected Follow button is not enough.
        let follow_paused = self.follow && !self.following;
        let has_source = self.request.is_some() && self.factory.is_some();
        let can_control = matches!(
            self.phase,
            LogPhase::Connecting | LogPhase::Streaming | LogPhase::Reconnecting { .. }
        );
        let filter_count = self.log_view_is_filtered().then(|| {
            let matching = self.visible_log_count();
            let total = self.buffer.len();
            (
                format!("{} / {}", format_count(matching), format_count(total)),
                format!("Log filter count: {matching} of {total}."),
            )
        });
        let dropped_note = (self.dropped_lines > 0).then(|| {
            let dropped = self.dropped_lines;
            (
                format!("{} dropped", format_count(dropped as usize)),
                format!(
                    "Log dropped count: {dropped} older lines. The buffer keeps the newest {} lines.",
                    format_count(RING_CAPACITY)
                ),
            )
        });
        let source_actions = h_flex()
            .id("dock-log-source-actions")
            .flex_none()
            .gap(space::XS)
            .items_center()
            .when(containers.len() > 1, |this| {
                this.child(self.render_container_menu(container, containers, cx))
            })
            .child(self.render_tail_menu(has_source, cx))
            .when(has_source && self.history_lines < history_cap(), |this| {
                this.child(
                    Button::new("dock-earlier", "Load More History")
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .label_size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::BODY,
                        ))))
                        .tab_index(5isize)
                        .tooltip(Tooltip::text(format!(
                            "Select Load More History to load more lines. The current limit is {} lines.",
                            format_count(self.history_lines as usize)
                        )))
                        .aria_label("Load More Log History")
                        .on_click(cx.listener(|this, _, _, cx| this.load_earlier(cx))),
                )
            });
        let view_actions = h_flex()
            .id("dock-log-view-actions")
            .flex_none()
            .gap(space::XS)
            .items_center()
            .child(
                Button::new("dock-follow", "Follow")
                    .style(ButtonStyle::OutlinedGhost)
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(6isize)
                    .toggle_state(follow)
                    .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .disabled(!can_control)
                    .tooltip(Tooltip::text(
                        "Keep the newest lines in view. Follow pauses when you scroll up and resumes at the bottom.",
                    ))
                     .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                         let next = !this.following;
                         this.set_follow(next, cx);
                     })),
            )
            .child(
                IconButton::new(
                    "dock-pause",
                    if paused {
                        IconName::PlayOutlined
                    } else {
                        IconName::DebugPause
                    },
                )
                .size(ButtonSize::Medium)
                .icon_size(IconSize::XSmall)
                .tab_index(7isize)
                .toggle_state(paused)
                .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
                .tooltip(command_tooltip(if paused {
                    "Resume live updates. New lines continue to buffer while paused."
                } else {
                    "Pause the view. New lines continue to buffer."
                }))
                .aria_label(if paused { "Resume" } else { "Pause" })
                .disabled(!can_control)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
            )
            .child(self.render_log_level_menu(cx))
            .child(self.render_log_options_menu(cx));
        h_flex()
            .id("dock-log-toolbar")
            .debug_selector(|| "dock-log-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .tab_group()
            .font_ui(cx)
            .text_size(rems_from_px(f32::from(design::text::BODY)))
            .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
            .bg(colors.toolbar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border_variant)
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .child(self.log_filter_input.clone())
            .when_some(filter_count, |this, (count, aria_label)| {
                this.child(
                    h_flex()
                        .id("dock-log-filter-status")
                        .flex_none()
                        .role(Role::Status)
                        .aria_label(aria_label)
                        .child(label_small(count).color(Color::Muted)),
                )
            })
            .when_some(dropped_note, |this, (count, aria_label)| {
                this.child(
                    h_flex()
                        .id("dock-log-dropped-status")
                        .debug_selector(|| "dock-log-dropped-status".to_owned())
                        .flex_none()
                        .role(Role::Status)
                        .aria_label(aria_label)
                        .child(label_small(count).color(Color::Muted)),
                )
            })
            .when(follow_paused, |this| {
                this.child(
                    h_flex()
                        .id("dock-log-follow-paused")
                        .debug_selector(|| "dock-log-follow-paused".to_owned())
                        .flex_none()
                        .gap(space::XS)
                        .role(Role::Status)
                        .aria_label(FOLLOW_PAUSED_DESCRIPTION)
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(label_small("Follow paused").color(Color::Muted)),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(design::border::LINE)
                    .h(design::size::CONTROL)
                    .bg(colors.border_variant),
            )
            .child(source_actions)
            .child(div().flex_1().min_w(space::SM))
            .child(view_actions)
            .into_any_element()
    }

    /// Severity scope for the log list. Severity is parsed from the first token of a line, so
    /// this is the only way to ask for warnings or errors without typing their labels.
    fn render_log_level_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let level = self.log_level;
        let has_lines = !self.buffer.is_empty();
        PopoverMenu::new("dock-log-level")
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    if !has_lines {
                        return menu;
                    }
                    LogLevelScope::ALL.into_iter().fold(menu, |menu, option| {
                        let panel = panel.clone();
                        menu.toggleable_entry(
                            option.label(),
                            option == level,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_log_level(option, cx))
                                    .ok();
                            },
                        )
                    })
                }))
            })
            .trigger(
                Button::new("dock-log-level", format!("Level: {}", level.short_label()))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(11isize)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(level.description()))
                    .aria_label(format!("Log level: {}", level.label()))
                    .disabled(!has_lines),
            )
            .into_any_element()
    }

    fn render_log_options_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let timestamps = self.timestamps;
        let wrap = self.wrap;
        let has_lines = !self.buffer.is_empty();
        PopoverMenu::new("dock-log-options")
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let timestamps_panel = panel.clone();
                    let menu = menu.toggleable_entry(
                        "Show Timestamps",
                        timestamps,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            timestamps_panel
                                .update(cx, |panel, cx| {
                                    let next = !panel.timestamps;
                                    panel.set_timestamps(next, cx);
                                })
                                .ok();
                        },
                    );
                    let wrap_panel = panel.clone();
                    let menu = menu.toggleable_entry(
                        "Wrap Long Lines",
                        wrap,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            wrap_panel
                                .update(cx, |panel, cx| panel.toggle_wrap(cx))
                                .ok();
                        },
                    );
                    if !has_lines {
                        return menu;
                    }
                    let clear_panel = panel.clone();
                    let menu = menu.item(
                        ContextMenuEntry::new("Clear Log Buffer")
                            .icon(IconName::Eraser)
                            .handler(move |_, cx| {
                                clear_panel.update(cx, |panel, cx| panel.clear(cx)).ok();
                            }),
                    );
                    let download_panel = panel;
                    menu.item(
                        ContextMenuEntry::new("Download Logs")
                            .icon(IconName::Download)
                            .handler(move |_, cx| {
                                download_panel
                                    .update(cx, |panel, cx| panel.download(cx))
                                    .ok();
                            }),
                    )
                }))
            })
            .trigger(
                Button::new("dock-log-options", "Options")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(8isize)
                    .start_icon(Icon::new(IconName::Settings).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("View log display and buffer actions."))
                    .aria_label("Log options"),
            )
            .into_any_element()
    }

    fn render_tail_menu(&self, enabled: bool, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let history_lines = self.history_lines;
        let cap = history_cap();
        PopoverMenu::new("dock-tail-menu")
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    // The menu offers the whole ladder, so the value Load More History reaches is
                    // also selectable here and exactly one entry stays checked.
                    let mut menu = TailLines::ALL.into_iter().fold(menu, |menu, option| {
                        let panel = panel.clone();
                        menu.toggleable_entry(
                            format!("Last {} Lines", option.label()),
                            option.value() == history_lines,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_tail(option, cx))
                                    .ok();
                            },
                        )
                    });
                    if history_lines >= cap {
                        menu = menu.separator();
                        menu = menu.item(
                            ContextMenuEntry::new(format!(
                                "History is capped at {} lines",
                                format_count(cap as usize)
                            ))
                            .disabled(true),
                        );
                    }
                    menu
                }))
            })
            .trigger(
                Button::new(
                    "dock-tail",
                    format!("History: {} Lines", format_count(history_lines as usize)),
                )
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Medium)
                .label_size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::BODY,
                ))))
                .tab_index(3isize)
                .disabled(!enabled)
                .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                .tooltip(Tooltip::text(
                    "Number of lines fetched from the container log.",
                ))
                .aria_label(format!(
                    "Log history: {} lines",
                    format_count(history_lines as usize)
                )),
            )
            .into_any_element()
    }

    fn render_container_menu(
        &self,
        selected: Option<SharedString>,
        containers: Vec<SharedString>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let label = selected
            .clone()
            .unwrap_or_else(|| SharedString::from("Container"));
        let aria_label = format!("Container: {label}");
        let panel = cx.entity().downgrade();
        PopoverMenu::new("dock-container-menu")
            .menu(move |window, cx| {
                let containers = containers.clone();
                let selected = selected.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    containers.into_iter().fold(menu, |menu, name| {
                        let toggled = selected.as_ref() == Some(&name);
                        let panel = panel.clone();
                        menu.toggleable_entry(
                            name.clone(),
                            toggled,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_container(name.clone(), cx))
                                    .ok();
                            },
                        )
                    })
                }))
            })
            .trigger(
                Button::new("dock-container", label)
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(4isize)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Select the container log source."))
                    .aria_label(aria_label),
            )
            .into_any_element()
    }

    /// The Retry control a failed stream offers. Both surfaces that can report a failure use this
    /// one element, so the tab order holds a single Retry and its meaning is the same in either
    /// place.
    fn render_log_retry(&self, cx: &Context<Self>) -> AnyElement {
        Button::new("dock-retry", "Retry")
            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .size(ButtonSize::Medium)
            .label_size(LabelSize::Custom(rems_from_px(f32::from(
                design::text::BODY,
            ))))
            .tab_index(9isize)
            .tooltip(Tooltip::text("Reconnect the log stream"))
            .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))
            .into_any_element()
    }

    /// Shows a recoverable log stream status, unless the log body is already showing it. The
    /// banner and the empty state are one entry point with two shapes, so a failure is never
    /// stated twice in the same view.
    fn render_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.log_body_carries_state() {
            return None;
        }
        let notice = self.log_failure_notice()?;
        let retry = notice.retry;
        Some(
            h_flex()
                .id("dock-log-banner")
                .debug_selector(|| "dock-log-banner".to_owned())
                .flex_none()
                .w_full()
                .h(design::size::ROW)
                .px(space::SM)
                .gap(space::XS)
                .items_center()
                .bg(cx.theme().colors().element_background)
                .child(status_message(
                    notice.severity,
                    notice.guidance,
                    notice.detail,
                    cx,
                ))
                .child(div().flex_1())
                .when(retry, |this| this.child(self.render_log_retry(cx)))
                .into_any_element(),
        )
    }

    fn render_logs(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let has_source = self.request.is_some() && self.factory.is_some();
        let body: AnyElement = if self.buffer.is_empty() {
            match self.log_failure_notice() {
                // With nothing to read, the body is the surface that reports the failure, so the
                // banner above it stays hidden instead of saying the same thing twice.
                Some(notice) => {
                    let retry = notice.retry.then(|| self.render_log_retry(cx));
                    empty_state_with_action(notice.icon, notice.title, notice.guidance, retry)
                }
                None => match &self.phase {
                    LogPhase::Idle => self.logs_unavailable_state(),
                    _ => empty_state(
                        IconName::LoadCircle,
                        "Loading logs",
                        "Waiting for the first log line…",
                    ),
                },
            }
        } else if self.visible_log_count() == 0 {
            empty_state(
                IconName::MagnifyingGlass,
                "No matching log lines",
                "Clear the filter to show the buffered logs.",
            )
        } else if self.log_body_is_too_short() {
            // The Dock was squeezed below a readable body. Half a row of text is worse than a
            // sentence that says what to do.
            empty_state(
                IconName::Warning,
                "Dock too short for log lines",
                "Drag the Dock divider up to read the log.",
            )
        } else if self.wrap {
            self.render_wrapped_list(window, cx)
        } else {
            self.render_nowrap_list(window, cx)
        };
        v_flex()
            .size_full()
            .min_h(px(0.))
            // The Terminal tab always draws its toolbar, so dropping the whole 40px band when
            // there is no log source moved the content's top edge by a full toolbar every time
            // the reader crossed between the two tabs. The band is isobaric: same height, same
            // surface, same hairline, one status line instead of the controls.
            .child(if has_source {
                self.render_toolbar(cx)
            } else {
                self.render_log_toolbar_shell(cx)
            })
            .children(self.render_banner(cx))
            .child(
                div()
                    .id("dock-log-body")
                    .debug_selector(|| "dock-log-body".to_owned())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .track_scroll(&self.log_body_handle)
                    .child(body),
            )
            .into_any_element()
    }

    /// The toolbar band the Logs tab shows when there is no log source to control.
    ///
    /// Same frame as [`Self::render_toolbar`]: `design::size::TOOLBAR`, the toolbar surface, and
    /// the same hairline, so crossing to the Terminal tab and back does not move the content.
    ///
    /// It also carries the `Open Logs` control, which is where every other log command already
    /// lives - Follow, Pause, Load More History - and the only place it fits. At
    /// `design::size::DOCK_MIN` the body under this band is 82px, and a centred empty state with a
    /// 28px control in it needs 114. Inside the body the control and the hint are the two
    /// flexible rows, so both would shrink: a 19px button above a zero-height sentence.
    fn render_log_toolbar_shell(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let status = if self.factory.is_some() {
            "No log target selected"
        } else {
            "No log source connected"
        };
        h_flex()
            .id("dock-log-toolbar-shell")
            .debug_selector(|| "dock-log-toolbar-shell".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(colors.toolbar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(label_small(status).color(Color::Muted).truncate()),
            )
            .child(self.render_open_logs_control(cx))
            .into_any_element()
    }

    /// The control that starts a log stream.
    ///
    /// It runs `k8s_shell::OpenLogs`, the same action the command palette and the keymap run. That
    /// action reports its own unavailability when nothing is selected, so the control is honest in
    /// every state: it either opens the stream or says which step is missing, instead of sending
    /// the reader after a menu that is not on screen.
    fn render_open_logs_control(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("dock-open-logs-control")
            .debug_selector(|| "dock-open-logs".to_owned())
            .flex_none()
            .child(
                Button::new("dock-open-logs", "Open Logs")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .track_focus(&self.open_logs_focus)
                    .tab_index(OPEN_LOGS_TAB_INDEX)
                    .tooltip(Tooltip::text(
                        "Stream the logs of the selected Pod. With nothing selected, this reports \
                         which step is missing.",
                    ))
                    .aria_label("Open Logs")
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(crate::shell::OpenLogs), cx);
                    })),
            )
            .into_any_element()
    }

    /// The empty state the Logs tab shows when nothing has been asked for yet.
    ///
    /// It used to be an icon, a title, a sentence naming a menu that is not on screen, and no
    /// control at all, on a surface whose sibling states ([`Self::terminal_empty_state`], the log
    /// failure state) both carry a real button. The hint now names the control that is on screen,
    /// and the control itself sits in the band above.
    fn logs_unavailable_state(&self) -> AnyElement {
        let hint = if self.request.is_some() {
            LOGS_NO_SOURCE_HINT
        } else {
            LOGS_NO_TARGET_HINT
        };
        empty_state(IconName::Reader, LOGS_NO_TARGET_TITLE, hint)
    }

    /// True when the measured log body cannot hold three rows, which is what the minimum Dock
    /// height leaves for it. Before the first layout the height is zero, and the list renders
    /// normally.
    fn log_body_is_too_short(&self) -> bool {
        let height = self.log_body_handle.bounds().size.height;
        height > px(0.) && height + dock_chrome_height() < design::size::DOCK_MIN
    }

    fn render_wrapped_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let buffer = self.buffer.clone();
        let state = self.list_state.clone();
        let font = buffer_font(cx);
        let visible = self.visible_log_indices.clone();
        let filtered = self.log_view_is_filtered();
        let compact = self.dock_is_compact(window);
        let timestamp_reserve = buffer.timestamp_column_reserve();
        let row_context = self.log_row_context(cx);
        div()
            .id("dock-log-scroll")
            .debug_selector(|| "dock-log-scroll".to_owned())
            .role(Role::List)
            .aria_label("Log lines")
            .size_full()
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.list_state),
                window,
                cx,
            )
            .restrict_scroll_to_axis()
            .child(
                list(state, move |row, _window, cx| {
                    let index = if filtered {
                        visible.get(row).copied()
                    } else {
                        Some(row)
                    };
                    match index.and_then(|index| buffer.line(index)) {
                        Some(line) => log_row(
                            row,
                            line,
                            LogRowGeometry {
                                wrap: true,
                                compact,
                                timestamp_reserve,
                            },
                            row_context.clone(),
                            &font,
                            cx,
                        ),
                        None => div().into_any_element(),
                    }
                })
                .size_full(),
            )
            .into_any_element()
    }

    fn render_nowrap_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let buffer = self.buffer.clone();
        let font = buffer_font(cx);
        let measure_row = self.longest_visible_log_row;
        let visible = self.visible_log_indices.clone();
        let filtered = self.log_view_is_filtered();
        let compact = self.dock_is_compact(window);
        let timestamp_reserve = buffer.timestamp_column_reserve();
        let row_context = self.log_row_context(cx);
        let list = uniform_list(
            "dock-log-lines",
            self.visible_log_count(),
            move |range, _window, cx| {
                range
                    .filter_map(|row| {
                        let index = if filtered {
                            visible.get(row).copied()
                        } else {
                            Some(row)
                        };
                        index.and_then(|index| buffer.line(index)).map(|line| {
                            log_row(
                                row,
                                line,
                                LogRowGeometry {
                                    wrap: false,
                                    compact,
                                    timestamp_reserve,
                                },
                                row_context.clone(),
                                &font,
                                cx,
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            },
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(measure_row))
        .track_scroll(&self.scroll_handle)
        .size_full();

        div()
            .id("dock-log-nowrap-scroll")
            .debug_selector(|| "dock-log-nowrap-scroll".to_owned())
            .role(Role::List)
            .aria_label("Log lines")
            .size_full()
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Both).tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
            .restrict_scroll_to_axis()
            .child(list)
            .into_any_element()
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.active_tab != 0 {
            return self.render_terminal(window, cx);
        }
        self.render_logs(window, cx)
    }

    /// Renders session controls, port forwards, and the active terminal.
    fn render_terminal(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let second = if self.terminal_split {
            self.terminals
                .iter()
                .enumerate()
                .find(|(index, _)| *index != self.active_terminal)
                .map(|(index, _)| index)
        } else {
            None
        };
        let body = if let Some(second) = second {
            let ratio = self
                .terminal_split_ratio
                .clamp(TERMINAL_SPLIT_MIN_RATIO, TERMINAL_SPLIT_MAX_RATIO);
            h_flex()
                .size_full()
                .min_h(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .flex_basis(relative(ratio))
                        .h_full()
                        .min_w(px(0.))
                        .child(self.render_terminal_pane(self.active_terminal, cx)),
                )
                .child(self.render_terminal_divider(window, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(self.render_terminal_pane(second, cx)),
                )
                .into_any_element()
        } else if self.terminals.get(self.active_terminal).is_some() {
            self.render_terminal_pane(self.active_terminal, cx)
        } else {
            self.terminal_empty_state(cx)
        };
        v_flex()
            .size_full()
            .min_h(px(0.))
            .overflow_hidden()
            .child(self.render_terminal_toolbar(window, cx))
            .when(!self.terminal_maximized, |this| {
                this.children(self.render_forwards(window, cx))
            })
            .child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body))
            .into_any_element()
    }

    /// The session view, with the boundary the terminal background needs.
    ///
    /// `terminal.background` is a surface role of its own and it does not go
    /// through `design::surface`, so nothing in the layer ramp states how it
    /// relates to the `panel` it sits in. Worse, the sign of that relation
    /// flips between appearances: the terminal is a hole *below* the Dock in
    /// dark and a card *above* it in light, at the same 2.9-3.2 L\* magnitude.
    /// The palette is not changed here, because the dim ANSI cells only clear
    /// `configured_minimum_contrast_reaches_each_ansi_cell` on this exact dark
    /// value (see `DESIGN.md` §9), so a rule states the relationship without
    /// spending a single point of the margin the palette has. The sign is
    /// documented, not enforced: `terminal_pane_is_bounded_by_a_rule` holds the
    /// boundary, and the two values are held apart in
    /// `the_terminal_surface_differs_from_the_dock_in_both_appearances`.
    fn render_terminal_pane(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        div()
            .id(("terminal-pane", index))
            .debug_selector(|| "terminal-pane".to_owned())
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(self.terminals[index].instance.view.clone())
            .into_any_element()
    }

    /// Divider between the two terminal panes. It matches the shell dividers: a hit area wider
    /// than the line, a splitter role, a focusable rail, and a keyboard resize.
    fn render_terminal_divider(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let dragging = self.terminal_split_drag.is_some();
        let focused = self.terminal_split_focus.is_focused(window);
        let ratio = self.terminal_split_ratio;
        let highlight = colors.border_focused;
        let line = if dragging || focused {
            colors.border_focused
        } else {
            colors.border
        };
        let body = self.dock_bounds();
        div()
            .id("terminal-split-divider")
            .debug_selector(|| "terminal-split-divider".to_owned())
            .role(Role::Splitter)
            .aria_label(TERMINAL_SPLIT_LABEL)
            .aria_description(
                "Press Left or Right to move the divider, Home for the smallest first pane, \
                 and End for the largest one.",
            )
            .aria_numeric_value(f64::from(ratio))
            .track_focus(&self.terminal_split_focus)
            .tab_stop(true)
            .tab_index(8isize)
            .focus_visible(|this| this.bg(highlight.opacity(0.12)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let step = if event.keystroke.modifiers.shift {
                    TERMINAL_SPLIT_KEY_STEP * 3.0
                } else {
                    TERMINAL_SPLIT_KEY_STEP
                };
                let next = match event.keystroke.key.as_str() {
                    "left" => ratio - step,
                    "right" => ratio + step,
                    "home" => TERMINAL_SPLIT_MIN_RATIO,
                    "end" => TERMINAL_SPLIT_MAX_RATIO,
                    _ => return,
                };
                this.set_terminal_split_ratio(next, cx);
                cx.stop_propagation();
            }))
            .flex_none()
            .cursor(CursorStyle::ResizeLeftRight)
            .flex()
            .justify_center()
            .items_center()
            .bg(if dragging {
                highlight.opacity(0.18)
            } else {
                line.opacity(0.0)
            })
            .hover(move |this| this.bg(highlight.opacity(0.12)))
            .active(move |this| this.bg(highlight.opacity(0.18)))
            .w(design::border::HIT)
            .h_full()
            .child(div().w(design::border::LINE).h_full().bg(line))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.terminal_split_focus, cx);
                    this.terminal_split_drag = Some(this.split_ratio_at(event.position.x, body));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if this.terminal_split_drag.is_none() {
                    return;
                }
                let next = this.split_ratio_at(event.position.x, body);
                this.set_terminal_split_ratio(next, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.terminal_split_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }

    /// Bounds the Dock was laid out with, which is also the split body before the divider.
    fn dock_bounds(&self) -> gpui::Bounds<Pixels> {
        self.width_handle.bounds()
    }

    /// Share of the body a pointer position asks for. A pointer outside the body clamps.
    fn split_ratio_at(&self, x: Pixels, body: gpui::Bounds<Pixels>) -> f32 {
        let width = f32::from(body.size.width).max(1.0);
        let offset = f32::from(x) - f32::from(body.origin.x);
        (offset / width).clamp(TERMINAL_SPLIT_MIN_RATIO, TERMINAL_SPLIT_MAX_RATIO)
    }

    fn set_terminal_split_ratio(&mut self, ratio: f32, cx: &mut Context<Self>) {
        let next = ratio.clamp(TERMINAL_SPLIT_MIN_RATIO, TERMINAL_SPLIT_MAX_RATIO);
        if (self.terminal_split_ratio - next).abs() <= f32::EPSILON {
            return;
        }
        self.terminal_split_ratio = next;
        cx.notify();
    }

    fn terminal_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let context = self.terminal_context_label();
        let hint = if let Some(context) = context {
            format!("{context} · Use New Terminal to open a local shell.")
        } else if self.terminal_available() {
            "Use New Terminal to open a local shell for the current context.".to_owned()
        } else {
            "Select a context to open a terminal.".to_owned()
        };
        let action = self.terminal_available().then(|| {
            div()
                .debug_selector(|| "terminal-add".to_owned())
                .child(
                    Button::new("terminal-empty-add", "New Terminal")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .size(ButtonSize::Medium)
                        .label_size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::BODY,
                        ))))
                        .track_focus(&self.terminal_add_focus)
                        .tab_index(5isize)
                        .tooltip(Tooltip::text(
                            "Open a local shell with the current context.",
                        ))
                        .aria_label("New Terminal")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.new_local_terminal(window, cx)),
                        ),
                )
                .into_any_element()
        });
        div()
            .id("terminal-empty-state")
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .child(empty_state_with_action(
                IconName::Terminal,
                "No terminal session",
                hint,
                action,
            ))
            .into_any_element()
    }

    fn render_terminal_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let compact = self.dock_is_compact(window);
        let context = self
            .terminal_services
            .as_ref()
            .and_then(|services| services.context.clone());
        let bar = h_flex()
            .id("terminal-toolbar")
            .debug_selector(|| "terminal-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(colors.toolbar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border_variant);
        if self.terminal_services.is_none() && self.terminals.is_empty() {
            return bar
                .child(
                    label_small("Select a context to open a terminal.")
                        .color(Color::Muted)
                        .truncate(),
                )
                .into_any_element();
        }
        let focus_handles = self.terminal_focus_handles.clone();
        let mut sessions = h_flex()
            .id("terminal-sessions")
            .gap(space::XS)
            .role(Role::TabList)
            .aria_label(TERMINAL_SESSION_TAB_LIST_LABEL);
        if self.terminals.len() > 1 {
            for (index, entry) in self.terminals.iter().enumerate() {
                let title = terminal_entry_title(entry, context.as_deref());
                let active = index == self.active_terminal;
                let Some(focus) = focus_handles.get(index).cloned() else {
                    continue;
                };
                let aria_title = format!("Terminal session: {title}");
                // The verdict rides in the accessible name too, so the state the chip now shows
                // is not only a shape.
                let aria_title = format!(
                    "{aria_title}. {}",
                    session_verdict_label(entry.exit.as_ref())
                );
                let aria_description = "Press Enter to activate. Press Delete to close.";
                let handles = focus_handles.clone();
                // Resolved before the builder chain, so the chip's own handlers keep the context.
                let verdict = session_verdict(entry.exit.as_ref())
                    .map(|severity| (design::health_icon(severity), severity.marker(cx)));
                let mut chip = div()
                    .id(("terminal-chip", index))
                    .debug_selector(move || format!("terminal-chip-{index}"))
                    .h(design::size::CONTROL)
                    .w(design::size::MAIN_CONTENT_MIN)
                    .px(space::SM)
                    .gap(space::XS)
                    .flex_none()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_transparent)
                    .hover(|this| this.bg(colors.element_hover))
                    .active(|this| this.bg(colors.element_active))
                    .when(active, |this| this.bg(colors.element_selected))
                    .cursor_pointer()
                    .track_focus(&focus)
                    .tab_index(3isize)
                    .tab_stop(active)
                    .focus_visible(|this| this.border_color(colors.border_focused))
                    .role(Role::Tab)
                    .aria_label(aria_title.clone())
                    .aria_description(aria_description)
                    .accessibility_id(format!("terminal-session-{index}"))
                    .aria_keyshortcuts("Enter Delete Backspace ArrowLeft ArrowRight Home End")
                    .aria_selected(active)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_terminal(index, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        this.context_stack_ready.set(true);
                        let count = handles.len();
                        if count == 0 {
                            return;
                        }
                        let target = match event.keystroke.key.as_str() {
                            "enter" | "return" | "space" => {
                                this.activate_terminal(index, window, cx);
                                cx.stop_propagation();
                                return;
                            }
                            "delete" | "backspace" => {
                                this.close_terminal(index, window, cx);
                                cx.stop_propagation();
                                return;
                            }
                            "left" | "up" => index.checked_sub(1).unwrap_or(count - 1),
                            "right" | "down" => (index + 1) % count,
                            "home" => 0,
                            "end" => count - 1,
                            _ => return,
                        };
                        this.activate_terminal(target, window, cx);
                        if let Some(handle) = handles.get(target) {
                            window.focus(handle, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::XSmall)
                            .color(if active { Color::Default } else { Color::Muted }),
                    )
                    // The verdict slot. It is always reserved, so a session that ends does not
                    // slide the title sideways, and it stays empty while the Dock has no verdict
                    // rather than claiming a healthy state it has not observed.
                    .child(
                        div()
                            .id(("terminal-chip-verdict", index))
                            .debug_selector(move || format!("terminal-chip-verdict-{index}"))
                            .flex_none()
                            .w(design::size::STATUS_MARKER)
                            .h(design::size::STATUS_MARKER)
                            .items_center()
                            .when_some(verdict, |this, (icon, marker)| {
                                this.child(
                                    Icon::new(icon)
                                        .size(IconSize::Custom(rems_from_px(f32::from(
                                            design::size::STATUS_MARKER,
                                        ))))
                                        .color(Color::Custom(marker)),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(label_text(title).truncate()),
                    );
                chip.interactivity().tooltip(command_tooltip(aria_title));
                sessions = sessions.child(chip);
            }
        }
        let maximize_label = if self.terminal_maximized {
            "Restore Terminal"
        } else {
            "Maximize Terminal"
        };
        let mut commands = h_flex().flex_none().gap(space::XS).items_center();
        if self.terminal_available() && !self.terminals.is_empty() {
            commands = commands.child(
                IconButton::new("terminal-add", IconName::Plus)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .tab_index(5isize)
                    .track_focus(&self.terminal_add_focus)
                    .tooltip(command_tooltip(
                        "Open a local shell with the current context.",
                    ))
                    .aria_label("New Terminal")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.new_local_terminal(window, cx)),
                    ),
            );
        }
        if (!compact || self.terminal_maximized) && !self.terminals.is_empty() {
            commands = commands.child(
                IconButton::new(
                    "terminal-maximize",
                    if self.terminal_maximized {
                        IconName::GenericRestore
                    } else {
                        IconName::GenericMaximize
                    },
                )
                .size(ButtonSize::Medium)
                .icon_size(IconSize::XSmall)
                .tab_index(7isize)
                .tooltip(command_tooltip(maximize_label))
                .aria_label(maximize_label)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_maximized(cx))),
            );
        }
        if self.terminal_available() || !self.terminals.is_empty() {
            commands = commands.child(
                div()
                    .flex_none()
                    .debug_selector(|| "terminal-actions-trigger".to_owned())
                    .child(self.render_terminal_menu(cx)),
            );
        }
        let sessions_area = if self.terminals.len() > 1 {
            div()
                .id("terminal-session-scroll")
                .flex_1()
                .min_w(px(0.))
                .overflow_x_scroll()
                .overflow_y_hidden()
                .restrict_scroll_to_axis()
                .track_scroll(&self.terminal_session_scroll)
                .child(sessions)
                .into_any_element()
        } else if let Some(entry) = self.terminals.get(self.active_terminal) {
            let title = terminal_entry_title(entry, context.as_deref());
            let mut target = div()
                .id("terminal-target")
                .debug_selector(|| "terminal-target".to_owned())
                .flex_1()
                .min_w(px(0.))
                .child(label_text(title.clone()).truncate());
            target.interactivity().tooltip(Tooltip::text(title));
            target.into_any_element()
        } else if let Some(context) = self.terminal_context_label() {
            let mut target = div()
                .id("terminal-context")
                .debug_selector(|| "terminal-context".to_owned())
                .flex_1()
                .min_w(px(0.))
                .child(label_text(context.clone()).truncate());
            target.interactivity().tooltip(Tooltip::text(context));
            target.into_any_element()
        } else {
            div().flex_1().min_w(px(0.)).into_any_element()
        };
        bar.child(sessions_area)
            .when_some(
                self.terminals.get(self.active_terminal).and_then(|entry| {
                    let exit = entry.exit.clone()?;
                    let recovery = "Select Restart to open a new session.";
                    Some((
                        SharedString::from("Terminal Session Ended"),
                        format!("{exit}. {recovery}"),
                    ))
                }),
                |this, (message, detail)| {
                    this.child(status_message(Severity::Warning, message, Some(detail), cx))
                },
            )
            // A maximised terminal hides the tab bar, so the stream state moves into the toolbar
            // that stays on screen.
            .when(
                self.terminal_maximized && self.header_status_is_useful(),
                |this| this.child(self.render_status_chip(cx)),
            )
            .child(commands)
            .into_any_element()
    }

    fn render_terminal_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity().downgrade();
        let has_terminal = !self.terminals.is_empty();
        let has_service = self.terminal_available();
        let maximized = self.terminal_maximized;
        let active_terminal = self.active_terminal;
        let can_restart = !self.terminals.is_empty();
        let context = self
            .terminal_services
            .as_ref()
            .and_then(|services| services.context.as_deref());
        let active_title = self
            .terminals
            .get(active_terminal)
            .map(|entry| terminal_entry_title(entry, context))
            .unwrap_or_else(|| "Terminal".into());
        PopoverMenu::new("terminal-actions")
            .menu(move |window, cx| {
                let panel = panel.clone();
                let active_title = active_title.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let mut menu = menu;
                    if has_service {
                        let new_panel = panel.clone();
                        menu = menu.item(
                            ContextMenuEntry::new("New Terminal")
                                .icon(IconName::Plus)
                                .handler(move |window, cx| {
                                    new_panel
                                        .update(cx, |panel, cx| {
                                            panel.new_local_terminal(window, cx)
                                        })
                                        .ok();
                                }),
                        );
                    }
                    if has_terminal {
                        let restart_panel = panel.clone();
                        let restart_title = active_title.clone();
                        if can_restart {
                            menu = menu.item(
                                ContextMenuEntry::new(format!("Restart {restart_title}"))
                                    .icon(IconName::RotateCw)
                                    .handler(move |window, cx| {
                                        restart_panel
                                            .update(cx, |panel, cx| {
                                                panel.restart_terminal(active_terminal, window, cx)
                                            })
                                            .ok();
                                    }),
                            );
                        }
                        let split_panel = panel.clone();
                        let maximize_panel = panel.clone();
                        menu = menu
                            .item(
                                ContextMenuEntry::new("Split Terminal")
                                    .icon(IconName::Split)
                                    .handler(move |window, cx| {
                                        split_panel
                                            .update(cx, |panel, cx| {
                                                panel.split_terminal(window, cx)
                                            })
                                            .ok();
                                    }),
                            )
                            .item(
                                ContextMenuEntry::new(if maximized {
                                    "Restore Terminal"
                                } else {
                                    "Maximize Terminal"
                                })
                                .icon(if maximized {
                                    IconName::GenericRestore
                                } else {
                                    IconName::GenericMaximize
                                })
                                .toggleable(IconPosition::Start, maximized)
                                .handler(move |_, cx| {
                                    maximize_panel
                                        .update(cx, |panel, cx| panel.toggle_terminal_maximized(cx))
                                        .ok();
                                }),
                            );
                        let close_panel = panel.clone();
                        menu = menu.item(
                            ContextMenuEntry::new("Close Terminal")
                                .icon(IconName::Close)
                                .handler(move |window, cx| {
                                    close_panel
                                        .update(cx, |panel, cx| {
                                            panel.close_terminal(active_terminal, window, cx)
                                        })
                                        .ok();
                                }),
                        );
                    }
                    menu
                }))
            })
            .trigger(
                Button::new("terminal-actions-trigger", "Actions")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .start_icon(Icon::new(IconName::Menu).size(IconSize::XSmall))
                    .tab_index(8isize)
                    .tooltip(command_tooltip("Terminal actions"))
                    .aria_label("Terminal actions"),
            )
            .into_any_element()
    }

    /// Renders active port forwards and their controls.
    fn render_forwards(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.forwards.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let strip = v_flex()
            .id("dock-forwards")
            .debug_selector(|| "dock-forwards-strip".to_owned())
            .role(Role::Region)
            .aria_label("Port Forwards")
            .flex_none()
            .w_full()
            .border_b_1()
            .border_color(colors.border_variant);
        let mut list = v_flex()
            .id("dock-forwards-list")
            .debug_selector(|| "dock-forwards-list".to_owned())
            .role(Role::List)
            .aria_label("Port forwards")
            .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
            .track_focus(&self.forwards_focus)
            .tab_index(10isize)
            // The rail is always reserved, so focusing the strip does not move the rows in it.
            .border_l(design::border::FOCUS_RAIL)
            .border_color(colors.border_transparent)
            .focus_visible(|style| style.border_color(colors.border_focused))
            .on_key_down(cx.listener(Self::on_forwards_key_down))
            .flex_1()
            .min_h(px(0.))
            .max_h(px(f32::from(design::size::ROW) * FORWARDS_VISIBLE_ROWS))
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.forwards_scroll);
        for entry in &self.forwards {
            let id = entry.id;
            let id_value = id.0;
            let target = forward_row_target(entry);
            let accessible_target = target.replace(" → ", " to ");
            let (icon, icon_color, status, aria_status, next_step) = match entry.phase {
                ForwardPhase::Stopped => (
                    IconName::Stop,
                    Color::Muted,
                    "Stopped",
                    "stopped",
                    "Select Start to run this forward again.",
                ),
                ForwardPhase::Starting => (
                    IconName::LoadCircle,
                    Color::Custom(Severity::Warning.marker(cx)),
                    "Starting…",
                    "starting",
                    "Select Stop to cancel startup.",
                ),
                ForwardPhase::Running => (
                    IconName::ArrowRightLeft,
                    Color::Custom(Severity::Success.marker(cx)),
                    "Forwarding",
                    "running",
                    "Select Stop to end this forward.",
                ),
                ForwardPhase::Stopping => (
                    IconName::LoadCircle,
                    Color::Custom(Severity::Warning.marker(cx)),
                    "Stopping…",
                    "stopping",
                    "Wait for the port forward to stop.",
                ),
                ForwardPhase::Failed => (
                    IconName::Warning,
                    Color::Custom(Severity::Error.marker(cx)),
                    "Failed",
                    "failed",
                    "Select Retry to reconnect.",
                ),
            };
            // The raw reason belongs in the spoken row and the tooltip. The row itself keeps the
            // state and the next step, which is what the user acts on.
            let detail = entry
                .error
                .clone()
                .map(|error| error.trim().to_owned())
                .filter(|error| !error.is_empty());
            let aria_label = forward_row_aria(entry, aria_status, next_step);
            let description = detail.clone().unwrap_or_else(|| next_step.to_owned());
            let mut content = h_flex()
                .id(("dock-forward-detail", id_value))
                .flex_1()
                .min_w(px(0.))
                .gap(space::SM)
                .items_center()
                .aria_description(description)
                .child(
                    label_small(target)
                        .flex_1()
                        .color(Color::Default)
                        .truncate(),
                )
                .child(label_small(status).color(Color::Muted));
            content
                .interactivity()
                .tooltip(Tooltip::text(detail.unwrap_or_else(|| aria_label.clone())));
            let action: AnyElement = match entry.phase {
                ForwardPhase::Stopped => Button::new(("dock-forward-start", id_value), "Start")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(9isize)
                    .tooltip(Tooltip::text("Start this port forward again"))
                    .aria_label(format!("Start port forward for {accessible_target}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = this.restart_forward(id, cx);
                    }))
                    .into_any_element(),
                ForwardPhase::Starting | ForwardPhase::Running => {
                    let stop_label = if entry.phase == ForwardPhase::Starting {
                        "Cancel starting port forward".to_owned()
                    } else {
                        forward_live_port(entry)
                            .map(|port| format!("Stop forwarding localhost:{port}"))
                            .unwrap_or_else(|| "Stop port forward".to_owned())
                    };
                    IconButton::new(("dock-forward-stop", id_value), IconName::Stop)
                        .size(ButtonSize::Medium)
                        .icon_size(IconSize::XSmall)
                        .tab_index(9isize)
                        .tooltip(command_tooltip(stop_label))
                        .aria_label(format!("Stop port forward for {accessible_target}"))
                        .on_click(cx.listener(move |this, _, _, cx| this.stop_forward(id, cx)))
                        .into_any_element()
                }
                ForwardPhase::Stopping => {
                    IconButton::new(("dock-forward-stop", id_value), IconName::Stop)
                        .size(ButtonSize::Medium)
                        .icon_size(IconSize::XSmall)
                        .tab_index(9isize)
                        .disabled(true)
                        .tooltip(command_tooltip("Waiting for port forward to stop"))
                        .aria_label(format!("Stopping port forward for {accessible_target}"))
                        .into_any_element()
                }
                ForwardPhase::Failed => Button::new(("dock-forward-retry", id_value), "Retry")
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .tab_index(9isize)
                    .tooltip(Tooltip::text("Retry this port forward"))
                    .aria_label(format!("Retry port forward for {accessible_target}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = this.retry_forward(id, cx);
                    }))
                    .into_any_element(),
            };
            list = list.child(
                ListItem::new(("dock-forward", id_value))
                    .aria_role(Role::ListItem)
                    .aria_label(aria_label)
                    .height(design::size::ROW)
                    .spacing(ListItemSpacing::ExtraDense)
                    .selectable(false)
                    .start_slot(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
                    .child(content)
                    .end_slot(action),
            );
        }
        Some(
            strip
                .child(
                    list.custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Vertical)
                            .tracked_scroll_handle(&self.forwards_scroll),
                        window,
                        cx,
                    ),
                )
                .into_any_element(),
        )
    }

    /// Scrolls the forward list from the keyboard. The list owns its scroll, so the log list and
    /// the terminal keep their positions.
    fn on_forwards_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.forwards_focus.is_focused(window)
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        if !matches!(key, "up" | "down" | "pageup" | "pagedown" | "home" | "end") {
            return;
        }
        cx.stop_propagation();
        let last = self.forwards.len().saturating_sub(1);
        let top = self.forwards_scroll.top_item();
        let bottom = self.forwards_scroll.bottom_item();
        let page = ((f32::from(self.forwards_page_height()) / f32::from(design::size::ROW)).floor()
            as usize)
            .max(1);
        let (target, to_top) = match key {
            "up" => (top.checked_sub(1), false),
            "down" => (Some((bottom + 1).min(last)), false),
            "pageup" => (Some(top.saturating_sub(page)), true),
            "pagedown" => (Some((bottom + 1).min(last)), true),
            "home" => (Some(0), true),
            _ => (Some(last), true),
        };
        let Some(target) = target else {
            return;
        };
        if to_top {
            self.forwards_scroll.scroll_to_top_of_item(target);
        } else {
            self.forwards_scroll.scroll_to_item(target);
        }
        cx.notify();
    }

    fn forwards_page_height(&self) -> Pixels {
        let height = self.forwards_scroll.bounds().size.height;
        if height > px(0.) {
            height
        } else {
            px(f32::from(design::size::ROW) * FORWARDS_VISIBLE_ROWS)
        }
    }

    fn log_viewport_height(&self, row_height: Pixels) -> Pixels {
        let height = if self.wrap {
            self.list_state.viewport_bounds().size.height
        } else {
            self.scroll_handle
                .0
                .borrow()
                .base_handle
                .bounds()
                .size
                .height
        };
        if height > px(0.) {
            height
        } else {
            row_height * 10.
        }
    }

    fn scroll_log_by(&mut self, distance: Pixels) {
        if self.wrap {
            self.list_state.scroll_by(distance);
            if distance > px(0.)
                && self.follow
                && self.list_state.is_scrolled_to_end() == Some(true)
            {
                self.list_state.set_follow_mode(gpui::FollowMode::Tail);
            }
            return;
        }
        let mut state = self.scroll_handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        let max_offset = state.base_handle.max_offset().y;
        let next_y = (offset.y + distance).clamp(-max_offset, px(0.));
        state.base_handle.set_offset(gpui::point(offset.x, next_y));
    }

    fn scroll_log_to_top(&mut self) {
        if self.wrap {
            self.list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        } else {
            let mut state = self.scroll_handle.0.borrow_mut();
            state.deferred_scroll_to_item = None;
            state
                .base_handle
                .set_offset(gpui::point(state.base_handle.offset().x, px(0.)));
        }
        self.following = false;
    }

    fn scroll_log_to_end(&mut self) {
        if self.wrap {
            self.list_state.scroll_to_end();
            if self.follow {
                self.list_state.set_follow_mode(gpui::FollowMode::Tail);
            }
        } else {
            self.scroll_handle.0.borrow_mut().deferred_scroll_to_item = None;
            self.scroll_handle.scroll_to_bottom();
        }
        self.following = self.follow;
    }

    /// Keeps the selection inside the visible rows after the buffer or the filter changed.
    fn clamp_log_selection(&mut self) {
        let last = self.visible_log_count().saturating_sub(1);
        if let Some(selection) = self.log_selection.as_mut() {
            selection.anchor = selection.anchor.min(last);
            selection.head = selection.head.min(last);
        }
    }

    /// Buffer index of a visible row, or `None` when the row has no line behind it.
    fn log_row_index(&self, row: usize) -> Option<usize> {
        if self.log_view_is_filtered() {
            self.visible_log_indices.get(row).copied()
        } else {
            self.buffer.line(row).map(|_| row)
        }
    }

    /// Moves the caret to `row`, dropping any range. A plain arrow press collapses the selection.
    fn focus_log_row(&mut self, row: usize, row_height: Pixels) {
        let row = row.min(self.visible_log_count().saturating_sub(1));
        self.log_selection = Some(LogSelection {
            anchor: row,
            head: row,
        });
        self.scroll_log_row_into_view(row, row_height);
    }

    /// Moves the caret to `target`, keeping `previous`'s anchor so Shift extends the range. The
    /// default selection starts at row zero, so Shift+Down extends from the row the caret would
    /// have been on instead of swallowing it.
    fn extend_log_row(&mut self, previous: LogSelection, target: usize, row_height: Pixels) {
        let target = target.min(self.visible_log_count().saturating_sub(1));
        self.log_selection = Some(LogSelection {
            anchor: previous.anchor,
            head: target,
        });
        self.scroll_log_row_into_view(target, row_height);
    }

    /// Scrolls the minimum amount that brings `row` into the viewport.
    fn scroll_log_row_into_view(&mut self, row: usize, row_height: Pixels) {
        let page = self.log_viewport_height(row_height);
        let rows = (f32::from(page) / f32::from(row_height)).floor().max(1.0) as usize;
        if self.wrap {
            let top = self.list_state.logical_scroll_top().item_ix;
            if row < top {
                self.list_state.scroll_to(ListOffset {
                    item_ix: row,
                    offset_in_item: px(0.),
                });
            } else if row >= top + rows {
                self.list_state.scroll_to(ListOffset {
                    item_ix: row + 1 - rows,
                    offset_in_item: px(0.),
                });
            }
            return;
        }
        let state = self.scroll_handle.0.borrow_mut();
        let offset = state.base_handle.offset();
        let max_offset = state.base_handle.max_offset().y;
        // Scroll offsets are `Pixels`, but the row arithmetic is easier to follow in `f32`, so
        // every measurement is converted once here and only the result goes back as a `Pixels`.
        let row_height = f32::from(row_height);
        let page = f32::from(page);
        let row_top = row as f32 * row_height;
        let viewport_top = -f32::from(offset.y);
        let next_y = if row_top < viewport_top {
            -row_top
        } else if row_top + row_height > viewport_top + page {
            viewport_top + page - row_top - row_height
        } else {
            f32::from(offset.y)
        };
        state.base_handle.set_offset(gpui::point(
            offset.x,
            px(next_y.clamp(-f32::from(max_offset), 0.)),
        ));
    }

    /// Raw text of the selected rows, one line each, or `None` when nothing is selected.
    fn log_selection_text(&self) -> Option<String> {
        let selection = self.log_selection?;
        let (start, end) = selection.bounds();
        let mut text = String::new();
        for row in start..=end {
            let Some(index) = self.log_row_index(row) else {
                continue;
            };
            let Some(line) = self.buffer.line(index) else {
                continue;
            };
            text.push_str(&line.raw);
            text.push('\n');
        }
        (!text.is_empty()).then_some(text)
    }

    /// Copies the selection. The chord is the one every text field uses, so a short log line is
    /// copyable without a pointer and without reaching for the per-line button.
    fn copy_log_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.log_selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Handles the log list from the keyboard: the editing chord first, then the selection keys,
    /// then the plain scrolling keys.
    fn on_log_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_tab != 0 || !self.focus_handle.is_focused(window) {
            return;
        }
        // One row count for the whole key: the pass that scores the filter, the rebase that
        // rebases it, and a batch from the stream can all move it while a key is being handled.
        let visible = self.visible_log_count();
        // The navigation keys belong to the log list while the Dock has the keyboard, with rows or
        // without. A failed or empty stream has no rows to move, and letting the arrows through
        // then moves the resource table behind the Dock, which is the opposite of what the key
        // says. Every other key stays available to the rest of the app.
        if visible == 0 {
            if is_log_navigation_key(event.keystroke.key.as_str()) {
                cx.stop_propagation();
            }
            return;
        }
        if is_log_copy_chord(&event.keystroke) {
            self.copy_log_selection(cx);
            cx.stop_propagation();
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        // Every measurement in this handler is one row, and the row is the reader's configured
        // data line, so a page of arrows moves the same number of lines whatever they set.
        let row_height = log_row_height(cx);
        let page = self.log_viewport_height(row_height);
        let last = visible.saturating_sub(1);
        if modifiers.shift {
            let rows = (f32::from(page) / f32::from(row_height)).floor().max(1.0) as usize;
            let previous = self.log_selection.unwrap_or_default();
            let target = match event.keystroke.key.as_str() {
                "up" => previous.head.saturating_sub(1),
                "down" => (previous.head + 1).min(last),
                "pageup" => previous.head.saturating_sub(rows),
                "pagedown" => (previous.head + rows).min(last),
                "home" => 0,
                "end" => last,
                _ => return,
            };
            self.extend_log_row(previous, target, row_height);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let handled = match event.keystroke.key.as_str() {
            "up" => {
                self.scroll_log_by(-row_height);
                true
            }
            "down" => {
                self.scroll_log_by(row_height);
                true
            }
            "pageup" => {
                self.scroll_log_by(-page);
                true
            }
            "pagedown" => {
                self.scroll_log_by(page);
                true
            }
            "home" => {
                self.scroll_log_to_top();
                true
            }
            "end" => {
                self.scroll_log_to_end();
                true
            }
            _ => false,
        };
        if handled {
            self.sync_scroll_follow();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn log_copy_action(&mut self, _: &CopyAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tab != 0 || !self.focus_handle.is_focused(window) {
            return;
        }
        self.copy_log_selection(cx);
        cx.stop_propagation();
    }

    /// Runs the same restart as the `Restart` entry in the Terminal actions menu, on behalf of the
    /// `Restart` control in a failed session's own state panel.
    ///
    /// That panel is implemented in `k8s-app` and cannot name a Dock method, so it dispatches this
    /// action instead. The session it restarts is the one the Dock is showing, which is the only
    /// session whose state panel can be on screen.
    fn restart_terminal_action(
        &mut self,
        _: &RestartTerminalSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_tab != 1 || self.active_terminal >= self.terminals.len() {
            return;
        }
        let index = self.active_terminal;
        self.restart_terminal(index, window, cx);
        cx.stop_propagation();
    }

    /// Whether the log list has rows to read and scroll.
    fn log_list_has_lines(&self) -> bool {
        self.visible_log_count() > 0
    }

    /// Spoken state of the selection, or `None` while the list has no caret.
    fn log_selection_aria(&self) -> Option<String> {
        let selection = self.log_selection?;
        let (start, end) = selection.bounds();
        // One count for the whole sentence, so the range and the total cannot disagree.
        let visible = self.visible_log_count();
        if !selection.is_range() {
            return Some(format!("Log line {} of {}.", selection.head + 1, visible));
        }
        Some(format!(
            "Log lines {} to {} selected, {} of {} lines.",
            start + 1,
            end + 1,
            visible,
            self.buffer.len()
        ))
    }

    /// Handles a pointer press on a row: it moves the caret, and Shift extends the range.
    fn on_log_row_click(&mut self, row: usize, extend: bool, cx: &mut Context<Self>) {
        if row >= self.visible_log_count() {
            return;
        }
        let row_height = log_row_height(cx);
        match (extend, self.log_selection) {
            (true, Some(previous)) => self.extend_log_row(previous, row, row_height),
            _ => self.focus_log_row(row, row_height),
        }
        cx.notify();
    }

    /// Updates follow state from the active scroll owner.
    ///
    /// The wrapped list reports its own tail state through `list_follow_report`, and the report is
    /// consumed here, at a point where the list holds no borrow. The uniform list owns the scroll
    /// in no-wrap mode and has no report to leave, so its handle is read directly. Either way the
    /// owner is the only source, so a report cannot go stale behind the state it describes.
    fn sync_scroll_follow(&mut self) {
        let reported = self.list_follow_report.replace(None);
        let owner_follows = if self.wrap {
            reported.unwrap_or_else(|| self.list_state.is_following_tail())
        } else {
            self.uniform_list_at_bottom()
        };
        self.following = self.follow && owner_follows;
    }
}

impl Render for DockPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.log_filter_observation.is_none() {
            let input = self.log_filter_input.clone();
            self.log_filter_observation = Some(cx.observe(&input, |panel, input, cx| {
                let query = input.read(cx).text().to_owned();
                panel.set_log_filter(&query, cx);
            }));
        }
        if self.active_tab == 0 {
            self.sync_scroll_follow();
        }
        // The filter pass continues here, one bounded budget per frame, so a burst of log lines
        // never turns into one full scan of the buffer per batch. The pass only asks for the next
        // frame while its own budget lasts: a live stream outruns the pass, so an unbounded ask
        // would keep the window redrawing long after the stream could catch up.
        if self.active_tab == 0 {
            self.continue_filter_pass(cx);
        }
        self.sync_focus_handles();
        let colors = cx.theme().colors();
        let selection_aria = self.log_selection_aria();
        v_flex()
            .id("dock-panel")
            .debug_selector(|| "dock-panel".to_owned())
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(colors.panel_background.alpha(1.0))
            .text_color(colors.text)
            .key_context("Dock")
            .track_scroll(&self.width_handle)
            .when(!self.terminal_maximized, |this| {
                this.child(self.render_tabs(window, cx))
            })
            .child(
                div()
                    .id("dock-content")
                    .debug_selector(|| "dock-content".to_owned())
                    .role(Role::TabPanel)
                    .aria_label(tab_panel_label(self.active_tab, self.terminals.len()))
                    .accessibility_id(format!("dock-panel-{}", self.active_tab))
                    .when(self.active_tab == 0 && self.log_list_has_lines(), |this| {
                        this.aria_description(match selection_aria {
                            Some(selection) => format!(
                                "Use Up, Down, Page Up, Page Down, Home, and End to scroll logs. \
                                 Use Shift with those keys to select a range, then Copy to copy it. \
                                 {selection}"
                            ),
                            None => "Use Up, Down, Page Up, Page Down, Home, and End to scroll logs. \
                                     Use Shift with those keys to select a range, then Copy to copy it."
                                .to_owned(),
                        })
                        .aria_keyshortcuts(
                            "ArrowUp ArrowDown PageUp PageDown Home End \
                             Shift+ArrowUp Shift+ArrowDown Control+c",
                        )
                    })
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .track_focus(&self.focus_handle)
                    .tab_index(20isize)
                    // The rail is always reserved, so focusing the tab content does not move the
                    // log rows and the terminal inside it.
                    .border_l(design::border::FOCUS_RAIL)
                    .border_color(colors.border_transparent)
                    .focus_visible(|style| style.border_color(colors.border_focused))
                    .on_action(cx.listener(Self::log_copy_action))
                    .on_action(cx.listener(Self::restart_terminal_action))
                    .on_key_down(cx.listener(Self::on_log_key_down))
                    .child(self.render_content(window, cx)),
            )
    }
}

/// What the list knows about a row: the selection it belongs to, and the panel that owns it.
#[derive(Clone)]
struct LogRowContext {
    selection: Option<LogSelection>,
    panel: gpui::WeakEntity<DockPanel>,
}

/// The shape the list draws a row in: wrap or no wrap, the width the Dock spends on chrome, and
/// the columns the buffer reserves for timestamps. The list picks one shape per pass, so the row
/// takes it whole instead of three loose flags.
#[derive(Clone, Copy)]
struct LogRowGeometry {
    wrap: bool,
    compact: bool,
    timestamp_reserve: usize,
}

fn log_row(
    row_id: usize,
    line: LogLine,
    geometry: LogRowGeometry,
    row_context: LogRowContext,
    font: &Font,
    cx: &App,
) -> AnyElement {
    let LogRowGeometry {
        wrap,
        compact,
        timestamp_reserve,
    } = geometry;
    let colors = cx.theme().colors();
    // One read of the configured data role, so the glyph, the line the row is tall enough for and
    // every column the row reserves come from the same setting.
    let typography = crate::settings::data_typography(cx);
    let row_height = typography.line_height;
    let marker = line.severity.marker(cx);
    let severity = line.severity;
    let timestamp = line.timestamp.clone().unwrap_or_default();
    let label = line.label.clone();
    let display_message = line.display_message().clone();
    let copy_text = line.raw.as_ref().to_owned();
    // A long line shows less than the copy button puts on the clipboard, so the control says so.
    let long = line.display_columns() > LOG_COPY_COLUMN_THRESHOLD;
    let compact_wrap = wrap && compact;
    let selected = row_context
        .selection
        .is_some_and(|selection| selection.contains(row_id));
    let focused = row_context
        .selection
        .is_some_and(|selection| selection.head == row_id);
    let row_aria = format!(
        "{} {} {}",
        if timestamp.is_empty() {
            ""
        } else {
            timestamp.as_ref()
        },
        label,
        display_message
    );
    let mut row = if compact_wrap {
        v_flex().id(("dock-log-row", row_id))
    } else {
        h_flex().id(("dock-log-row", row_id))
    }
    .debug_selector(|| "dock-log-row".to_owned())
    .flex_none()
    .min_w(px(0.))
    .px(space::SM)
    .font(font.clone())
    .font_features(typography.features.clone())
    .text_size(rems_from_px(f32::from(typography.size)))
    .line_height(rems_from_px(f32::from(typography.line_height)))
    .role(Role::ListItem)
    .aria_label(row_aria)
    .aria_selected(selected)
    // Only the caret row is addressable, so a frame does not build one id per visible row.
    .when(focused, |this| {
        this.accessibility_id(format!("dock-log-row-{row_id}"))
    })
    // The rail is always reserved, so selecting a row does not move the text next to it.
    .border_l(design::border::FOCUS_RAIL)
    .border_color(if focused {
        colors.border_focused
    } else {
        colors.border_transparent
    });
    if selected {
        row = row.bg(colors.element_selected);
    }
    if compact_wrap {
        row = row.w_full().min_h(row_height).items_start().gap(space::XS);
    } else if wrap {
        row = row.w_full().items_start().gap(space::SM);
    } else {
        row = row
            .w(log_row_width(
                &line,
                compact,
                timestamp_reserve,
                &typography,
            ))
            .h(row_height)
            .items_center()
            .gap(space::SM);
    }
    let timestamp_width = log_timestamp_width(&line, timestamp_reserve, &typography);
    let mut timestamp_cell = div()
        .debug_selector(|| "dock-log-timestamp".to_owned())
        .flex_none()
        .w(timestamp_width)
        .h(row_height)
        .items_center()
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .text_color(colors.text_muted)
        .child(timestamp.clone());
    if !timestamp.is_empty() {
        timestamp_cell
            .interactivity()
            .tooltip(Tooltip::text(timestamp.clone()));
    }
    let mut severity_cell = h_flex()
        .debug_selector(|| "dock-log-severity".to_owned())
        .flex_none()
        .w(if compact {
            compact_severity_column_width()
        } else {
            severity_column_width(&typography)
        })
        .h(row_height)
        .gap(space::XS)
        .items_center()
        .child(
            Icon::new(severity_icon(severity))
                .size(IconSize::Custom(rems_from_px(f32::from(
                    design::size::STATUS_MARKER,
                ))))
                .color(Color::Custom(marker)),
        );
    if !compact {
        severity_cell = severity_cell.child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_color(colors.text_muted)
                .child(label),
        );
    }
    let prefix = h_flex()
        .flex_none()
        .when(compact_wrap, |this| this.w_full().items_center())
        .gap(space::SM)
        .child(
            div()
                .debug_selector(|| "dock-log-marker".to_owned())
                .flex_none()
                .w(design::size::STATUS_DOT)
                .h(row_height)
                .items_center()
                .child(
                    div()
                        .w(design::size::STATUS_DOT)
                        .h(design::size::STATUS_DOT)
                        .rounded_full()
                        .bg(marker),
                ),
        )
        .child(timestamp_cell)
        .child(severity_cell);
    let mut message = div()
        .debug_selector(|| "dock-log-message".to_owned())
        .min_w(px(0.))
        .text_color(colors.text)
        .child(display_message.clone());
    if wrap {
        message = message.flex_1().whitespace_normal();
        if compact_wrap {
            message = message.min_w(log_columns(&typography, LOG_MESSAGE_MIN_COLUMNS));
        }
    } else {
        message = message
            .flex_none()
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis();
    }
    // The copy control is drawn on every row, whatever the line width. A short line is exactly
    // as copyable as a long one, and the selection chord reaches it without the button.
    //
    // A wrapped row is taller than the control, so the control keeps its own height. A no-wrap
    // row is exactly one line tall: the control spans that line and uses the full control width
    // the row already reserves for it.
    let (copy_width, copy_height) = if wrap {
        (design::size::CONTROL, design::size::CONTROL)
    } else {
        (design::size::CONTROL, row_height)
    };
    let copy_label = if long {
        "Copy Full Log Line"
    } else {
        "Copy Log Line"
    };
    let mut copy = div()
        .id(("dock-log-copy", row_id))
        .debug_selector(|| "dock-log-copy".to_owned())
        .role(Role::Button)
        .aria_label(copy_label)
        .tab_index(-1isize)
        .tab_stop(false)
        .flex_none()
        .w(copy_width)
        .h(copy_height)
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|this| this.bg(colors.element_hover))
        .active(|this| this.bg(colors.element_active))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
            cx.stop_propagation();
        })
        .child(Icon::new(IconName::Copy).size(IconSize::XSmall));
    copy.interactivity().tooltip(Tooltip::text(if long {
        "Copy the full line. The row shows only part of it.".to_owned()
    } else {
        copy_label.to_owned()
    }));
    message = h_flex()
        .min_w(px(0.))
        .gap(space::SM)
        .when(wrap, |this| this.flex_1().items_start())
        .when(!wrap, |this| this.flex_none().items_center())
        .child(message)
        .child(copy);
    message
        .interactivity()
        .tooltip(Tooltip::text(display_message));

    let panel = row_context.panel;
    row.on_mouse_down(MouseButton::Left, move |event, _, cx| {
        panel
            .update(cx, |panel, cx| {
                panel.on_log_row_click(row_id, event.modifiers.shift, cx);
            })
            .ok();
    })
    .child(prefix)
    .child(message)
    .into_any_element()
}

fn reconnect_delay(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(4);
    (RECONNECT_BASE * factor).min(RECONNECT_MAX)
}

/// Counts read the same on every surface, so the Dock uses the shared formatter.
fn format_count(value: usize) -> String {
    design::format::count(value)
}

/// Exports log text with owner-only permissions and an atomic replace.
///
/// Exported logs can contain tokens and full stderr output, so the file must not
/// inherit a permissive umask and must never be observed half-written.
fn export_log_file(
    directory: &std::path::Path,
    filename: &str,
    text: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    ensure_export_directory(directory)?;
    let path = directory.join(filename);
    write_atomic(&path, text)?;
    Ok(path)
}

/// Creates the export directory privately, but leaves an existing directory alone.
///
/// The download directory is usually a user-owned shared folder, so its mode is
/// never rewritten here.
fn ensure_export_directory(directory: &std::path::Path) -> std::io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    create_private_dir_all(directory)
}

/// Keeps resource names usable as a single filename component.
///
/// Kubernetes names are already DNS labels, but the log panel also renders names
/// from watch events and cached rows, so the exporter never trusts the input.
fn sanitize_filename_component(raw: &str) -> String {
    const MAX_COMPONENT_CHARS: usize = 64;
    let filtered: String = raw
        .chars()
        .take(MAX_COMPONENT_CHARS)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = filtered.trim_matches(['.', '-']).to_owned();
    if trimmed.is_empty() {
        "logs".to_owned()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Instant;

    use gpui::{Focusable, TestAppContext};
    use theme::LoadThemes;

    use super::*;
    use crate::panels::logs::{LOG_TIMESTAMP_COLUMNS, LogEvent, LogSink, LogSubscription};

    fn init_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
    }

    /// The K8s Studio theme, read from the file the app ships.
    const K8S_STUDIO_THEME: &str = include_str!("../../../k8s-app/assets/themes/k8s-studio.json");

    /// CIE L\*, the unit `DESIGN.md` §3.4 quotes.
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

    fn theme_color(appearance: &str, key: &str) -> u32 {
        let theme_name = match appearance {
            "light" => "K8s Studio Light",
            "dark" => "K8s Studio Dark",
            other => panic!("unknown K8s Studio appearance {other}"),
        };
        let theme: serde_json::Value = serde_json::from_str(K8S_STUDIO_THEME).expect("theme JSON");
        let value = theme["themes"]
            .as_array()
            .expect("theme list")
            .iter()
            .find(|theme| theme["name"].as_str() == Some(theme_name))
            .unwrap_or_else(|| panic!("missing {theme_name}"))
            .get("style")
            // The dotted top-level key, which is the one the theme crate loads.
            // Reading `style.colors` here found nothing but a dead duplicate that
            // had already drifted, so the assertions below were passing against
            // values the app never ran on.
            .and_then(|style| style.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("missing {theme_name}/{key}"));
        let value = value.strip_prefix('#').expect("hex color");
        u32::from_str_radix(&value[..6], 16).expect("hex color") << 8 | 0xff
    }

    fn request() -> LogRequest {
        LogRequest {
            namespace: Some("default".into()),
            name: "web-0".into(),
            containers: vec!["app".into(), "sidecar".into()],
        }
    }

    /// Fake log source that sends test events through the sink.
    struct FakeLogs {
        sink: LogSink,
        restarts: usize,
        options: Vec<LogOptions>,
    }

    struct FakeSubscription;

    impl LogSubscription for FakeSubscription {
        fn cancel(&mut self) {}
    }

    fn fake_factory(state: Rc<RefCell<FakeLogs>>) -> LogFactory {
        Rc::new(move |_request, options, sink| {
            let mut state = state.borrow_mut();
            state.sink = sink;
            state.restarts += 1;
            state.options.push(options);
            Box::new(FakeSubscription)
        })
    }

    fn setup(
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<DockPanel>,
        Rc<RefCell<FakeLogs>>,
        &mut gpui::VisualTestContext,
    ) {
        setup_in_dock(cx, None, None)
    }

    /// Same stream, but the Dock gets its own width and height inside a realistically sized
    /// window. The Dock is `w_full()` in the app, so a narrow window is not how the compact
    /// breakpoint is reached.
    fn setup_in_dock(
        cx: &mut TestAppContext,
        width: Option<Pixels>,
        height: Option<Pixels>,
    ) -> (
        gpui::Entity<DockPanel>,
        Rc<RefCell<FakeLogs>>,
        &mut gpui::VisualTestContext,
    ) {
        init_app(cx);
        let (sink, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let state = Rc::new(RefCell::new(FakeLogs {
            sink,
            restarts: 0,
            options: Vec::new(),
        }));
        let factory = fake_factory(Rc::clone(&state));
        let (panel, cx) = add_dock_window(cx, width, height);
        panel.update(cx, |panel, cx| {
            panel.set_log_factory(Some(factory), cx);
            panel.open_logs(request(), cx);
        });
        (panel, state, cx)
    }

    /// A wheel gesture over a log list. A positive delta moves toward the older lines, which is
    /// what takes a list off its tail.
    fn scroll_log_list(selector: &'static str, cx: &mut gpui::VisualTestContext) {
        let list = cx.debug_bounds(selector).expect("the log list");
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: list.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(120.))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
    }

    fn push(
        state: &Rc<RefCell<FakeLogs>>,
        events: Vec<LogEvent>,
        cx: &mut gpui::VisualTestContext,
    ) {
        for event in events {
            let mut pending = Some(event);
            while let Some(event) = pending.take() {
                let sink = state.borrow().sink.clone();
                match sink.try_send(event) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                        pending = Some(event);
                        cx.run_until_parked();
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(event)) => {
                        panic!("fake stream closed before receiving {event:?}");
                    }
                }
            }
        }
        cx.run_until_parked();
    }

    #[gpui::test]
    fn log_queue_stops_at_its_configured_boundary(cx: &mut TestAppContext) {
        let (_panel, state, cx) = setup(cx);
        let sink = state.borrow().sink.clone();
        assert_eq!(sink.max_capacity(), LOG_EVENT_BUFFER);
        for index in 0..LOG_EVENT_BUFFER {
            sink.try_send(LogEvent::Line(format!("queued {index}")))
                .expect("queue has capacity");
        }
        assert!(matches!(
            sink.try_send(LogEvent::Line("over capacity".to_owned())),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        cx.run_until_parked();
        sink.try_send(LogEvent::Line("after consume".to_owned()))
            .expect("consumer made capacity");
        cx.run_until_parked();
    }

    #[gpui::test]
    fn log_buffer_keeps_newest_lines_within_its_byte_limit(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            let (appended, dropped, rebuilt) = panel.append_log_lines_with_limit(
                vec!["first".to_owned(), "second".to_owned(), "third".to_owned()],
                12,
            );
            assert_eq!(appended, 3);
            assert_eq!(dropped, 0);
            assert!(rebuilt);
            assert_eq!(panel.buffer.text(), "second\nthird\n");
            assert_eq!(panel.buffer_bytes, 11);
        });
    }

    struct CompactDockHarness {
        dock: gpui::Entity<DockPanel>,
        /// Width the Dock gets inside the window. A `None` width fills the window.
        width: Option<Pixels>,
        height: Pixels,
    }

    impl Render for CompactDockHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let dock = div().h(self.height).child(self.dock.clone());
            div()
                .size_full()
                .child(match self.width {
                    Some(width) => dock.w(width),
                    None => dock.w_full(),
                })
                .into_any_element()
        }
    }

    /// Adds a Dock to a fresh window, wrapped in `CompactDockHarness` when both a width and a
    /// height are given so the Dock is narrower than the window. Both paths hand back the Dock
    /// entity itself, so callers never have to know which window view was created.
    fn add_dock_window(
        cx: &mut TestAppContext,
        width: Option<Pixels>,
        height: Option<Pixels>,
    ) -> (gpui::Entity<DockPanel>, &mut gpui::VisualTestContext) {
        let (Some(width), Some(height)) = (width, height) else {
            return cx.add_window_view(|_, cx| DockPanel::new(cx));
        };
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            CompactDockHarness {
                dock,
                width: Some(width),
                height,
            }
        });
        let dock = harness.read_with(cx, |harness, _| harness.dock.clone());
        (dock, cx)
    }

    /// Window size the real app allows. The Dock is narrower than the window, which is what the
    /// compact breakpoint measures.
    const REAL_WINDOW: gpui::Size<Pixels> = gpui::size(px(960.), px(640.));
    /// Dock width below `design::size::CENTER_MIN`, so the compact layout is the one under test.
    const COMPACT_DOCK_WIDTH: Pixels = px(400.);
    /// Dock width above the breakpoint, so the full layout is the one under test.
    const WIDE_DOCK_WIDTH: Pixels = px(520.);
    /// Dock height the shell offers by default. It clears `design::size::DOCK_MIN`, so the log
    /// rows and the terminal body are both laid out at this height.
    const REAL_DOCK_HEIGHT: Pixels = px(240.);

    /// Stands in for the Shell so the Dock header's close control can be observed running the
    /// same action as the `ToggleDock` key.
    struct DockCloseHarness {
        dock: gpui::Entity<DockPanel>,
        toggled: Rc<Cell<usize>>,
    }

    impl Render for DockCloseHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let toggled = Rc::clone(&self.toggled);
            div()
                .size_full()
                .key_context("Shell")
                .on_action(move |_: &crate::shell::ToggleDock, _window, _cx| {
                    toggled.set(toggled.get() + 1);
                })
                .child(self.dock.clone())
                .into_any_element()
        }
    }

    /// The Dock had no close control at all, and `ToggleDock` is released on every surface that
    /// owns the keyboard, including the Terminal the Dock hosts. The header is the only way out
    /// from inside a session.
    #[gpui::test]
    fn dock_header_close_control_is_visible_and_runs_the_toggle_action(cx: &mut TestAppContext) {
        init_app(cx);
        let toggled = Rc::new(Cell::new(0));
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            DockCloseHarness {
                dock,
                toggled: Rc::clone(&toggled),
            }
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let row = cx
            .debug_bounds("dock-tabs-row")
            .expect("the Dock header is laid out");
        let last_tab = cx
            .debug_bounds("dock-tab-1")
            .expect("the Terminal tab is laid out");
        let close = cx
            .debug_bounds("dock-close")
            .expect("the Dock header has a close control");
        assert!(
            close.origin.x >= last_tab.right(),
            "the close control sits after the tabs, not between them"
        );
        assert!(close.origin.y >= row.origin.y && close.bottom() <= row.bottom());
        assert!(
            close.size.height >= design::size::HIT_MIN,
            "the control keeps a grabbable height: {:?}",
            close.size.height
        );
        assert!(
            close.size.width >= design::size::HIT_MIN,
            "the control keeps a grabbable width: {:?}",
            close.size.width
        );

        let dock = harness.read_with(cx, |harness, _| harness.dock.clone());
        let focus = dock.read_with(cx, |dock, _| dock.close_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        assert!(cx.update(|window, _| focus.is_focused(window)));
        cx.simulate_click(close.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(toggled.get(), 1, "the control runs the ToggleDock action");
    }

    /// The Logs empty state has to name a control, and there has to be a control. It used to be an
    /// icon, a title, and a sentence naming a menu that is not on screen, while the two sibling
    /// states in the same panel - the Terminal empty state and the log failure state - both had a
    /// real button.
    #[gpui::test]
    fn the_log_body_empty_state_offers_a_control(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = add_dock_window(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let state = cx.debug_bounds("empty-state").expect("Logs empty state");
        let band = cx
            .debug_bounds("dock-log-toolbar-shell")
            .expect("the Logs band");
        let open = cx
            .debug_bounds("dock-open-logs")
            .expect("the empty state offers a control");
        assert!(f32::from(open.size.height) >= f32::from(design::size::CONTROL));
        assert!(
            open.origin.y >= band.origin.y && open.bottom() <= band.bottom(),
            "the control shares the band that owns every other log command: {open:?} in {band:?}"
        );
        assert!(
            band.bottom() <= state.origin.y,
            "the control is above the body"
        );
        assert!(panel.read_with(cx, |panel, _| panel.log_band_is_shell()));
    }

    /// The `Open Logs` control is painted with the isobaric band and nowhere else. Once a target
    /// and a source are both there, the log controls own the band, and a control that reopens what
    /// is already open must not take a tab stop.
    #[gpui::test]
    fn the_open_logs_control_belongs_to_the_shell_band_alone(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| DockPanel::new(cx));
        assert!(
            panel.read_with(cx, |panel, _| panel.log_band_is_shell()),
            "a Dock with nothing asked for is exactly the state the control repairs"
        );
        // A source, not just a target: a target with nothing to read it is the state the band
        // exists for, and the log controls only take the band once the stream can run.
        panel.update(cx, |panel, cx| {
            panel.set_log_factory(
                Some(Rc::new(|_request, _options, _sink| {
                    Box::new(FakeSubscription)
                })),
                cx,
            );
            panel.open_logs(request(), cx);
        });
        assert!(
            !panel.read_with(cx, |panel, _| panel.log_band_is_shell()),
            "a Dock with a log target and a source must not offer to open another one"
        );
    }

    /// The Logs band is isobaric with the Terminal band. Dropping the whole 40px toolbar when
    /// there is no source moved the content's top edge by a full toolbar on a high-frequency
    /// interaction.
    #[gpui::test]
    fn the_logs_band_stays_put_when_there_is_no_log_source(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = add_dock_window(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let band = cx
            .debug_bounds("dock-log-toolbar-shell")
            .expect("the Logs band");
        let body = cx.debug_bounds("dock-log-body").expect("the log body");
        assert!(
            (f32::from(band.size.height) - f32::from(design::size::TOOLBAR)).abs() <= 1.0,
            "the band keeps the toolbar height with no source: {band:?}"
        );
        assert!(
            (f32::from(body.origin.y - band.bottom()) - f32::from(design::border::LINE)).abs()
                <= 1.0,
            "the body starts under the band and its rule"
        );

        panel.update(cx, |panel, cx| panel.show_terminal_tab(cx));
        cx.run_until_parked();
        let terminal_band = cx
            .debug_bounds("terminal-toolbar")
            .expect("the Terminal band");
        let terminal_body = cx.debug_bounds("empty-state").expect("the Terminal body");
        assert!(
            (f32::from(terminal_band.size.height) - f32::from(band.size.height)).abs() <= 1.0,
            "both tabs reserve the same band"
        );
        assert!(
            (f32::from(terminal_body.origin.y) - f32::from(body.origin.y)).abs() <= 1.0,
            "crossing between the tabs does not move the content's top edge"
        );
    }

    #[gpui::test]
    fn compact_log_empty_state_fits_without_clipping(cx: &mut TestAppContext) {
        init_app(cx);
        let (_harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            CompactDockHarness {
                dock,
                width: None,
                height: design::size::DOCK_MIN,
            }
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let state = cx
            .debug_bounds("empty-state")
            .expect("Logs empty state is laid out");
        let title = cx
            .debug_bounds("empty-state-title")
            .expect("Logs empty-state title is laid out");
        let hint = cx
            .debug_bounds("empty-state-hint")
            .expect("Logs empty-state hint is laid out");

        assert!(f32::from(state.size.width) > 0.0);
        assert!(state.size.height <= design::size::DOCK_MIN);
        assert!(f32::from(title.size.width) > 0.0);
        assert!(f32::from(title.size.height) > 0.0);
        assert!(f32::from(hint.size.width) > 0.0);
        assert!(f32::from(hint.size.height) > 0.0);
        assert!(hint.size.width <= state.size.width);
        assert!(hint.size.height <= design::text::METADATA_LINE_HEIGHT * 2.0);
        assert!(
            title.origin.y >= state.origin.y,
            "title starts above compact state"
        );
        assert!(
            hint.origin.y >= state.origin.y,
            "hint starts above compact state"
        );
        assert!(
            title.bottom() <= state.bottom(),
            "title escapes compact state"
        );
        assert!(
            hint.bottom() <= state.bottom(),
            "hint escapes compact state"
        );
    }

    #[test]
    fn target_labels_keep_cluster_namespace_kind_and_name() {
        let local = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
        };
        assert_eq!(
            terminal_target_label(&local, None).as_ref(),
            "kind-dev/default"
        );
        let all = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("kind-dev".to_owned()),
            namespace: None,
        };
        assert_eq!(
            terminal_target_label(&all, None).as_ref(),
            "kind-dev/All Namespaces"
        );
        let request = TerminalRequest {
            kind: TerminalKind::Exec {
                namespace: "default".to_owned(),
                pod: "web-0".to_owned(),
                container: Some("app".to_owned()),
            },
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
        };
        assert_eq!(
            terminal_target_label(&request, None).as_ref(),
            "kind-dev/default/Pod/web-0:app"
        );
        let forward = ForwardRequest {
            context: None,
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        assert_eq!(
            forward_target_label(&forward, Some("kind-dev")).as_ref(),
            "kind-dev/default/Pod/web-0"
        );
    }

    #[test]
    fn log_width_ignores_ansi_controls_and_counts_wide_text() {
        assert_eq!(
            crate::panels::logs::log_message_columns("\u{1b}[31m你好\u{1b}[0m\t"),
            4
        );
        assert_eq!(crate::panels::logs::log_message_columns("a\u{200b}"), 1);
    }

    /// The close control never advertises a key a focused terminal has taken.
    ///
    /// `default-linux.json` binds `secondary-j` to `ToggleDock` outside the command palette and
    /// `Terminal` unbinds it, so with a session focused the chord is a character the shell reads
    /// (`^J` starts readline's reverse history search). The hint used to be built from a context
    /// constant, which cannot see a live focus stack, and the Dock hosts the terminal in question.
    #[gpui::test]
    fn the_dock_close_chord_follows_the_focused_surface(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
        let dock = gpui::KeyContext::parse(DOCK_CLOSE_CONTEXT).expect("a plain context name");
        let terminal =
            gpui::KeyContext::parse(DOCK_TERMINAL_CONTEXT).expect("a plain context name");

        let chord = cx.update(|cx| dock_close_chord(std::slice::from_ref(&dock), cx));
        assert!(
            chord.is_some(),
            "the Dock is a surface the key reaches, so its own close control has to say so"
        );
        assert_eq!(
            cx.update(|cx| dock_close_chord(&[dock.clone(), terminal.clone()], cx)),
            None,
            "a session has focus, so the chord is a character the shell reads: \
             {chord:?} would be advertised for a key the terminal took away"
        );
        assert_eq!(
            cx.update(|cx| dock_close_chord(&[terminal], cx)),
            None,
            "the terminal alone is enough: it is the surface under the pointer that owns the keys"
        );
    }

    /// A hint is written against a focus path, so the path has to be a context some view declares.
    ///
    /// `DOCK_CLOSE_CONTEXT` was `"Shell Dock"`, which no view declared. `FocusPath::contexts`
    /// turned that into `["", "Shell Dock"]`, no keymap predicate could match it, and the hint
    /// came to advertise `Ctrl-J` on a surface the keymap had never heard of. The scan is the
    /// same one the audit ran by hand, kept as a test so the next context constant cannot drift.
    #[test]
    fn the_dock_hint_names_a_context_some_view_declares() {
        let mut declared = Vec::new();
        for root in [
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../k8s-term/src"),
        ] {
            collect_key_contexts(&root, &mut declared);
        }
        assert!(
            declared.len() > 10,
            "the scan found {} contexts, so it is not reading the sources",
            declared.len()
        );
        for (name, context) in [
            ("DOCK_CLOSE_CONTEXT", DOCK_CLOSE_CONTEXT),
            ("DOCK_TERMINAL_CONTEXT", DOCK_TERMINAL_CONTEXT),
        ] {
            assert!(
                declared.iter().any(|found| found == context),
                "{name} is {context:?} and no view declares that key_context. A hint is written \
                 against a focus path, so it has to name a surface that exists. Declared: \
                 {declared:?}"
            );
        }
    }

    /// Every `key_context("…")` literal under `root`, as the name the view gave it.
    fn collect_key_contexts(root: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_key_contexts(&path, out);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            // The argument parser below skips anything that is neither a quoted
            // name nor a constant it can resolve, so the copy of this call living
            // in this file contributes nothing.
            let call = String::from(".key_context(");
            let mut rest = source.as_str();
            while let Some(at) = rest.find(&call) {
                // The argument is bounded by its closing paren before anything is
                // read out of it. A context name is a bare word or a quoted word,
                // so the paren is the only thing either can contain -- and reading
                // past it would pick up an unrelated quote from the rest of the
                // file.
                let after = &rest[at + call.len()..];
                let end = after.find(')').unwrap_or(after.len());
                let argument = after[..end].trim();
                if let Some(literal) = argument
                    .strip_prefix('"')
                    .and_then(|tail| tail.strip_suffix('"'))
                {
                    out.push(literal.to_owned());
                } else if let Some(value) = string_const_in(&source, argument) {
                    // `k8s-term` declares its Terminal context as
                    // `key_context(TERMINAL_KEY_CONTEXT)`, so a literal-only scan
                    // reported a surface that exists as one that does not.
                    out.push(value);
                }
                rest = &after[end..];
            }
        }
    }

    /// The string a `const NAME: &str = "…"` declaration in `source` binds, if it does.
    fn string_const_in(source: &str, name: &str) -> Option<String> {
        for prefix in [
            format!("const {name}: &str = \""),
            format!("{name}: &str = \""),
        ] {
            let Some(at) = source.find(&prefix) else {
                continue;
            };
            let rest = &source[at + prefix.len()..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_owned());
        }
        None
    }

    /// The data typography the geometry helpers are measured against.
    ///
    /// `settings::test_data_typography` is the seam that needs neither a theme nor a text system,
    /// and it is the only way to see the raised half of the "Data font size" promise without
    /// writing the reader's settings file.
    fn log_typography(size: f32, line_height: f32) -> DataTypography {
        crate::settings::test_data_typography(size, line_height)
    }

    #[test]
    fn log_geometry_reserves_fixed_columns_for_critical_and_plain_lines() {
        let critical = LogLine::parse("CRITICAL readiness probe failed");
        let plain = LogLine::parse("plain");
        let long = LogLine::parse(&format!(
            "INFO {}",
            "x".repeat(LOG_COPY_COLUMN_THRESHOLD + 1)
        ));
        let reserve = LOG_TIMESTAMP_COLUMNS;
        let typography = log_typography(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            crate::settings::PRODUCT_DATA_LINE_HEIGHT * crate::settings::PRODUCT_DATA_FONT_SIZE,
        );
        assert!(critical.timestamp.is_none());
        assert!(plain.timestamp.is_none());
        assert_eq!(
            timestamp_column_width(reserve, &typography),
            typography.columns(reserve as f32)
        );
        assert!(
            severity_column_width(&typography) >= typography.columns(LOG_SEVERITY_COLUMNS as f32)
        );
        // The glyph in the cell is `design::size::STATUS_MARKER`, so the cell has to be at least
        // that wide or the level label runs past the row's measured width.
        assert!(
            severity_column_width(&typography) >= design::size::STATUS_MARKER,
            "the severity cell must fit the status marker"
        );
        assert!(
            compact_severity_column_width() >= design::size::STATUS_MARKER,
            "a compact row drops the label, not the marker"
        );
        let fixed = log_row_fixed_width(
            timestamp_column_width(reserve, &typography),
            false,
            &typography,
        );
        let copy = design::size::CONTROL + space::SM;
        assert_eq!(
            log_row_width(&plain, false, reserve, &typography),
            fixed + typography.columns(plain.message_columns() as f32) + copy,
            "a short line reserves the copy control too, so it stays copyable"
        );
        assert_eq!(
            log_row_width(&long, false, reserve, &typography),
            fixed + typography.columns(long.message_columns() as f32) + copy
        );
        assert!(
            log_row_width(&critical, false, reserve, &typography)
                > log_row_width(&plain, false, reserve, &typography)
        );
    }

    #[test]
    fn compact_rows_reserve_only_the_columns_they_render() {
        let stamp = LogLine::parse("2026-09-22T21:14:02.331Z INFO ready");
        let plain = LogLine::parse("plain");
        let reserve = LOG_TIMESTAMP_COLUMNS;
        let typography = log_typography(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            crate::settings::PRODUCT_DATA_LINE_HEIGHT * crate::settings::PRODUCT_DATA_FONT_SIZE,
        );
        let wide = log_row_fixed_width(
            timestamp_column_width(reserve, &typography),
            false,
            &typography,
        );
        let compact = log_row_fixed_width(
            timestamp_column_width(reserve, &typography),
            true,
            &typography,
        );
        let copy = design::size::CONTROL + space::SM;
        assert!(
            compact < wide,
            "compact rows drop the severity label column"
        );
        assert_eq!(
            log_row_width(&plain, true, reserve, &typography),
            compact + typography.columns(plain.message_columns() as f32) + copy,
            "a compact row reserves what it draws"
        );
        assert_eq!(
            log_row_width(&stamp, true, reserve, &typography),
            compact + typography.columns(stamp.message_columns() as f32) + copy,
            "a stamped compact row adds exactly the timestamp column"
        );
        let no_stamp_reserve = 0;
        assert_eq!(
            log_row_width(&plain, true, no_stamp_reserve, &typography),
            log_row_fixed_width(px(0.), true, &typography)
                + typography.columns(plain.message_columns() as f32)
                + copy,
            "a stream without timestamps gives the column back"
        );
    }

    #[test]
    fn timestamp_reserve_sizes_the_column_from_the_buffer() {
        let mut buffer = LogBuffer::new();
        buffer.push_many(["2026-09-22T21:14:02.331Z INFO ready".to_owned()]);
        let line = buffer.line(0).expect("line");
        let reserve = buffer.timestamp_column_reserve();
        let typography = log_typography(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            crate::settings::PRODUCT_DATA_LINE_HEIGHT * crate::settings::PRODUCT_DATA_FONT_SIZE,
        );
        assert_eq!(reserve, "2026-09-22T21:14:02.331Z".len());
        assert_eq!(
            log_timestamp_width(&line, reserve, &typography),
            typography.columns(reserve as f32),
            "the row shows the millisecond timestamp in full"
        );
        assert_eq!(
            log_timestamp_width(&LogLine::parse("plain"), reserve, &typography),
            typography.columns(reserve as f32),
            "a line without a timestamp keeps the column so messages align"
        );
        buffer.push_many(["2026-09-22T21:14:02.331331331+02:00 INFO offset".to_owned()]);
        let long = buffer.line(1).expect("line");
        let reserve = buffer.timestamp_column_reserve();
        assert_eq!(
            log_timestamp_width(&long, reserve, &typography),
            typography.columns(long.timestamp_columns() as f32)
        );
        assert!(
            log_row_width(&long, false, reserve, &typography)
                >= log_timestamp_width(&long, reserve, &typography)
                    + typography.columns(long.display_columns() as f32),
            "the row width covers a token wider than the reserve"
        );
    }

    /// A raised data font widens the columns, or every no-wrap row is ellipsised.
    ///
    /// The row height and the column widths are two halves of one promise, and the columns are the
    /// half that no bounds assertion can see: a 20px glyph measured into a 12px column still draws
    /// a row, it just draws the wrong number of characters in it.
    #[test]
    fn the_log_columns_follow_the_data_font() {
        let plain = LogLine::parse("plain");
        let reserve = LOG_TIMESTAMP_COLUMNS;
        let default = log_typography(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            crate::settings::PRODUCT_DATA_LINE_HEIGHT * crate::settings::PRODUCT_DATA_FONT_SIZE,
        );
        let large = log_typography(20., 30.);
        assert!(
            log_columns(&large, plain.message_columns())
                > log_columns(&default, plain.message_columns()),
            "the message column has to grow with the font it measures"
        );
        assert!(
            timestamp_column_width(reserve, &large) > timestamp_column_width(reserve, &default),
            "the timestamp column has to grow with the font it measures"
        );
        assert!(
            log_row_width(&plain, false, reserve, &large)
                > log_row_width(&plain, false, reserve, &default),
            "the whole row has to grow: the uniform list scrolls the width the row reserves"
        );
    }

    #[test]
    fn forward_rows_speak_the_failure_and_hide_the_dead_port() {
        let mut entry = ForwardEntry {
            id: ForwardId(1),
            request: ForwardRequest {
                context: Some("kind-dev".to_owned()),
                namespace: Some("default".to_owned()),
                name: "web-0".into(),
                remote_port: 8080,
                local_port: None,
            },
            label: "kind-dev/default/Pod/web-0".into(),
            remote_port: 8080,
            local_port: Some(4321),
            handle: None,
            phase: ForwardPhase::Running,
            error: None,
            runtime_id: 0,
        };
        assert_eq!(forward_live_port(&entry), Some(4321));
        assert_eq!(
            forward_row_target(&entry),
            "localhost:4321 → kind-dev/default/Pod/web-0:8080"
        );
        assert_eq!(
            forward_row_aria(&entry, "running", "Select Stop to end this forward."),
            "localhost:4321 to kind-dev/default/Pod/web-0:8080. Port forward running. \
             Select Stop to end this forward."
        );

        // A failed forward stopped listening, so it must not keep reporting the port.
        entry.phase = ForwardPhase::Failed;
        entry.error = Some("port 8080: connection reset".into());
        assert_eq!(forward_live_port(&entry), None);
        assert_eq!(
            forward_row_target(&entry),
            "kind-dev/default/Pod/web-0:8080"
        );
        let aria = forward_row_aria(&entry, "failed", "Select Retry to reconnect.");
        assert!(aria.contains("Port forward failed."));
        assert!(
            aria.contains("port 8080: connection reset"),
            "the reason must reach the spoken row: {aria}"
        );
        assert!(
            !forward_row_target(&entry).contains("4321"),
            "a failed row must not show a port it no longer listens on"
        );
    }

    #[test]
    fn tab_panels_are_named_after_the_active_tab() {
        assert_eq!(tab_panel_label(0, 0).as_ref(), "Logs");
        assert_eq!(tab_panel_label(1, 0).as_ref(), "Terminal");
        assert_eq!(tab_panel_label(9, 0).as_ref(), "Dock");
    }

    /// The Terminal tab counts its sessions once there is more than one. A strip that reads
    /// `Terminal` in every state says nothing about how many shells are open, while the log body
    /// counts them.
    #[test]
    fn the_terminal_tab_counts_sessions_past_the_first() {
        assert_eq!(terminal_tab_label(0).as_ref(), "Terminal");
        assert_eq!(terminal_tab_label(1).as_ref(), "Terminal");
        assert_eq!(terminal_tab_label(2).as_ref(), "Terminal · 2");
        assert_eq!(
            terminal_tab_label(1234).as_ref(),
            "Terminal · 1,234",
            "the count reads the same as every other count in the app"
        );
        assert_eq!(
            tab_panel_label(1, 3).as_ref(),
            "Terminal · 3",
            "the tab panel announces the same name as the tab that opens it"
        );
    }

    /// The terminal canvas is a surface role of its own, and nothing in the layer ramp says how it
    /// relates to the `panel` it sits in. The rule is the smallest change that states it.
    ///
    /// The *sign* of the relation is not asserted. It is in fact inverted between appearances -
    /// the terminal is a hole below the Dock in dark and a card above it in light - and the reason
    /// it cannot be fixed here is recorded in `DESIGN.md` §9: the dim ANSI palette only clears
    /// `configured_minimum_contrast_reaches_each_ansi_cell` on this exact dark terminal value, so
    /// moving it would make a known-failing test fail harder. The sign is therefore documented
    /// rather than enforced, and this assertion only holds the two apart.
    #[test]
    fn the_terminal_surface_differs_from_the_dock_in_both_appearances() {
        for appearance in ["light", "dark"] {
            let panel = theme_color(appearance, "panel.background");
            let terminal = theme_color(appearance, "terminal.background");
            assert_ne!(
                terminal, panel,
                "{appearance}: the terminal canvas shares the panel value, so the Dock has no \
                 layer under the session at all"
            );
            let separation = (lightness_star(terminal) - lightness_star(panel)).abs();
            assert!(
                separation >= 1.0,
                "{appearance}: terminal and panel sit {separation:.2} L* apart, which reads as one \
                 surface. The sign is documented in DESIGN.md; the step is not negotiable."
            );
        }
    }

    /// A session that has exited says so on its chip. A session that has not is not drawn as
    /// healthy: the Dock does not observe the process, so it has no verdict to report, and the
    /// slot is reserved so the chip does not shift when one arrives.
    #[test]
    fn a_session_chip_reports_a_verdict_only_when_it_has_one() {
        let exit = SharedString::from("The shell exited with code 1.");
        assert_eq!(
            session_verdict(None),
            None,
            "an open session has no verdict: the Dock never watched the process"
        );
        assert_eq!(session_verdict_label(None), "Session open");
        assert_eq!(session_verdict(Some(&exit)), Some(Severity::Warning));
        assert_eq!(
            session_verdict_label(Some(&exit)),
            design::health_label(Severity::Warning),
            "the chip's accessible name carries the same word the glyph means"
        );
    }

    #[test]
    fn uniform_list_follow_uses_negative_offsets() {
        assert!(uniform_list_at_bottom(px(-96.), px(100.)));
        assert!(!uniform_list_at_bottom(px(-80.), px(100.)));
        assert!(uniform_list_at_bottom(px(0.), px(0.)));
    }

    /// The chord that copies the log selection. The AltGr third level prints a character, so it
    /// must not be mistaken for it, and Shift belongs to the commands built on top of it.
    #[test]
    fn log_copy_chord_ignores_altgr_and_the_platform_modifier() {
        let chord = |key: &str, modifiers: gpui::Modifiers| gpui::Keystroke {
            key: key.to_owned(),
            key_char: None,
            modifiers,
        };
        assert!(is_log_copy_chord(&chord("c", gpui::Modifiers::control())));
        // The keysym carries whatever case the layout and CapsLock produce, so the chord is
        // matched on the key alone.
        assert!(is_log_copy_chord(&chord("C", gpui::Modifiers::control())));
        for modifiers in [
            gpui::Modifiers::default(),
            gpui::Modifiers::control_shift(),
            gpui::Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
            gpui::Modifiers {
                control: true,
                platform: true,
                ..Default::default()
            },
        ] {
            assert!(!is_log_copy_chord(&chord("c", modifiers)), "{modifiers:?}");
        }
        assert!(!is_log_copy_chord(&chord("v", gpui::Modifiers::control())));
    }

    #[test]
    fn longest_log_row_uses_filtered_row_positions() {
        let mut buffer = LogBuffer::new();
        buffer.push_many([
            "INFO short".to_owned(),
            "a much longer matching message".to_owned(),
            "WARN another longer message".to_owned(),
        ]);
        assert_eq!(buffer.longest_message_index(), 1);
        assert_eq!(longest_log_row(&buffer, &[1]), 0);
    }

    /// The compact layout is decided by the Dock width, so a realistic window with a narrow Dock
    /// is the only way to reach it.
    #[gpui::test]
    fn compact_dock_header_and_log_row_fit_narrow_width(cx: &mut TestAppContext) {
        let (_panel, state, cx) =
            setup_in_dock(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        push(
            &state,
            vec![
                LogEvent::Line(
                    "CRITICAL hash 0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                ),
                LogEvent::Line("plain output without a timestamp".into()),
            ],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let tabs = cx
            .debug_bounds("dock-tabs-row")
            .expect("Dock tabs is laid out");
        let first_tab = cx.debug_bounds("dock-tab-0").expect("Logs tab is laid out");
        let toolbar = cx
            .debug_bounds("dock-log-toolbar")
            .expect("Logs toolbar is laid out");
        let row = cx
            .debug_bounds("dock-log-row")
            .expect("A log row is laid out");
        let message = cx
            .debug_bounds("dock-log-message")
            .expect("A compact log message is laid out");
        assert!((f32::from(first_tab.origin.x) - f32::from(tabs.origin.x)).abs() <= 1.0);
        assert!((f32::from(toolbar.size.height) - f32::from(design::size::TOOLBAR)).abs() <= 1.0);
        assert!(toolbar.origin.y >= tabs.bottom());
        assert!(row.origin.x >= tabs.origin.x);
        let row_height = cx.update(|_, cx| log_row_height(cx));
        let typography = cx.update(|_, cx| crate::settings::data_typography(cx));
        assert!(row.size.height >= row_height);
        assert!(message.size.width >= log_columns(&typography, LOG_MESSAGE_MIN_COLUMNS) - px(1.));
        assert!(cx.debug_bounds("dock-log-filter").is_none());
    }

    /// The log row is the data line the reader configured.
    ///
    /// `LOG_ROW_HEIGHT` was a `const` reading `design::text::DATA_LINE_HEIGHT`, and a `const`
    /// cannot read a runtime setting: raising "Data font size" made the glyphs taller and left the
    /// row, the uniform list's item height and the scroll arithmetic at 18px, which is a taller
    /// glyph in a shorter box — the case the setting's own help text says cannot happen. The row
    /// is read at the configured size here, with the setting moved through the store so no test
    /// writes the reader's settings file.
    #[gpui::test]
    fn the_log_row_follows_the_configured_data_font(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![LogEvent::Line(
                "2026-09-22T21:14:02.331Z INFO ready".to_owned(),
            )],
            cx,
        );
        // The no-wrap list is the one with a fixed row height, and it is the mode whose uniform
        // list and scroll arithmetic are measured with the same number.
        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let before = f32::from(
            cx.debug_bounds("dock-log-row")
                .expect("a no-wrap log row is laid out")
                .size
                .height,
        );
        cx.update(|_, cx| {
            crate::settings::SettingsStore::update(cx, |store, cx| {
                store
                    .set_user_settings(r#"{ "buffer_font_size": 20 }"#, cx)
                    .result()
                    .expect("the data size applies");
            });
        });
        panel.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        let expected = cx.update(|_, cx| log_row_height(cx));
        let after = f32::from(
            cx.debug_bounds("dock-log-row")
                .expect("the row is still laid out")
                .size
                .height,
        );
        assert!(
            (after - f32::from(expected)).abs() <= 1.0,
            "the row drew {after}px and the configured data line is {expected}px; the constant \
             held it at {before}px while the glyph grew"
        );
        assert!(
            after > before,
            "the fixture has to actually raise the data font, or this test proves nothing"
        );
    }

    #[gpui::test]
    fn wrapped_log_columns_align_to_first_line(cx: &mut TestAppContext) {
        let (_panel, state, cx) = setup_in_dock(cx, Some(WIDE_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        let hash = "0123456789abcdef".repeat(2);
        push(
            &state,
            vec![LogEvent::Line(format!(
                "2026-09-22T21:14:02.331Z CRITICAL {hash}"
            ))],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let row = cx
            .debug_bounds("dock-log-row")
            .expect("wrapped log row is laid out");
        let marker = cx
            .debug_bounds("dock-log-marker")
            .expect("log marker column is laid out");
        let timestamp = cx
            .debug_bounds("dock-log-timestamp")
            .expect("log timestamp column is laid out");
        let severity = cx
            .debug_bounds("dock-log-severity")
            .expect("log severity column is laid out");
        let message = cx
            .debug_bounds("dock-log-message")
            .expect("log message column is laid out");
        // The column follows the precision this stream sends, not the widest form it could send.
        let reserve = "2026-09-22T21:14:02.331Z".len();
        // The row reserves the focus rail before its padding, so the columns start inside it.
        let typography = cx.update(|_, cx| crate::settings::data_typography(cx));
        let row_height = cx.update(|_, cx| log_row_height(cx));
        let message_origin = row.origin.x
            + design::border::FOCUS_RAIL
            + log_row_fixed_width(
                timestamp_column_width(reserve, &typography),
                false,
                &typography,
            )
            - space::SM;

        assert!((f32::from(marker.origin.y) - f32::from(row.origin.y)).abs() <= 1.0);
        assert!((f32::from(timestamp.origin.y) - f32::from(row.origin.y)).abs() <= 1.0);
        assert!((f32::from(severity.origin.y) - f32::from(row.origin.y)).abs() <= 1.0);
        assert!((f32::from(marker.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(timestamp.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(severity.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(message.origin.x) - f32::from(message_origin)).abs() <= 1.0);
        assert!(f32::from(message.size.height) >= f32::from(row_height) * 2.0 - 1.0);
    }

    #[gpui::test]
    /// Mouse and trackpad scrolling must move the log list and report follow state, and must not
    /// read the list state that the list already borrowed for its own scroll event.
    #[gpui::test]
    fn log_list_scrolls_with_the_mouse(cx: &mut TestAppContext) {
        for wrap in [true, false] {
            let (panel, state, cx) = setup(cx);
            panel.update(cx, |panel, cx| {
                if !wrap {
                    panel.toggle_wrap(cx);
                }
            });
            push(
                &state,
                (0..400)
                    .map(|index| LogEvent::Line(format!("line {index}")))
                    .collect(),
                cx,
            );
            cx.simulate_resize(gpui::size(px(960.), px(360.)));
            cx.run_until_parked();
            assert!(
                panel.read_with(cx, |panel, _| panel.is_following()),
                "wrap={wrap}: a fresh stream follows the tail"
            );
            let row = cx
                .debug_bounds("dock-log-row")
                .expect("log row is laid out");

            for _ in 0..5 {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: row.center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::Point {
                        x: px(0.),
                        y: px(120.),
                    }),
                    modifiers: gpui::Modifiers::none(),
                    touch_phase: gpui::TouchPhase::Moved,
                });
                cx.run_until_parked();
            }

            assert!(
                !panel.read_with(cx, |panel, _| panel.is_following()),
                "wrap={wrap}: scrolling up pauses the follow state"
            );
            panel.update(cx, |panel, cx| panel.set_follow(true, cx));
            cx.run_until_parked();
            assert!(
                panel.read_with(cx, |panel, _| panel.is_following()),
                "wrap={wrap}: the Follow action resumes the tail"
            );
        }
    }

    /// The Follow state must come from the list that owns the scroll: while the uniform list owns
    /// it, the wrapped list state must not re-arm the tail.
    #[gpui::test]
    fn follow_reads_the_active_scroll_owner(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..400)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(gpui::size(px(960.), px(360.)));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.is_following()));

        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.run_until_parked();
        panel.update(cx, |panel, _| panel.sync_scroll_follow());
        assert!(
            panel.read_with(cx, |panel, _| panel.is_following()),
            "at the newest line the follow state is on"
        );

        panel.update(cx, |panel, _| {
            let half = px(panel.scroll_handle.0.borrow().base_handle.max_offset().y / px(2.));
            assert!(half > px(0.), "the uniform list has room to scroll");
            panel
                .scroll_handle
                .0
                .borrow_mut()
                .base_handle
                .set_offset(gpui::point(px(0.), -half));
            panel.sync_scroll_follow();
        });
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_following()),
            "the wrapped list cannot re-arm follow while the uniform list is scrolled up"
        );
    }

    /// Stands in for the Dock so the wrapped list can be scrolled on its own. The list owns its
    /// scroll state and runs its handler while it holds that state, so the handler has to record
    /// what it saw without a panel to call back into.
    struct ScrollReportHarness {
        state: ListState,
        report: Rc<Cell<Option<bool>>>,
    }

    impl ScrollReportHarness {
        fn new() -> Self {
            let state = ListState::new(0, ListAlignment::Top, px(100.));
            let report: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
            install_list_scroll_report(&state, &report);
            state.reset_with_uniform_height(200, px(28.));
            Self { state, report }
        }
    }

    impl Render for ScrollReportHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            list(self.state.clone(), |row, _, _| {
                div().h(px(28.)).child(row.to_string()).into_any_element()
            })
            .size_full()
        }
    }

    /// The handler is a state write. A handler that re-entered the panel would mutate the panel in
    /// the middle of the list's own scroll pass, and it would run again on every wheel event. The
    /// report is the whole handoff, and it lands without a panel anywhere in sight.
    #[gpui::test]
    fn the_list_scroll_handler_records_state_without_a_panel(cx: &mut TestAppContext) {
        init_app(cx);
        let (harness, cx) = cx.add_window_view(|_, _| ScrollReportHarness::new());
        let report = harness.read_with(cx, |harness, _| Rc::clone(&harness.report));
        assert_eq!(report.get(), None, "the list starts with nothing to report");

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(200.), px(150.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(120.))),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        assert_eq!(
            report.get(),
            Some(false),
            "the list left its tail and recorded that on its own"
        );
    }

    /// The uniform list owns the scroll in no-wrap mode and leaves no report, so the follow state
    /// comes from its own handle. Both scroll owners have to reach the same state without the list
    /// calling into the panel.
    #[gpui::test]
    fn the_nowrap_scroll_owner_reads_its_own_handle(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..400)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                !panel.wrap,
                "the uniform list is the scroll owner under test"
            );
            assert!(panel.list_follow_report.get().is_none());
        });
        assert!(panel.read_with(cx, |panel, _| panel.is_following()));

        // Scrolling the uniform list up leaves the tail, and the panel reads that off the handle.
        let list = cx
            .debug_bounds("dock-log-nowrap-scroll")
            .expect("the no-wrap log list");
        assert!(list.size.height > px(0.), "the list was laid out");
        scroll_log_list("dock-log-nowrap-scroll", cx);
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_following()),
            "the uniform list reported that it left the tail"
        );
    }

    /// The failure state has no rows, and the arrows must not fall through to the resource table
    /// behind the Dock while the Dock holds the keyboard. A failed stream is exactly the case where
    /// the list has nothing to move.
    #[gpui::test]
    fn a_failed_log_stream_keeps_the_navigation_keys(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.phase = LogPhase::Failed {
                reason: "connection reset".to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.visible_log_count()),
            0,
            "a failed stream has nothing to scroll"
        );
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));

        // Every navigation key is spent by the empty list, so the key event stops at the Dock
        // instead of reaching the shell. The panel keeps its own state, which is what a pass
        // through would change: a scroll or a caret.
        for key in ["up", "down", "pageup", "pagedown", "home", "end"] {
            assert!(is_log_navigation_key(key), "{key} is a navigation key");
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_count(), 0);
            assert!(
                panel.log_selection.is_none(),
                "an empty list takes no caret"
            );
        });

        // Everything the list cannot move stays available to the rest of the app.
        for key in ["a", "f5", "escape", "tab", "left", "right"] {
            assert!(
                !is_log_navigation_key(key),
                "{key} must stay available to the rest of the app"
            );
        }
    }

    /// A key works from one row count, and the last row index saturates, so a filter that empties
    /// the list cannot wrap the caret around or underflow the count. Every state the list can be in
    /// is driven with every navigation key: a wrap shows up as a caret outside the rows, and the
    /// unchecked subtraction of an empty count panics in a test build.
    #[gpui::test]
    fn navigation_keys_stay_inside_the_visible_rows(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..50)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));

        // One matching row, no filter at all, and nothing at all: the three row counts the keys can
        // meet, including the one the old subtraction could not survive.
        for filter in ["line 4", "", "no-such-line"] {
            panel.update(cx, |panel, cx| panel.set_log_filter(filter, cx));
            // A pointer press is the only thing that puts a caret in an untouched list.
            panel.update(cx, |panel, cx| panel.on_log_row_click(0, false, cx));
            cx.run_until_parked();
            for key in [
                "up",
                "down",
                "home",
                "end",
                "pageup",
                "pagedown",
                "shift-up",
                "shift-down",
                "shift-home",
                "shift-end",
            ] {
                cx.simulate_keystrokes(key);
                cx.run_until_parked();
                panel.read_with(cx, |panel, _| {
                    let visible = panel.visible_log_count();
                    let Some(selection) = panel.log_selection else {
                        assert_eq!(
                            visible, 0,
                            "filter {filter:?} has {visible} rows and no caret, so a key wrapped it"
                        );
                        return;
                    };
                    assert!(
                        selection.head < visible,
                        "filter {filter:?}, {key}: row {} of {visible} visible rows",
                        selection.head
                    );
                });
            }
        }
    }

    /// A split exec session keeps the pane it was split from identifiable: the new pane takes the
    /// keyboard, and it keeps the pod label while the source pane follows its OSC title.
    #[gpui::test]
    fn exec_split_activates_the_new_pane_with_its_own_title(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel
                .open_terminal(
                    TerminalKind::Exec {
                        namespace: "team-a".to_owned(),
                        pod: "web-0".to_owned(),
                        container: Some("app".to_owned()),
                    },
                    cx,
                )
                .expect("open exec terminal");
        });
        let source_sink = terminals.borrow().sink.clone().expect("source sink");
        cx.update(|_, cx| source_sink.as_ref()(TerminalEvent::Title("bash".into()), cx));
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminals.len(), 2);
            assert!(panel.terminal_split);
            assert_eq!(
                panel.active_terminal, 1,
                "the pane the user just created takes the keyboard"
            );
            assert!(panel.terminals[1].split_peer);
            let titles = [
                terminal_entry_title(&panel.terminals[0], Some("kind-dev")),
                terminal_entry_title(&panel.terminals[1], Some("kind-dev")),
            ];
            assert_eq!(titles[0].as_ref(), "bash");
            assert_eq!(titles[1].as_ref(), "kind-dev/team-a/Pod/web-0:app");
            assert_ne!(
                titles[0], titles[1],
                "two panes of the same pod must not share a title"
            );
        });

        // The shared OSC title must not collapse both panes onto the same name.
        let peer_sink = terminals.borrow().sink.clone().expect("peer sink");
        cx.update(|_, cx| peer_sink.as_ref()(TerminalEvent::Title("bash".into()), cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.terminals[1].title.is_none());
            assert_eq!(
                terminal_entry_title(&panel.terminals[1], Some("kind-dev")).as_ref(),
                "kind-dev/team-a/Pod/web-0:app"
            );
        });
    }

    /// A local split stays in the namespace of the pane it was split from, not the current scope.
    #[gpui::test]
    fn local_split_keeps_the_session_namespace(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            let mut services = panel.terminal_services.clone().expect("terminal services");
            services.namespace = Some("team-a".to_owned());
            panel.set_terminal_services(Some(services), cx);
            panel.open_terminal(TerminalKind::Local, cx).expect("open");
        });
        panel.update(cx, |panel, cx| {
            let mut services = panel.terminal_services.clone().expect("terminal services");
            services.namespace = Some("team-b".to_owned());
            panel.set_terminal_services(Some(services), cx);
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });

        let requests = terminals.borrow().requests.clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].namespace.as_deref(), Some("team-a"));
        assert_eq!(requests[1].kind, TerminalKind::Local);
    }

    /// An exit hands the keyboard to the neighbour, and only when the dead pane held it.
    #[gpui::test]
    fn terminal_exit_focuses_the_neighbour_chip(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("first");
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("second");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.activate_terminal(0, window, cx));
        });
        cx.run_until_parked();
        let handles = panel.read_with(cx, |panel, _| panel.terminal_focus_handles.clone());
        let sink = terminals
            .borrow()
            .sinks
            .first()
            .cloned()
            .expect("first sink");
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(1),
                    signal: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            cx.update(|window, _| handles[1].is_focused(window)),
            "the session that took the dead pane's place owns the keyboard"
        );
    }

    #[gpui::test]
    fn background_terminal_exit_keeps_the_current_focus(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.activate_terminal(0, window, cx));
        });
        cx.run_until_parked();
        let add_focus = panel.read_with(cx, |panel, _| panel.terminal_add_focus.clone());
        let filter = panel.read_with(cx, |panel, _| panel.log_filter_input.clone());
        let _ = filter;
        cx.update(|window, cx| window.focus(&add_focus, cx));
        cx.run_until_parked();

        let sink = terminals.borrow().sink.clone().expect("sink");
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(1),
                    signal: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            cx.update(|window, _| add_focus.is_focused(window)),
            "an async exit must not move the keyboard away from the control in use"
        );
    }

    /// The forward strip owns its scroll: it caps its rows, scrolls from the keyboard, and leaves
    /// the log list where it was.
    #[gpui::test]
    fn forwards_strip_scrolls_on_its_own(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        for index in 0..7 {
            panel.update(cx, |panel, cx| {
                panel
                    .start_forward(
                        ForwardRequest {
                            context: Some("kind-dev".to_owned()),
                            namespace: Some("default".to_owned()),
                            name: format!("web-{index}").into(),
                            remote_port: 8080 + index as u16,
                            local_port: None,
                        },
                        cx,
                    )
                    .expect("start forward");
            });
        }
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let list = cx
            .debug_bounds("dock-forwards-list")
            .expect("forwards list is laid out");
        assert!(
            (f32::from(list.size.height) - f32::from(design::size::ROW) * FORWARDS_VISIBLE_ROWS)
                .abs()
                <= 1.0,
            "the strip keeps four rows and scrolls the rest"
        );
        let scroll = panel.read_with(cx, |panel, _| panel.forwards_scroll.clone());
        assert!(
            scroll.max_offset().y > px(0.),
            "the strip has more rows than it shows"
        );
        assert_eq!(scroll.top_item(), 0);

        let focus = panel.read_with(cx, |panel, _| panel.forwards_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        assert_eq!(
            scroll.offset().y,
            -scroll.max_offset().y,
            "the keyboard reaches the last forward"
        );
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            scroll.offset().y,
            px(0.),
            "the keyboard returns to the first forward"
        );
    }

    /// A failed forward drops the port it used to bind, in the model as well as in the row.
    #[gpui::test]
    fn failed_forward_drops_the_bound_port(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel
                .start_forward(
                    ForwardRequest {
                        context: Some("kind-dev".to_owned()),
                        namespace: Some("default".to_owned()),
                        name: "web-0".into(),
                        remote_port: 8080,
                        local_port: None,
                    },
                    cx,
                )
                .expect("start");
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.phase, ForwardPhase::Running);
            assert_eq!(snapshot.local_port, Some(4321));
        });

        forwards
            .borrow()
            .errors
            .clone()
            .expect("error stream")
            .send("connection lost".to_owned())
            .ok();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let entry = &panel.forwards[0];
            assert_eq!(entry.phase, ForwardPhase::Failed);
            assert_eq!(
                entry.local_port, None,
                "a stopped forward no longer owns the port"
            );
            assert_eq!(forward_live_port(entry), None);
            assert_eq!(forward_row_target(entry), "kind-dev/default/Pod/web-0:8080");
        });
    }

    /// The Tail menu, Load More History, and the cap notice must offer the same ladder.
    #[gpui::test]
    fn history_ladder_is_shared_by_every_entry_point(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("a".into())], cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.history_lines(), TailLines::FiveHundred.value());
        });

        let mut seen = vec![panel.read_with(cx, |panel, _| panel.history_lines())];
        for _ in 0..TailLines::ALL.len() {
            panel.update(cx, |panel, cx| panel.load_earlier(cx));
            cx.run_until_parked();
            seen.push(panel.read_with(cx, |panel, _| panel.history_lines()));
        }
        assert_eq!(
            &seen[..4],
            &[500, 2_000, 5_000, history_cap()],
            "Load More History walks the shared ladder"
        );
        assert!(
            seen[4..].iter().all(|value| *value == history_cap()),
            "Load More History stops at the capacity: {seen:?}"
        );
        assert_eq!(history_cap(), TailLines::cap().value());
        let selectable: Vec<i64> = TailLines::ALL.map(TailLines::value).to_vec();
        for step in selectable {
            panel.update(cx, |panel, cx| {
                panel.set_tail(TailLines::from_value(step).expect("ladder step"), cx)
            });
            cx.run_until_parked();
            assert_eq!(panel.read_with(cx, |panel, _| panel.history_lines()), step);
            assert_eq!(
                TailLines::ALL
                    .into_iter()
                    .filter(|option| option.value() == step)
                    .count(),
                1,
                "the Tail menu keeps exactly one entry checked at {step}"
            );
        }
    }

    /// A compact no-wrap row must be as wide as the content it draws, or horizontal scrolling
    /// spends a screen of empty space on columns the compact layout dropped.
    #[gpui::test]
    fn compact_nowrap_row_matches_its_reserved_width(cx: &mut TestAppContext) {
        let (panel, state, cx) =
            setup_in_dock(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        push(
            &state,
            vec![
                LogEvent::Line("2026-09-22T21:14:02.331Z INFO ready".into()),
                LogEvent::Line(format!(
                    "INFO {}",
                    "x".repeat(LOG_COPY_COLUMN_THRESHOLD + 1)
                )),
            ],
            cx,
        );
        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let (expected, wide) = panel.read_with(cx, |panel, cx| {
            let line = panel.buffer.line(1).expect("line");
            let reserve = panel.buffer.timestamp_column_reserve();
            let typography = crate::settings::data_typography(cx);
            (
                log_row_width(&line, true, reserve, &typography),
                log_row_width(&line, false, reserve, &typography),
            )
        });
        let drawn = cx.debug_bounds("dock-log-row").expect("nowrap row");
        assert!(
            (f32::from(drawn.size.width) - f32::from(expected)).abs() <= 1.0,
            "the row drew {} but reserved {expected}",
            drawn.size.width
        );
        assert!(
            expected < wide,
            "a compact row gives back the columns the compact layout dropped: \
             {expected:?} against {wide:?}"
        );
        // The copy control fills the line and uses the width the row reserves for it.
        let copy = cx.debug_bounds("dock-log-copy").expect("copy control");
        let row_height = cx.update(|_, cx| log_row_height(cx));
        assert!((f32::from(copy.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(copy.size.width) - f32::from(design::size::CONTROL)).abs() <= 1.0);
    }

    #[gpui::test]
    fn nowrap_follow_tracks_appends_reopen_and_restart(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        push(
            &state,
            (0..100)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        let scroll = panel.read_with(cx, |panel, _| panel.scroll_handle.clone());
        assert_eq!(scroll.is_scrolled_to_end(), Some(true));
        assert!(panel.read_with(cx, |panel, _| panel.is_following()));

        scroll
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(0.)));
        panel.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.is_following()));

        push(&state, vec![LogEvent::Line("line 100".into())], cx);
        assert!(!panel.read_with(cx, |panel, _| panel.is_following()));
        assert_ne!(scroll.is_scrolled_to_end(), Some(true));

        panel.update(cx, |panel, cx| panel.set_follow(true, cx));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.is_following()));
        assert_eq!(scroll.is_scrolled_to_end(), Some(true));

        panel.update(cx, |panel, cx| panel.open_logs(request(), cx));
        cx.run_until_parked();
        push(
            &state,
            (200..300)
                .map(|index| LogEvent::Line(format!("reopened {index}")))
                .collect(),
            cx,
        );
        assert_eq!(scroll.is_scrolled_to_end(), Some(true));

        panel.update(cx, |panel, cx| panel.set_tail(TailLines::TwoThousand, cx));
        cx.run_until_parked();
        push(
            &state,
            (400..500)
                .map(|index| LogEvent::Line(format!("restarted {index}")))
                .collect(),
            cx,
        );
        assert_eq!(scroll.is_scrolled_to_end(), Some(true));
    }

    #[gpui::test]
    fn log_content_scrolls_with_keyboard_only(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..100)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.list_state.logical_scroll_top().item_ix),
            0
        );
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.list_state.logical_scroll_top().item_ix
                > 0)
        );
    }

    #[gpui::test]
    fn tab_arrows_move_selection_and_focus(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        let handles = panel.read_with(cx, |panel, _| panel.tab_focus_handles.clone());
        cx.update(|window, cx| window.focus(&handles[0], cx));
        cx.simulate_keystrokes("right");
        assert_eq!(panel.read_with(cx, |panel, _| panel.active_tab), 1);
        assert!(cx.update(|window, _| handles[1].is_focused(window)));
    }

    #[gpui::test]
    fn idle_log_target_does_not_show_a_status_chip(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| panel.phase = LogPhase::Idle);
        assert!(!panel.read_with(cx, |panel, _| panel.should_show_log_status()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_status_label()),
            None
        );
        panel.update(cx, |panel, _| panel.phase = LogPhase::Streaming);
        assert!(panel.read_with(cx, |panel, _| panel.should_show_log_status()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_status_label()),
            Some("Live")
        );
    }

    /// The stream keeps running on the Terminal tab and behind a maximised pane, so its state
    /// must not be tied to the Logs tab.
    #[gpui::test]
    fn log_status_survives_the_terminal_tab_and_a_maximised_pane(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel.open_logs(request(), cx);
            panel.set_lines(vec!["INFO ready".to_owned()], cx);
        });
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        panel.update(cx, |panel, cx| panel.show_terminal_tab(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.should_show_log_status()),
            "the chip stays on the Terminal tab while the stream runs"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "the chip is drawn next to the tab bar on the Terminal tab"
        );

        panel.update(cx, |panel, cx| panel.toggle_terminal_maximized(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-tabs-row").is_none(),
            "a maximised terminal hides the tab bar"
        );
        assert!(
            cx.debug_bounds("dock-close").is_none(),
            "the close control belongs to that header, so it hides with it"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "the state moves into the toolbar that stays on screen"
        );
    }

    /// "Following" was only an unselected button. Scrolling away now has a visible state.
    #[gpui::test]
    fn follow_paused_is_visible_after_scrolling_away(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..100)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-follow-paused").is_none(),
            "a view that follows the tail says nothing"
        );
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| !panel.is_following()));
        assert!(
            cx.debug_bounds("dock-log-follow-paused").is_some(),
            "the paused follow state must be visible, not only implied by the button"
        );
    }

    /// A short line has no per-line button to fall back on, so the keyboard path is the path.
    #[gpui::test]
    fn a_short_log_line_is_selectable_and_copyable_by_keyboard(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![
                LogEvent::Line("short".into()),
                LogEvent::Line("second".into()),
                LogEvent::Line("third".into()),
            ],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let copy = cx.debug_bounds("dock-log-copy").expect("copy control");
        assert!(
            (f32::from(copy.size.width) - f32::from(design::size::CONTROL)).abs() <= 1.0,
            "the copy control is drawn whatever the line width"
        );

        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("shift-down");
        cx.simulate_keystrokes("shift-down");
        panel.read_with(cx, |panel, _| {
            let selection = panel.log_selection.expect("the list took the caret");
            assert_eq!(selection.anchor, 0);
            assert_eq!(selection.head, 2);
            assert_eq!(
                panel.log_selection_text().as_deref(),
                Some("short\nsecond\nthird\n")
            );
        });
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("short\nsecond\nthird\n".to_owned())
        );
    }

    /// A long line wraps onto several visual rows. The selection still counts lines, not rows.
    #[gpui::test]
    fn wrapped_long_lines_do_not_break_the_selection(cx: &mut TestAppContext) {
        let (panel, state, cx) =
            setup_in_dock(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        let long = "y".repeat(400);
        push(
            &state,
            vec![
                LogEvent::Line(format!("INFO {long}")),
                LogEvent::Line("tail".into()),
            ],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("shift-down");
        panel.read_with(cx, |panel, _| {
            let selection = panel.log_selection.expect("caret");
            assert_eq!(selection.bounds(), (0, 1));
            let text = panel.log_selection_text().expect("selection text");
            assert_eq!(
                text.lines().count(),
                2,
                "one entry per line, not per visual row"
            );
        });
    }

    /// The filter must not rescan the buffer for every batch. A pass only reads the lines that
    /// arrived since the last one, so the cursor is what proves it.
    #[gpui::test]
    fn log_filter_scores_only_the_new_lines_per_batch(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        let lines = |from: usize, to: usize| -> Vec<LogEvent> {
            (from..to)
                .map(|index| LogEvent::Line(format!("INFO m32-line {index}")))
                .collect()
        };
        push(&state, lines(0, FILTER_SCORING_BUDGET + 500), cx);
        // Setting the filter notifies, and every frame that follows continues the pass, so the
        // budget is asserted inside the update that spends it, before any frame can add to it.
        panel.update(cx, |panel, cx| {
            panel.set_log_filter("m32-line", cx);
            assert_eq!(
                panel.filter_cursor, FILTER_SCORING_BUDGET,
                "one pass scores one budget"
            );
            assert_eq!(
                panel.visible_log_indices.len(),
                FILTER_SCORING_BUDGET,
                "the first pass stops at its budget"
            );
        });
        // Drain the rest of the pass, so the next batch is the only work left to account for.
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        let scored = panel.read_with(cx, |panel, _| panel.visible_log_indices.len());
        assert_eq!(scored, FILTER_SCORING_BUDGET + 500);
        push(
            &state,
            lines(FILTER_SCORING_BUDGET + 500, FILTER_SCORING_BUDGET + 600),
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.visible_log_indices.len() - scored,
                100,
                "the next batch costs only its own lines, a rescan would add every match twice"
            );
            assert_eq!(panel.filter_cursor, panel.buffer.len());
        });
    }

    /// A filter pass continues on the following frames until the buffer is scored.
    #[gpui::test]
    fn log_filter_continues_until_the_buffer_is_scored(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..FILTER_SCORING_BUDGET * 2 + 7)
                .map(|index| LogEvent::Line(format!("INFO needle {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, cx| panel.set_log_filter("needle", cx));
        cx.run_until_parked();
        // The render loop continues the pass one budget per frame. Drive it directly as well so
        // the assertion does not depend on how many frames the test window drew.
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.filter_cursor, panel.buffer.len());
            assert_eq!(panel.visible_log_count(), panel.buffer.len());
        });
        // `run_filter_pass` mutates the cursor, so the drained check needs a mutable borrow.
        panel.update(cx, |panel, cx| {
            assert!(
                !panel.run_filter_pass(log_row_height(cx)),
                "nothing is left to score"
            );
        });
    }

    /// A pass must not chain frames without a limit. A live stream appends faster than one budget
    /// can score, so the cursor never catches up and every frame asks for another one: the window
    /// would redraw forever with the stream running. The pass stops asking on its budget and waits
    /// for the next batch instead.
    #[gpui::test]
    fn a_filter_pass_stops_asking_for_frames_on_its_budget(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..FILTER_SCORING_BUDGET * 4)
                .map(|index| LogEvent::Line(format!("INFO needle {index}")))
                .collect(),
            cx,
        );
        // One frame of budget, so the exhaustion is reachable without a ring-sized buffer. The
        // query resets the budget, so it is set after the query lands.
        panel.update(cx, |panel, cx| {
            panel.set_log_filter("needle", cx);
            panel.filter_frames_left = 1;
            assert!(
                panel.continue_filter_pass(cx),
                "the first frame is inside budget"
            );
            assert!(
                !panel.continue_filter_pass(cx),
                "the pass must stop asking once its budget is spent"
            );
            assert!(
                panel.filter_pass_deferred,
                "the pass is throttled, not finished"
            );
            assert_eq!(
                panel.filter_cursor,
                FILTER_SCORING_BUDGET * 3,
                "a deferred pass stops where the budget stopped it"
            );
        });

        // The next batch resumes it, so a throttled pass is not a stalled one.
        push(
            &state,
            (FILTER_SCORING_BUDGET * 4..FILTER_SCORING_BUDGET * 4 + 10)
                .map(|index| LogEvent::Line(format!("INFO needle {index}")))
                .collect(),
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.filter_cursor,
                FILTER_SCORING_BUDGET * 4 + 10,
                "the batch scored the lines that arrived"
            );
            assert_eq!(
                panel.visible_log_indices.len(),
                FILTER_SCORING_BUDGET * 4 + 10
            );
        });
        panel.update(cx, |panel, cx| {
            assert!(
                !panel.continue_filter_pass(cx),
                "the pass has nothing left to score"
            );
            assert!(
                !panel.filter_pass_deferred,
                "a finished pass is no longer throttled"
            );
            assert_eq!(
                panel.filter_frames_left, FILTER_PASS_FRAME_BUDGET,
                "the next pass starts on a full budget"
            );
        });
    }

    /// A filter over the whole retained history finishes without waiting for the log stream, so
    /// the frame budget is wide enough to cover the ring.
    #[gpui::test]
    fn the_filter_budget_covers_the_whole_ring(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..RING_CAPACITY)
                .map(|index| LogEvent::Line(format!("INFO ring {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, cx| panel.set_log_filter("ring", cx));
        panel.update(cx, |panel, cx| {
            while panel.continue_filter_pass(cx) {}
            assert_eq!(
                panel.filter_cursor,
                panel.buffer.len(),
                "the pass scored the ring inside its frame budget"
            );
            assert!(!panel.filter_pass_deferred);
        });
    }

    /// Opening a new target starts its own pass. The match set and the buffer are cleared, so the
    /// cursor has to be cleared with them: a cursor left at the end of the old buffer would skip
    /// the first lines of the new one.
    #[gpui::test]
    fn opening_a_log_target_restarts_the_filter_cursor(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..FILTER_SCORING_BUDGET + 100)
                .map(|index| LogEvent::Line(format!("INFO first {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, cx| panel.set_log_filter("first", cx));
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.filter_cursor, panel.buffer.len());
        });

        panel.update(cx, |panel, cx| panel.open_logs(request(), cx));
        push(
            &state,
            (0..200)
                .map(|index| LogEvent::Line(format!("INFO second {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, cx| panel.set_log_filter("second", cx));
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.visible_log_indices.len(),
                200,
                "the new target scored its own lines, not the tail of the old one"
            );
        });
    }

    /// Ring eviction rebases the stored match set instead of losing it.
    #[gpui::test]
    fn log_filter_rebases_when_the_ring_drops_lines(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            let raws = (0..RING_CAPACITY + 20)
                .map(|index| format!("INFO keep me {index}"))
                .collect::<Vec<_>>();
            panel.append_log_lines(raws);
        });
        panel.update(cx, |panel, cx| panel.set_log_filter("keep me", cx));
        // The render loop continues the pass one budget per frame, so the whole buffer is scored
        // here rather than depending on how many frames the test window drew.
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.filter_cursor, panel.buffer.len());
            assert_eq!(panel.visible_log_indices.len(), RING_CAPACITY);
            assert_eq!(
                *panel.visible_log_indices.last().expect("last match"),
                RING_CAPACITY - 1
            );
        });
        // The tail goes through the stream, so the ring eviction and the rebase run together.
        push(
            &state,
            (0..30)
                .map(|index| LogEvent::Line(format!("INFO keep me tail {index}")))
                .collect(),
            cx,
        );
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.buffer.len(), RING_CAPACITY);
            assert!(
                panel
                    .visible_log_indices
                    .iter()
                    .all(|index| *index < panel.buffer.len()),
                "every stored index still points at a buffered line"
            );
            assert!(
                panel
                    .visible_log_indices
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            );
            assert_eq!(
                panel.visible_log_indices.len(),
                RING_CAPACITY,
                "the rebased set keeps every match, the 30 that arrived included"
            );
            assert_eq!(
                *panel.visible_log_indices.last().expect("last match"),
                RING_CAPACITY - 1
            );
        });
    }

    /// Severity is parsed from the first token, so the level scope is the only way to ask for
    /// warnings or errors without typing their labels.
    #[gpui::test]
    fn log_level_scope_filters_by_parsed_severity(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![
                LogEvent::Line("INFO ready".into()),
                LogEvent::Line("WARN slow upstream".into()),
                LogEvent::Line("ERROR upstream failed".into()),
                LogEvent::Line("plain output".into()),
            ],
            cx,
        );
        panel.update(cx, |panel, cx| {
            panel.set_log_level(LogLevelScope::Error, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_indices, vec![2]);
        });
        panel.update(cx, |panel, cx| {
            panel.set_log_level(LogLevelScope::Warning, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_indices, vec![1, 2]);
        });
        panel.update(cx, |panel, cx| panel.set_log_level(LogLevelScope::All, cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.visible_log_indices.is_empty());
            assert_eq!(panel.visible_log_count(), 4);
        });
        // The scope and the free-text filter compose.
        panel.update(cx, |panel, cx| {
            panel.set_log_level(LogLevelScope::Warning, cx)
        });
        panel.update(cx, |panel, cx| panel.set_log_filter("upstream", cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_indices, vec![1, 2]);
        });
    }

    /// The ring buffer drops the oldest lines silently. The toolbar now says so.
    #[gpui::test]
    fn dropped_log_lines_are_reported(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("only line".into())], cx);
        assert_eq!(panel.read_with(cx, |panel, _| panel.dropped_lines), 0);
        assert!(cx.debug_bounds("dock-log-dropped-status").is_none());
        push(
            &state,
            (0..RING_CAPACITY + 12)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.dropped_lines > 0,
                "the ring eviction is counted, not swallowed"
            );
        });
        assert!(
            cx.debug_bounds("dock-log-dropped-status").is_some(),
            "the drop is visible without opening a menu"
        );
    }

    /// The token now covers the fixed chrome plus three log rows, and a Dock squeezed below it
    /// says so instead of showing half a row of text.
    #[gpui::test]
    fn a_squeezed_dock_says_instead_of_showing_half_a_row(cx: &mut TestAppContext) {
        init_app(cx);
        // `DOCK_MIN` is stated for the product default data font, while the
        // harness installs the upstream Base theme and its buffer font is larger.
        // So the two facts are checked separately: the token equals the
        // derivation at the product default, and the live derivation clears it at
        // whatever font is actually installed. Asserting the token against a live
        // number would compare 12px against 16px and call the disagreement a
        // layout bug.
        assert_eq!(
            design::size::DOCK_MIN,
            dock_chrome_height() + design::text::DATA_LINE_HEIGHT * 3.,
            "the token carries the fixed chrome plus three product-default log rows"
        );
        let recommended = cx.update(|cx| dock_min_recommended(cx));
        assert!(
            recommended >= design::size::DOCK_MIN,
            "the Dock must be at least its floor, and never less than the product default: \
             {recommended:?} against {:?}",
            design::size::DOCK_MIN
        );
        // A Dock narrower than the window and 100px tall: the harness only sizes the Dock when it
        // gets both, so a `None` width would leave it filling the resized window instead.
        let (panel, state, cx) = setup_in_dock(cx, Some(WIDE_DOCK_WIDTH), Some(px(100.)));
        push(
            &state,
            (0..200)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.log_body_is_too_short()));
        assert!(cx.debug_bounds("dock-log-row").is_none());
        assert!(cx.debug_bounds("empty-state").is_some());
    }

    #[gpui::test]
    fn streaming_appends_lines_and_follows(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![
                LogEvent::Line("2026-09-23T10:00:00Z INFO ready".into()),
                LogEvent::Line("second line".into()),
            ],
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert!(matches!(panel.phase(), LogPhase::Streaming));
            assert_eq!(panel.buffer.len(), 2);
            assert_eq!(panel.synced, 2);
            assert_eq!(panel.buffer.line(0).expect("line").label.as_ref(), "INFO");
            assert_eq!(
                panel.buffer.line(1).expect("line").severity,
                Severity::Muted
            );
        });
    }

    #[gpui::test]
    fn log_filter_is_local_tracks_stream_and_clears_with_escape(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![
                LogEvent::Line("INFO ready".into()),
                LogEvent::Line("WARN upstream timeout".into()),
            ],
            cx,
        );
        let filter_bounds = cx
            .debug_bounds("shared-text-input")
            .expect("log filter is laid out");
        let filter_focus = panel.read_with(cx, |panel, cx| {
            panel.log_filter_input.read(cx).focus_handle(cx)
        });
        assert!(filter_focus.tab_stop);
        cx.simulate_click(filter_bounds.center(), gpui::Modifiers::none());
        assert!(cx.update(|window, _| filter_focus.is_focused(window)));
        cx.simulate_input("warn");
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel
                .log_filter_input
                .read(cx)
                .text()
                .to_owned()),
            "warn"
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.log_filter, "warn");
            assert_eq!(panel.visible_log_count(), 1);
            assert_eq!(panel.visible_log_indices, vec![1]);
            assert_eq!(panel.buffer.len(), 2);
            assert!(panel.is_following());
        });
        assert_eq!(state.borrow().restarts, 1);

        push(
            &state,
            vec![
                LogEvent::Line("INFO still healthy".into()),
                LogEvent::Line("WARN upstream recovered".into()),
            ],
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_indices, vec![1, 3]);
            assert!(panel.is_following());
        });
        assert_eq!(state.borrow().restarts, 1);

        panel.update(cx, |panel, cx| panel.set_follow(false, cx));
        push(
            &state,
            vec![LogEvent::Line("WARN upstream still recovering".into())],
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.visible_log_indices, vec![1, 3, 4]);
            assert!(!panel.is_following());
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.log_filter.is_empty());
            assert_eq!(panel.visible_log_count(), 5);
            assert!(panel.visible_log_indices.is_empty());
        });
    }

    #[gpui::test]
    fn ended_stream_schedules_a_reconnect(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Ended("boom".into())], cx);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.phase(),
                LogPhase::Reconnecting { attempt: 1, .. }
            ));
        });
        // Reconnect after the scheduled delay.
        cx.executor()
            .advance_clock(RECONNECT_BASE + Duration::from_millis(1));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(matches!(panel.phase(), LogPhase::Connecting));
        });
        assert_eq!(state.borrow().restarts, 2);
    }

    #[gpui::test]
    fn tail_and_container_changes_restart_with_new_options(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("a".into())], cx);
        panel.update(cx, |panel, cx| panel.set_tail(TailLines::TwoThousand, cx));
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.set_container("sidecar".into(), cx));
        cx.run_until_parked();

        let state = state.borrow();
        assert_eq!(
            state.restarts, 3,
            "initial start + tail change + container change"
        );
        assert_eq!(state.options[2].tail_lines, Some(2000));
        assert_eq!(state.options[2].container.as_deref(), Some("sidecar"));
        assert!(
            panel.read_with(cx, |panel, _| panel.buffer.is_empty()),
            "a stream change must clear old lines"
        );
    }

    /// Load More History grows history one step at a time.
    #[gpui::test]
    fn load_earlier_grows_history_step_by_step(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("a".into())], cx);

        panel.update(cx, |panel, cx| panel.load_earlier(cx));
        cx.run_until_parked();
        assert_eq!(state.borrow().options[1].tail_lines, Some(2_000));
        assert_eq!(state.borrow().restarts, 2);

        panel.update(cx, |panel, cx| panel.load_earlier(cx));
        cx.run_until_parked();
        assert_eq!(state.borrow().options[2].tail_lines, Some(5_000));

        for _ in 0..8 {
            panel.update(cx, |panel, cx| panel.load_earlier(cx));
            cx.run_until_parked();
        }
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.history_lines(), history_cap())
        });
        assert_eq!(
            state
                .borrow()
                .options
                .last()
                .expect("last history step")
                .tail_lines,
            Some(history_cap()),
            "The history limit remains capped."
        );
        assert_eq!(
            state.borrow().restarts,
            4,
            "initial start + three history steps, then no restart"
        );
        assert_eq!(history_cap(), RING_CAPACITY as i64);
    }

    #[gpui::test]
    fn pause_turns_follow_off_and_resume_restores_it(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, cx| panel.toggle_pause(cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.is_paused());
            assert!(!panel.is_following());
        });
        panel.update(cx, |panel, cx| panel.toggle_pause(cx));
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_paused());
            assert!(panel.is_following());
        });
    }

    #[gpui::test]
    fn failed_stream_cannot_be_paused(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, cx| {
            panel.phase = LogPhase::Failed {
                reason: "connection reset".to_owned(),
            };
            panel.toggle_pause(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(!panel.is_paused());
            assert!(panel.is_following());
        });
    }

    /// The failure the log source reports for a Pod that is still Pending: no container has a
    /// status yet, so the Pod is not missing.
    const PENDING_POD_REASON: &str = "The log request failed: containerStatuses is empty for pod \
         \"web-0\". Check the Pod, cluster connection, and access permissions, then try again.";

    /// A Pod that has not started yet is not a Pod that is gone, and the wording must not send
    /// the user looking for a Pod that is still there.
    #[gpui::test]
    fn a_pending_pod_names_the_container_and_not_a_missing_pod(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.phase = LogPhase::Failed {
                reason: PENDING_POD_REASON.to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let notice = panel.log_failure_notice().expect("a failure to report");
            assert_eq!(notice.title, "No Container");
            let guidance = notice.guidance.to_ascii_lowercase();
            assert!(
                guidance.contains("no container can stream yet"),
                "{guidance}"
            );
            assert!(guidance.contains("retry"), "{guidance}");
            for claim in ["gone", "exist", "deleted", "not found"] {
                assert!(
                    !guidance.contains(claim),
                    "a Pending Pod must not be reported as {claim}: {guidance}"
                );
            }
            assert_eq!(notice.severity, Severity::Warning);
            assert!(notice.retry, "waiting for a container is worth a retry");
            assert_eq!(notice.detail.as_deref(), Some(PENDING_POD_REASON));
        });
        assert!(
            cx.debug_bounds("empty-state-block").is_some(),
            "an empty log body reports the failure where the lines would be"
        );
        assert!(
            cx.debug_bounds("dock-log-banner").is_none(),
            "the banner would say the same thing one row above"
        );
    }

    /// One state, one place. The banner, the header chip, and the status bar all watched the same
    /// phase, so a failure was read out three times over.
    #[gpui::test]
    fn a_failed_stream_is_reported_by_one_surface(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.phase = LogPhase::Failed {
                reason: PENDING_POD_REASON.to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.log_body_carries_state(),
                "an empty log body is the surface that reports the failure"
            );
            assert!(
                panel.log_body_reports_failure(),
                "the body names the failure, so the chip has nothing left to add"
            );
            assert!(
                !panel.header_status_is_useful(),
                "the chip would repeat the state the body just reported"
            );
        });
        assert!(
            cx.debug_bounds("empty-state-block").is_some(),
            "the log body is the entry point"
        );
        assert!(
            cx.debug_bounds("dock-log-banner").is_none(),
            "the banner steps aside while the empty state carries the failure"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_none(),
            "the chip steps aside for the same reason"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_status_label()),
            Some("Failed"),
            "the status bar keeps the transport state, which is all it has room for"
        );

        // Buffered lines move the failure into the banner, and the chip still steps aside.
        push(&state, vec![LogEvent::Line("INFO ready".into())], cx);
        panel.update(cx, |panel, _| {
            panel.phase = LogPhase::Failed {
                reason: PENDING_POD_REASON.to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-banner").is_some(),
            "with lines to read, the banner is the entry point"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_none(),
            "the chip must not repeat the banner above it"
        );

        // The Terminal tab hides the log body, so the chip is the Dock-local word again.
        panel.update(cx, |panel, cx| panel.show_terminal_tab(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "the chip carries the state while the banner is off screen"
        );
        assert!(panel.read_with(cx, |panel, _| panel.should_show_log_status()));
    }

    /// Four classes, four words, four next steps: a failure must not borrow the sentence of
    /// another one.
    #[gpui::test]
    fn every_failure_class_reports_its_own_next_step(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        for (reason, title, severity) in [
            ("pods \"web-0\" not found", "Pod Missing", Severity::Warning),
            (PENDING_POD_REASON, "No Container", Severity::Warning),
            (
                "pods \"web-0\" is forbidden: User \"dev\" cannot get resource \"pods/log\"",
                "Access Denied",
                Severity::Error,
            ),
            (
                "The server rejected our request for an unknown reason",
                "Log Request Failed",
                Severity::Error,
            ),
        ] {
            panel.update(cx, |panel, _| {
                panel.phase = LogPhase::Failed {
                    reason: reason.to_owned(),
                };
            });
            panel.read_with(cx, |panel, _| {
                let notice = panel.log_failure_notice().expect("a failure to report");
                assert_eq!(notice.title, title, "{reason}");
                assert_eq!(notice.severity, severity, "{reason}");
                assert_eq!(notice.detail.as_deref(), Some(reason));
                assert!(notice.retry, "{reason} offers a retry");
            });
        }
        panel.update(cx, |panel, _| {
            panel.phase = LogPhase::Failed {
                reason: "pods \"web-0\" not found".to_owned(),
            };
        });
        panel.read_with(cx, |panel, _| {
            let notice = panel.log_failure_notice().expect("a failure to report");
            let guidance = notice.guidance.to_ascii_lowercase();
            assert_eq!(notice.title, "Pod Missing");
            assert!(
                guidance.contains("no longer has this pod"),
                "a Pod the cluster cannot find is the only failure that says so: {guidance}"
            );
            assert!(
                guidance.contains("refresh"),
                "the step for a missing Pod is to refresh the list: {guidance}"
            );
        });
    }

    #[gpui::test]
    fn clear_keeps_streaming_and_empties_the_buffer(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("a".into())], cx);
        panel.update(cx, |panel, cx| panel.clear(cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.buffer.is_empty());
            assert_eq!(panel.synced, 0);
            assert!(matches!(panel.phase(), LogPhase::Streaming));
        });
        push(&state, vec![LogEvent::Line("b".into())], cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.buffer.len(),
                1,
                "the stream continues after the buffer is cleared"
            );
        });
    }

    /// Processes 100,000 lines in batches and caps the buffer at 10,000 lines.
    /// Prints elapsed time for manual performance checks.
    #[gpui::test]
    fn burst_of_one_hundred_thousand_lines_is_buffered(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        let events: Vec<LogEvent> = (0..100_000)
            .map(|i| {
                LogEvent::Line(format!(
                    "2026-09-23T10:{:02}:{:02}Z INFO m32-line {i}",
                    (i / 60) % 60,
                    i % 60
                ))
            })
            .collect();

        let started = Instant::now();
        push(&state, events, cx);
        let elapsed = started.elapsed();
        println!("100k line throughput: {elapsed:?}");

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.buffer.len(), crate::panels::logs::RING_CAPACITY);
            assert!(panel.buffer_bytes <= LOG_BUFFER_MAX_BYTES);
            assert_eq!(panel.lines_received, 100_000);
            assert!(matches!(panel.phase(), LogPhase::Streaming));
        });
        assert!(
            elapsed < Duration::from_secs(10),
            "100,000 lines must not block the UI (actual time: {elapsed:?})"
        );
    }

    /// Uses the download filename pattern.
    #[gpui::test]
    fn suggested_filename_follows_the_download_convention(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        let name = panel.read_with(cx, |panel, _| panel.suggested_filename());
        assert!(name.starts_with("k8s-gpui-web-0-"), "{name}");
        assert!(name.ends_with(".log"), "{name}");
        let stamp = &name["k8s-gpui-web-0-".len()..name.len() - ".log".len()];
        assert_eq!(stamp.len(), "YYYYMMDD-HHMMSS".len(), "{name}");
        assert!(stamp.chars().all(|ch| ch.is_ascii_digit() || ch == '-'));
    }

    /// Creates a unique scratch directory and removes it when the test ends.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "k8s-gpui-log-export-{label}-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).expect("create scratch dir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sanitize_filename_component_stays_one_path_component() {
        assert_eq!(sanitize_filename_component("web-0"), "web-0");
        assert_eq!(
            sanitize_filename_component("../../etc/passwd"),
            "etc-passwd"
        );
        assert_eq!(sanitize_filename_component(".."), "logs");
        assert_eq!(sanitize_filename_component(""), "logs");
        assert_eq!(sanitize_filename_component("...."), "logs");
        assert_eq!(sanitize_filename_component("a b\tc"), "a-b-c");
        for raw in ["../../etc/passwd", "..", "a/b", "a\\b", "pod\u{202e}name"] {
            let sanitized = sanitize_filename_component(raw);
            assert!(!sanitized.is_empty(), "{raw}");
            assert!(!sanitized.contains('/'), "{raw} -> {sanitized}");
            assert!(!sanitized.contains('\\'), "{raw} -> {sanitized}");
            assert!(!sanitized.contains(".."), "{raw} -> {sanitized}");
        }
        let long = sanitize_filename_component(&"n".repeat(200));
        assert_eq!(long.chars().count(), 64);
    }

    #[test]
    fn export_log_file_writes_owner_only_content() {
        let root = TestDir::new("owner-only");
        let directory = root.path().join("logs");
        let path = export_log_file(&directory, "k8s-gpui-web-0-20260926-101500.log", b"line\n")
            .expect("export");

        assert_eq!(path, directory.join("k8s-gpui-web-0-20260926-101500.log"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "line\n");

        let entries: Vec<String> = std::fs::read_dir(&directory)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            entries,
            vec!["k8s-gpui-web-0-20260926-101500.log".to_owned()],
            "the atomic staging directory must not survive the export"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o777, 0o600, "{path:?}");
            let dir_mode = std::fs::metadata(&directory)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "{directory:?}");
        }
    }

    #[test]
    fn export_log_file_replaces_content_without_widening_permissions() {
        let root = TestDir::new("replace");
        let name = "k8s-gpui-pod-20260926-101500.log";
        let path = root.path().join(name);
        export_log_file(root.path(), name, b"first").expect("first export");
        export_log_file(root.path(), name, b"second").expect("second export");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("widen");
            export_log_file(root.path(), name, b"third").expect("third export");
            assert_eq!(std::fs::read_to_string(&path).expect("read"), "third");
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "re-export must not keep 0644");
        }
    }

    #[test]
    fn export_log_file_keeps_an_existing_directory_untouched() {
        let root = TestDir::new("existing-dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
                .expect("widen");
        }
        export_log_file(root.path(), "k8s-gpui-pod-20260926-101500.log", b"line").expect("export");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.path())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o755,
                "a user-owned download directory must keep its mode"
            );
        }
    }

    #[test]
    fn reconnect_backoff_grows_and_caps() {
        assert_eq!(reconnect_delay(1), Duration::from_millis(1_000));
        assert_eq!(reconnect_delay(2), Duration::from_millis(2_000));
        assert_eq!(reconnect_delay(5), Duration::from_millis(10_000));
        assert_eq!(reconnect_delay(9), Duration::from_millis(10_000));
    }

    #[test]
    fn counts_are_thousands_separated() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234), "1,234");
    }

    // Terminal sessions and port forwards.

    struct FakeTerminalView {
        focus_handle: FocusHandle,
    }

    impl Focusable for FakeTerminalView {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl gpui::Render for FakeTerminalView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .debug_selector(|| "fake-terminal-view".to_owned())
                .track_focus(&self.focus_handle)
        }
    }

    #[derive(Default)]
    struct FakeTerminals {
        sink: Option<Rc<TerminalEventSink>>,
        /// Every sink the factory handed out, in creation order.
        sinks: Vec<Rc<TerminalEventSink>>,
        opened: usize,
        requests: Vec<TerminalRequest>,
    }

    struct FakeForwardHandle {
        state: Rc<RefCell<FakeForwards>>,
    }

    #[derive(Default)]
    struct FakeForwards {
        starts: usize,
        stopped: usize,
        errors: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        binding: Option<tokio::sync::oneshot::Sender<Result<u16, String>>>,
        last_request: Option<ForwardRequest>,
        fail: Option<String>,
        pending: bool,
    }

    impl ForwardHandle for FakeForwardHandle {
        fn stop(&mut self) {
            self.state.borrow_mut().stopped += 1;
        }
    }

    fn fake_services(
        terminals: Rc<RefCell<FakeTerminals>>,
        forwards: Rc<RefCell<FakeForwards>>,
    ) -> TerminalServices {
        let terminal_state = Rc::clone(&terminals);
        let factory: crate::panels::TerminalFactory = Rc::new(move |request, sink, cx| {
            let mut state = terminal_state.borrow_mut();
            let sink = Rc::new(sink);
            state.sinks.push(Rc::clone(&sink));
            state.sink = Some(sink);
            state.requests.push(request);
            state.opened += 1;
            let view = cx.new(|cx| FakeTerminalView {
                focus_handle: cx.focus_handle().tab_stop(true).tab_index(20isize),
            });
            let weak_view = view.downgrade();
            Ok(TerminalInstance {
                view: view.into(),
                activate: Box::new(move |window, cx| {
                    if let Some(view) = weak_view.upgrade() {
                        let focus = view.read(cx).focus_handle.clone();
                        window.focus(&focus, cx);
                    }
                }),
            })
        });
        let forward_state = Rc::clone(&forwards);
        let forward_factory: super::super::terminal::PortForwardFactory =
            Rc::new(move |request, _cx| {
                let mut state = forward_state.borrow_mut();
                state.starts += 1;
                state.last_request = Some(request.clone());
                if let Some(reason) = state.fail.clone() {
                    return Err(reason);
                }
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                state.errors = Some(sender);
                let (binding_sender, binding) = tokio::sync::oneshot::channel();
                if state.pending {
                    state.binding = Some(binding_sender);
                } else {
                    binding_sender.send(Ok(4321)).ok();
                }
                Ok(StartedForward {
                    handle: Box::new(FakeForwardHandle {
                        state: Rc::clone(&forward_state),
                    }),
                    binding,
                    errors: receiver,
                })
            });
        TerminalServices {
            terminals: factory,
            forwards: forward_factory,
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
        }
    }

    type TerminalHarness<'a> = (
        gpui::Entity<DockPanel>,
        Rc<RefCell<FakeTerminals>>,
        Rc<RefCell<FakeForwards>>,
        &'a mut gpui::VisualTestContext,
    );

    fn setup_terminals(cx: &mut TestAppContext) -> TerminalHarness<'_> {
        setup_terminals_in_dock(cx, None)
    }

    /// Same terminal services, but the Dock gets its own width inside a realistically sized
    /// window. The terminal toolbar reads the same compact breakpoint as the log rows.
    fn setup_terminals_in_dock(
        cx: &mut TestAppContext,
        width: Option<Pixels>,
    ) -> TerminalHarness<'_> {
        init_app(cx);
        let terminals = Rc::new(RefCell::new(FakeTerminals::default()));
        let forwards = Rc::new(RefCell::new(FakeForwards::default()));
        let services = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        let (panel, cx) = add_dock_window(cx, width, Some(REAL_DOCK_HEIGHT));
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(services), cx);
        });
        (panel, terminals, forwards, cx)
    }

    #[gpui::test]
    fn missing_terminal_services_show_connect_without_add(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| DockPanel::new(cx));
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(None, cx);
            panel.show_terminal_tab(cx);
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.terminal_available()));
        assert!(cx.debug_bounds("terminal-add").is_none());
        assert!(cx.debug_bounds("terminal-actions-trigger").is_none());
        let toolbar = cx
            .debug_bounds("terminal-toolbar")
            .expect("Terminal toolbar");
        let state = cx
            .debug_bounds("empty-state")
            .expect("Terminal empty state");
        assert!(f32::from(toolbar.size.height) > 0.0);
        assert!(f32::from(state.size.height) > 0.0);
    }

    #[gpui::test]
    fn terminal_empty_state_exposes_add_and_context(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) =
            setup_terminals_in_dock(cx, Some(COMPACT_DOCK_WIDTH));
        panel.update(cx, |panel, cx| panel.show_terminal_tab(cx));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        assert!(panel.read_with(cx, |panel, _| panel.terminal_available()));
        assert_eq!(
            panel
                .read_with(cx, |panel, _| panel.terminal_context_label())
                .as_deref(),
            Some("kind-dev/default")
        );
        let toolbar = cx
            .debug_bounds("terminal-toolbar")
            .expect("Terminal toolbar");
        let context = cx
            .debug_bounds("terminal-context")
            .expect("Terminal context");
        let state = cx
            .debug_bounds("empty-state")
            .expect("Terminal empty state");
        let add = cx.debug_bounds("terminal-add").expect("New Terminal");
        assert!(f32::from(context.size.width) > 0.0);
        assert!(context.origin.y >= toolbar.origin.y);
        assert!(context.bottom() <= toolbar.bottom());
        assert!(add.size.height >= design::size::CONTROL);
        assert!(add.origin.y >= state.origin.y);
        assert!(add.bottom() <= state.bottom());
        // A compact Dock drops the maximize control, so the empty state must not depend on it.
        assert!(cx.debug_bounds("terminal-maximize").is_none());

        cx.simulate_click(add.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.terminal_count()), 1);
        assert_eq!(terminals.borrow().opened, 1);
    }

    #[gpui::test]
    fn terminal_service_binding_keeps_empty_state_action_without_context(cx: &mut TestAppContext) {
        init_app(cx);
        let terminals = Rc::new(RefCell::new(FakeTerminals::default()));
        let forwards = Rc::new(RefCell::new(FakeForwards::default()));
        let mut services = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        services.context = None;
        let (panel, cx) = cx.add_window_view(|_, cx| DockPanel::new(cx));
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(services), cx);
            panel.show_terminal_tab(cx);
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.terminal_available()));
        assert!(cx.debug_bounds("terminal-add").is_some());
        assert!(cx.debug_bounds("terminal-actions-trigger").is_some());
    }

    #[gpui::test]
    fn terminal_single_session_hides_chip_and_pane_flushes_right(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) =
            setup_terminals_in_dock(cx, Some(COMPACT_DOCK_WIDTH));
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        assert!(cx.debug_bounds("terminal-chip-0").is_none());
        let content = cx.debug_bounds("dock-content").expect("Dock content");
        let toolbar = cx
            .debug_bounds("terminal-toolbar")
            .expect("Terminal toolbar");
        let target = cx.debug_bounds("terminal-target").expect("Terminal header");
        let actions = cx
            .debug_bounds("terminal-actions-trigger")
            .expect("Terminal actions");
        let pane = cx.debug_bounds("terminal-pane").expect("Terminal pane");
        assert!((f32::from(content.right()) - f32::from(pane.right())).abs() <= 1.0);
        assert!((f32::from(toolbar.size.height) - f32::from(design::size::TOOLBAR)).abs() <= 1.0);
        assert!(target.origin.y >= toolbar.origin.y);
        assert!(target.bottom() <= toolbar.bottom());
        assert!((f32::from(actions.size.height) - f32::from(design::size::CONTROL)).abs() <= 1.0);
        assert!(cx.debug_bounds("terminal-split").is_none());
        assert!(cx.debug_bounds("terminal-close").is_none());
        assert!(cx.debug_bounds("terminal-maximize").is_none());

        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open second terminal");
        cx.run_until_parked();
        assert!(cx.debug_bounds("terminal-chip-0").is_some());
        assert!(cx.debug_bounds("terminal-chip-1").is_some());
    }

    /// The terminal canvas is a surface role of its own and the layer ramp does not describe it, so
    /// the pane states the relation with a rule instead. The session view has to stop at the rule:
    /// a hairline drawn under the text is chrome laid over the content, not a boundary.
    #[gpui::test]
    fn the_terminal_pane_reserves_a_rule_between_the_session_and_the_dock(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) =
            setup_terminals_in_dock(cx, Some(COMPACT_DOCK_WIDTH));
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let pane = cx.debug_bounds("terminal-pane").expect("Terminal pane");
        let view = cx
            .debug_bounds("fake-terminal-view")
            .expect("the session view");
        assert_eq!(
            view.origin, pane.origin,
            "the rule is at the bottom edge only"
        );
        assert!(
            (f32::from(pane.bottom() - view.bottom()) - f32::from(design::border::LINE)).abs()
                <= 1.0,
            "the pane reserves one rule below the session: pane {pane:?}, view {view:?}"
        );
    }

    /// The verdict slot is reserved whether or not there is a verdict, so a session that ends does
    /// not slide its neighbours along the session strip.
    #[gpui::test]
    fn every_session_chip_reserves_its_verdict_slot(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) =
            setup_terminals_in_dock(cx, Some(COMPACT_DOCK_WIDTH));
        for _ in 0..2 {
            panel
                .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
                .expect("open terminal");
        }
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        let open = cx
            .debug_bounds("terminal-chip-verdict-0")
            .expect("verdict slot");
        assert!(
            (f32::from(open.size.width) - f32::from(design::size::STATUS_MARKER)).abs() <= 1.0,
            "the slot is the marker token wide: {open:?}"
        );

        // The first session ends. Its slot must not move.
        let sink = terminals
            .borrow()
            .sinks
            .first()
            .cloned()
            .expect("factory received a sink");
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(1),
                    signal: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("terminal-chip-verdict-0"),
            Some(open),
            "a verdict fills the reserved slot instead of moving it"
        );
    }

    #[gpui::test]
    fn open_terminal_switches_to_terminal_tab(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("local terminal opens");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 1);
            assert_eq!(panel.terminal_focus_handles.len(), 1);
            assert_eq!(
                panel.active_tab, 1,
                "opening a terminal selects the Terminal tab"
            );
        });
        assert_eq!(terminals.borrow().opened, 1);
    }

    #[gpui::test]
    fn existing_local_terminal_keeps_namespace_when_new_terminal_uses_updated_scope(
        cx: &mut TestAppContext,
    ) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            let mut services = panel.terminal_services.clone().expect("terminal services");
            services.namespace = Some("team-a".to_owned());
            panel.set_terminal_services(Some(services), cx);
            panel
                .open_terminal(TerminalKind::Local, cx)
                .expect("open first terminal");
        });
        panel.update(cx, |panel, cx| {
            let mut services = panel.terminal_services.clone().expect("terminal services");
            services.namespace = Some("team-b".to_owned());
            panel.set_terminal_services(Some(services), cx);
            panel
                .open_terminal(TerminalKind::Local, cx)
                .expect("open second terminal");
        });

        let requests = terminals.borrow().requests.clone();
        assert_eq!(requests[0].namespace.as_deref(), Some("team-a"));
        assert_eq!(requests[1].namespace.as_deref(), Some("team-b"));
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.terminals[0].request.namespace.as_deref(),
                Some("team-a")
            );
            assert_eq!(
                panel.terminals[1].request.namespace.as_deref(),
                Some("team-b")
            );
            assert_eq!(
                terminal_entry_title(&panel.terminals[0], Some("kind-dev")).as_ref(),
                "kind-dev/team-a"
            );
            assert_eq!(
                terminal_entry_title(&panel.terminals[1], Some("kind-dev")).as_ref(),
                "kind-dev/team-b"
            );
        });
    }

    #[gpui::test]
    fn closing_terminal_releases_focus_before_focusing_neighbor_or_add(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open first");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.activate_terminal(0, window, cx));
        });
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open second");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.activate_terminal(1, window, cx));
        });
        cx.run_until_parked();
        let terminal_focus = panel
            .read_with(cx, |panel, _| panel.terminals[1].focus_handle.clone())
            .expect("terminal focus");
        let chip_focus = panel.read_with(cx, |panel, _| panel.terminal_focus_handles[0].clone());
        let add_focus = panel.read_with(cx, |panel, _| panel.terminal_add_focus.clone());
        assert!(cx.update(|window, _| terminal_focus.is_focused(window)));

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.close_terminal(1, window, cx));
        });
        cx.run_until_parked();
        assert!(!cx.update(|window, _| terminal_focus.is_focused(window)));
        assert!(cx.update(|window, _| chip_focus.is_focused(window)));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.terminal_focus_handles.len()),
            1
        );

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.close_terminal(0, window, cx));
        });
        cx.run_until_parked();
        assert!(!cx.update(|window, _| chip_focus.is_focused(window)));
        assert!(cx.update(|window, _| add_focus.is_focused(window)));
        assert!(panel.read_with(cx, |panel, _| panel.terminals.is_empty()));
    }

    #[gpui::test]
    fn terminal_chips_rove_activate_and_delete(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open first");
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open second");
        cx.run_until_parked();
        let handles = panel.read_with(cx, |panel, _| panel.terminal_focus_handles.clone());
        assert!(!handles[0].tab_stop);
        assert!(handles[1].tab_stop);

        cx.update(|window, cx| window.focus(&handles[1], cx));
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.active_terminal), 0);
        assert!(cx.update(|window, _| handles[0].is_focused(window)));
        assert!(!handles[1].clone().tab_stop);

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let terminal_focus = panel
            .read_with(cx, |panel, _| panel.terminals[0].focus_handle.clone())
            .expect("terminal focus");
        assert!(cx.update(|window, _| terminal_focus.is_focused(window)));

        cx.update(|window, cx| window.focus(&handles[0], cx));
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.terminal_count()), 1);
        assert!(cx.update(|window, _| handles[1].is_focused(window)));
    }

    #[gpui::test]
    fn split_exec_clones_the_complete_terminal_request(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel
                .open_terminal(
                    TerminalKind::Exec {
                        namespace: "team-a".to_owned(),
                        pod: "web-0".to_owned(),
                        container: Some("app".to_owned()),
                    },
                    cx,
                )
                .expect("open exec terminal");
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });
        let requests = terminals.borrow().requests.clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1], requests[0]);
        assert_eq!(
            requests[1].kind,
            TerminalKind::Exec {
                namespace: "team-a".to_owned(),
                pod: "web-0".to_owned(),
                container: Some("app".to_owned()),
            }
        );
    }

    #[gpui::test]
    fn split_and_maximize_terminal_actions_update_layout_state(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 2);
            assert!(panel.terminal_split);
        });
        panel.update(cx, |panel, cx| panel.toggle_terminal_maximized(cx));
        assert!(panel.read_with(cx, |panel, _| panel.terminal_maximized));
    }

    /// The split divider is a control, not a decorative line: it has a role, a hit area, and a
    /// keyboard resize that stays inside the pane bounds.
    #[gpui::test]
    fn terminal_split_divider_is_a_focusable_resize_control(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });
        cx.run_until_parked();

        let divider = cx
            .debug_bounds("terminal-split-divider")
            .expect("split divider");
        assert!(
            divider.size.width >= design::border::HIT,
            "the divider needs a hit area wider than the line it draws"
        );
        let focus = panel.read_with(cx, |panel, _| panel.terminal_split_focus.clone());
        assert!(
            focus.tab_stop,
            "the divider takes a tab stop while it is on screen"
        );
        cx.update(|window, cx| window.focus(&focus, cx));
        assert!(panel.read_with(cx, |panel, _| panel.terminal_split_ratio) == 0.5);

        cx.simulate_keystrokes("right");
        assert!(
            panel.read_with(cx, |panel, _| panel.terminal_split_ratio) > 0.5,
            "Right grows the active pane"
        );
        cx.simulate_keystrokes("left");
        assert!(
            panel.read_with(cx, |panel, _| (panel.terminal_split_ratio - 0.5).abs()
                < 1e-6)
        );

        cx.simulate_keystrokes("end");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.terminal_split_ratio),
            TERMINAL_SPLIT_MAX_RATIO
        );
        cx.simulate_keystrokes("home");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.terminal_split_ratio),
            TERMINAL_SPLIT_MIN_RATIO,
            "the divider cannot swallow the second pane"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.split_ratio_at(
                px(10_000.),
                gpui::Bounds::new(
                    gpui::point(px(0.), px(0.)),
                    gpui::size(px(1_000.), px(100.))
                )
            )),
            TERMINAL_SPLIT_MAX_RATIO,
            "a pointer past the right edge clamps too"
        );
    }

    #[gpui::test]
    fn closing_the_split_resets_the_divider(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.split_terminal(window, cx));
        });
        panel.update(cx, |panel, cx| panel.set_terminal_split_ratio(0.7, cx));
        cx.update(|window, cx| panel.update(cx, |panel, cx| panel.close_terminal(1, window, cx)));
        panel.read_with(cx, |panel, _| {
            assert!(!panel.terminal_split);
            assert_eq!(panel.terminal_split_ratio, 0.5);
            assert!(panel.terminal_split_drag.is_none());
        });
    }

    #[gpui::test]
    fn closing_cluster_sessions_clears_session_focus_handles(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        panel.update(cx, |panel, cx| panel.close_cluster_sessions(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 0);
            assert!(panel.terminal_focus_handles.is_empty());
        });
    }

    #[gpui::test]
    fn local_terminal_ignores_osc_title_and_tracks_exit(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        let sink = terminals
            .borrow()
            .sink
            .clone()
            .expect("factory received sink");
        cx.update(|_, cx| sink.as_ref()(TerminalEvent::Title("bash".into()), cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.terminals[0].title.is_none());
            assert_eq!(
                terminal_entry_title(&panel.terminals[0], Some("new-context")).as_ref(),
                "kind-dev/default"
            );
        });
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(3),
                    signal: None,
                },
                cx,
            )
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.terminals[0].exit.as_deref(),
                Some("Exited with code 3")
            );
        });
    }

    #[gpui::test]
    fn terminal_exit_moves_focus_to_add_action(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.activate_terminal(0, window, cx));
        });
        cx.run_until_parked();
        let sink = terminals
            .borrow()
            .sink
            .clone()
            .expect("factory received sink");
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(1),
                    signal: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        let add_focus = panel.read_with(cx, |panel, _| panel.terminal_add_focus.clone());
        assert!(cx.update(|window, _| add_focus.is_focused(window)));
    }

    #[gpui::test]
    fn exec_osc_title_updates_dynamic_title(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel
                .open_terminal(
                    TerminalKind::Exec {
                        namespace: "team-a".to_owned(),
                        pod: "web-0".to_owned(),
                        container: None,
                    },
                    cx,
                )
                .expect("open terminal");
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                terminal_entry_title(&panel.terminals[0], Some("new-context")).as_ref(),
                "kind-dev/team-a/Pod/web-0"
            );
        });
        let sink = terminals
            .borrow()
            .sink
            .clone()
            .expect("factory received sink");
        cx.update(|_, cx| sink.as_ref()(TerminalEvent::Title("bash".into()), cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminals[0].title.as_deref(), Some("bash"));
            assert_eq!(
                terminal_entry_title(&panel.terminals[0], Some("new-context")).as_ref(),
                "bash"
            );
        });
    }

    #[gpui::test]
    fn restart_replaces_instance_and_clears_exit(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open terminal");
        let sink = terminals.borrow().sink.clone().expect("sink");
        cx.update(|_, cx| {
            sink.as_ref()(
                TerminalEvent::Exited {
                    code: Some(1),
                    signal: None,
                },
                cx,
            )
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.restart_terminal(0, window, cx));
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 1, "restart does not add a session");
            assert!(
                panel.terminals[0].exit.is_none(),
                "restart clears the exit status"
            );
        });
        assert_eq!(terminals.borrow().opened, 2, "factory was called again");
    }

    #[gpui::test]
    fn stop_forward_retains_entry_until_the_error_stream_closes(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        panel
            .update(cx, |panel, cx| panel.start_forward(request.clone(), cx))
            .expect("start");
        cx.run_until_parked();
        let id = panel.read_with(cx, |panel, _| panel.forward_snapshots()[0].id);

        panel.update(cx, |panel, cx| panel.stop_forward(id, cx));
        panel.read_with(cx, |panel, _| {
            let entry = &panel.forwards[0];
            assert_eq!(entry.phase, ForwardPhase::Stopping);
            assert_eq!(entry.request, request);
            assert!(entry.label.contains("Pod/web-0"));
            assert_eq!(entry.remote_port, 8080);
            assert!(entry.handle.is_none());
            assert_eq!(panel.forward_summary().pending, 1);
        });
        assert_eq!(forwards.borrow().stopped, 1);

        forwards.borrow_mut().errors.take();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.phase, ForwardPhase::Stopped);
            assert_eq!(snapshot.request, request);
            assert_eq!(snapshot.remote_port, 8080);
            assert_eq!(snapshot.local_port, None);
            assert!(snapshot.error.is_none());
            assert_eq!(panel.forward_summary().stopped, 1);
        });
    }

    #[gpui::test]
    fn finish_forward_failure_sends_raw_reason_as_notice_detail(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let handler_delivered = Rc::clone(&delivered);
        panel.update(cx, |panel, _| {
            panel.set_notice_handler(move |message, severity, _| {
                handler_delivered.borrow_mut().push((message, severity));
            });
        });
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        panel
            .update(cx, |panel, cx| panel.start_forward(request, cx))
            .expect("start");
        cx.run_until_parked();

        let reason = "port 8080: lost connection to 10.0.0.12:8080".to_owned();
        forwards
            .borrow()
            .errors
            .as_ref()
            .expect("error stream")
            .send(reason.clone())
            .ok();
        cx.run_until_parked();

        let delivered = delivered.borrow();
        let error_notice = delivered
            .iter()
            .find(|(_, severity)| *severity == Severity::Error)
            .expect("error notice");
        assert!(
            error_notice
                .0
                .starts_with("Port forward for kind-dev/default/Pod/web-0 stopped.")
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel
                .pending_notice_detail(&error_notice.0, Severity::Error)),
            Some(reason)
        );
    }

    #[gpui::test]
    fn stopped_forward_starts_again_by_stable_id(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        let id = panel
            .update(cx, |panel, cx| panel.create_forward(request.clone(), cx))
            .expect("create");
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.active_tab), 0);
        panel.update(cx, |panel, cx| panel.stop_forward(id, cx));
        forwards.borrow_mut().errors.take();
        cx.run_until_parked();
        forwards.borrow_mut().pending = true;

        panel
            .update(cx, |panel, cx| panel.restart_forward(id, cx))
            .expect("restart stopped forward");
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.id, id);
            assert_eq!(snapshot.phase, ForwardPhase::Starting);
            assert_eq!(snapshot.local_port, None);
            assert_eq!(panel.forward_summary().pending, 1);
        });

        let binding = forwards
            .borrow_mut()
            .binding
            .take()
            .expect("factory retained the binding sender");
        binding.send(Ok(4321)).ok();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.id, id);
            assert_eq!(snapshot.phase, ForwardPhase::Running);
            assert_eq!(snapshot.local_port, Some(4321));
            assert_eq!(panel.forward_summary().active, 1);
        });
        assert_eq!(forwards.borrow().starts, 2);
        assert_eq!(forwards.borrow().last_request.as_ref(), Some(&request));
    }

    #[gpui::test]
    fn failed_forward_retries_by_id_without_switching_tabs(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        panel
            .update(cx, |panel, cx| panel.start_forward(request.clone(), cx))
            .expect("start");
        cx.run_until_parked();
        let id = panel.read_with(cx, |panel, _| panel.forward_snapshots()[0].id);
        panel.update(cx, |panel, _| panel.active_tab = 0);
        forwards
            .borrow()
            .errors
            .clone()
            .expect("error stream")
            .send("connection lost".to_owned())
            .ok();
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.forward_summary().failed),
            1
        );
        panel
            .update(cx, |panel, cx| panel.retry_forward(id, cx))
            .expect("retry failed forward");
        cx.run_until_parked();

        assert_eq!(forwards.borrow().starts, 2);
        assert_eq!(forwards.borrow().last_request.as_ref(), Some(&request));
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.id, id);
            assert_eq!(snapshot.phase, ForwardPhase::Running);
            assert!(snapshot.label.contains("Pod/web-0"));
            assert_eq!(panel.active_tab, 0);
        });
    }

    #[gpui::test]
    fn stale_attempt_callback_does_not_change_the_new_attempt(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("default".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port: None,
        };
        panel
            .update(cx, |panel, cx| panel.start_forward(request, cx))
            .expect("start");
        cx.run_until_parked();
        let id = panel.read_with(cx, |panel, _| panel.forward_snapshots()[0].id);
        let old_sender = forwards.borrow().errors.clone().expect("error stream");
        old_sender.send("connection lost".to_owned()).ok();
        cx.run_until_parked();
        panel
            .update(cx, |panel, cx| panel.retry_forward(id, cx))
            .expect("retry");
        cx.run_until_parked();

        old_sender.send("stale callback".to_owned()).ok();
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let snapshot = &panel.forward_snapshots()[0];
            assert_eq!(snapshot.id, id);
            assert_eq!(snapshot.phase, ForwardPhase::Running);
            assert_eq!(snapshot.local_port, Some(4321));
            assert!(snapshot.error.is_none());
        });
    }

    #[gpui::test]
    fn failed_forward_keeps_detail_for_retry(cx: &mut TestAppContext) {
        let (panel, _terminals, forwards, cx) = setup_terminals(cx);
        forwards.borrow_mut().fail = Some("address already in use".to_owned());
        let request = ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: None,
            name: "web-0".into(),
            remote_port: 80,
            local_port: None,
        };
        let result = panel.update(cx, |panel, cx| panel.start_forward(request, cx));
        assert_eq!(
            result.expect_err("The request must fail."),
            "The port forward did not start. Check the port and connection, then try again."
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.forward_count(), 1);
            assert_eq!(panel.forwards[0].phase, ForwardPhase::Failed);
            assert_eq!(
                panel.forwards[0].error.as_deref(),
                Some("address already in use")
            );
        });
    }
}
