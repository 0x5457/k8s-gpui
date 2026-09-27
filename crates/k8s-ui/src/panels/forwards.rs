use std::cmp::Ordering;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, ListHorizontalSizingBehavior, ParentElement, Render, Role, ScrollHandle,
    ScrollStrategy, SharedString, Styled, Subscription, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use kube_core::DynamicObject;
use ui::prelude::*;
use ui::{IconButton, ListItem, ListItemSpacing, TintColor, Tooltip};

use super::common;
use super::dock::{DockPanel, ForwardId, ForwardPhase, ForwardSnapshot, ForwardSummary};
use crate::design::{self, Severity, space};
use crate::settings::{self, DataTypography};
use crate::table_view::TextInput;

pub type NewForwardCallback = Rc<dyn Fn(&mut Window, &mut App)>;

const STATE_COLUMN_WIDTH: f32 = 128.;
const URL_COLUMN_WIDTH: f32 = 168.;
const TARGET_MIN_WIDTH: f32 = 260.;
const ACTIONS_COLUMN_WIDTH: f32 = 168.;
const ACTION_TEXT_WIDTH: f32 = 72.;
const MIN_TABLE_WIDTH: f32 = 784.;
const FILTER_WIDTH: f32 = 208.;
const SUMMARY_LABEL: &str = "Port forward summary";
const EMPTY_TITLE: &str = "No port forwards";
const EMPTY_HINT: &str =
    "Open Pods from the resource tree, then create a port forward from a running Pod.";
const FILTERED_EMPTY_TITLE: &str = "No matching port forwards";
const FILTERED_EMPTY_HINT: &str =
    "No port forward matches the filter. Clear it to see every forward.";
const FOOTER_TEXT: &str = "Enter runs the row action. Control C copies the address, Control O opens it, and / focuses the filter.";
/// How long a copy or open confirmation stays in the toolbar.
const FEEDBACK_DURATION: Duration = Duration::from_millis(1500);
const NO_ADDRESS_TOOLTIP: &str = "This forward has no address right now";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowAction {
    Start(ForwardId),
    Stop(ForwardId),
    Retry(ForwardId),
    Waiting(ForwardId),
}

impl RowAction {
    fn id(self) -> ForwardId {
        match self {
            Self::Start(id) | Self::Stop(id) | Self::Retry(id) | Self::Waiting(id) => id,
        }
    }

    /// One name for the control a phase shows. The element id and the debug selector both use
    /// it, so a test can address the control a row renders right now.
    fn name(self) -> &'static str {
        match self {
            Self::Start(_) => "forwards-start",
            Self::Stop(_) => "forwards-stop",
            Self::Retry(_) => "forwards-retry",
            Self::Waiting(_) => "forwards-waiting",
        }
    }
}

/// One rendered row. A namespace caption introduces the forwards that follow it, so a long list
/// stays scannable without a second table.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Group {
        namespace: SharedString,
        count: usize,
    },
    Forward(ForwardId),
}

/// A column the user can order rows by. `Actions` is left out: it holds controls, not data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    State,
    Url,
    Target,
}

impl Column {
    fn title(self) -> &'static str {
        match self {
            Self::State => "State",
            Self::Url => "URL",
            Self::Target => "Target",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Ascending,
    Descending,
}

impl Direction {
    fn flipped(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    fn read_text(self) -> &'static str {
        match self {
            Self::Ascending => "sorted from low to high",
            Self::Descending => "sorted from high to low",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sort {
    column: Column,
    direction: Direction,
}

impl Default for Sort {
    fn default() -> Self {
        // The target is what the user recognizes a forward by, and the default order is the
        // order the forwards were created in within each namespace.
        Self {
            column: Column::Target,
            direction: Direction::Ascending,
        }
    }
}

impl Sort {
    /// Clicking the sorted column reverses it; clicking another column starts ascending.
    fn toggled_to(self, column: Column) -> Self {
        if self.column == column {
            Self {
                column,
                direction: self.direction.flipped(),
            }
        } else {
            Self {
                column,
                direction: Direction::Ascending,
            }
        }
    }

    fn reads(self, column: Column) -> bool {
        self.column == column
    }
}

fn row_action(snapshot: &ForwardSnapshot) -> RowAction {
    match snapshot.phase {
        ForwardPhase::Stopped => RowAction::Start(snapshot.id),
        ForwardPhase::Starting | ForwardPhase::Running => RowAction::Stop(snapshot.id),
        ForwardPhase::Failed => RowAction::Retry(snapshot.id),
        ForwardPhase::Stopping => RowAction::Waiting(snapshot.id),
    }
}

/// A failed or stopped forward keeps no listener. The row must not keep advertising the
/// port it used to bind, or the user reads a port that no longer answers.
fn live_local_port(snapshot: &ForwardSnapshot) -> Option<u16> {
    match snapshot.phase {
        ForwardPhase::Running | ForwardPhase::Stopping => snapshot.local_port,
        ForwardPhase::Stopped | ForwardPhase::Starting | ForwardPhase::Failed => None,
    }
}

/// The address a browser can open. A forward that holds no listener has no address, so copying
/// or opening one is refused instead of handing over a port that answers nothing.
fn forward_url(snapshot: &ForwardSnapshot) -> Option<String> {
    live_local_port(snapshot).map(|port| format!("http://localhost:{port}"))
}

/// The local port the user asked for, when it is not the one in use. A taken port is reported
/// here rather than silently replaced, because the user may have pointed something at it.
fn port_substitution(snapshot: &ForwardSnapshot) -> Option<String> {
    let requested = snapshot.request.local_port?;
    let live = live_local_port(snapshot)?;
    (requested != live).then(|| {
        format!("Local port {requested} was already in use, so this forward listens on {live}.")
    })
}

/// Namespace a forward targets. `default` is the namespace Kubernetes uses when a request leaves
/// it empty, so a group caption never reads as a blank cell.
fn forward_namespace(snapshot: &ForwardSnapshot) -> &str {
    snapshot
        .request
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or("default")
}

/// True when a query matches anything the user can see in the row: the target, the namespace, the
/// live URL, or the state text.
fn matches_filter(snapshot: &ForwardSnapshot, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let mut haystack = format!(
        "{} {} {} {} {}",
        snapshot.label,
        snapshot.remote_port,
        forward_namespace(snapshot),
        phase_presentation(snapshot.phase).1,
        forward_url(snapshot).unwrap_or_default()
    );
    if let Some(error) = snapshot.error.as_deref() {
        haystack.push(' ');
        haystack.push_str(error);
    }
    haystack.to_lowercase().contains(&query)
}

/// Order weight of a phase. A running forward is the one the user is working with, so it leads.
fn state_rank(phase: ForwardPhase) -> u8 {
    match phase {
        ForwardPhase::Running => 0,
        ForwardPhase::Starting | ForwardPhase::Stopping => 1,
        ForwardPhase::Failed => 2,
        ForwardPhase::Stopped => 3,
    }
}

/// Order rows by the active column. The namespace is the outer frame, so a caption never repeats
/// and the sorted column orders the rows inside each group.
fn compare_forwards(a: &ForwardSnapshot, b: &ForwardSnapshot, sort: Sort) -> Ordering {
    let by_namespace = forward_namespace(a).cmp(forward_namespace(b));
    if by_namespace != Ordering::Equal {
        return by_namespace;
    }
    let by_column = match sort.column {
        Column::State => state_rank(a.phase).cmp(&state_rank(b.phase)),
        Column::Url => live_local_port(a)
            .unwrap_or(u16::MAX)
            .cmp(&live_local_port(b).unwrap_or(u16::MAX)),
        Column::Target => target_text(a).cmp(&target_text(b)),
    };
    let by_column = match sort.direction {
        Direction::Ascending => by_column,
        Direction::Descending => by_column.reverse(),
    };
    by_column
        .then_with(|| target_text(a).cmp(&target_text(b)))
        .then_with(|| a.id.0.cmp(&b.id.0))
}

/// Rows to render: forwards that match the filter, grouped by namespace, in the sorted order.
fn build_rows(forwards: &[ForwardSnapshot]) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::with_capacity(forwards.len() + 1);
    let mut start = 0;
    while start < forwards.len() {
        let namespace = forward_namespace(&forwards[start]);
        let mut end = start;
        while end < forwards.len() && forward_namespace(&forwards[end]) == namespace {
            end += 1;
        }
        rows.push(Row::Group {
            namespace: SharedString::from(namespace),
            count: end - start,
        });
        for snapshot in &forwards[start..end] {
            rows.push(Row::Forward(snapshot.id));
        }
        start = end;
    }
    rows
}

fn row_forward(row: &Row) -> Option<ForwardId> {
    match row {
        Row::Forward(id) => Some(*id),
        Row::Group { .. } => None,
    }
}

/// The display row of a forward, which grouping shifts away from its index in the forward list.
fn row_of_forward(rows: &[Row], id: ForwardId) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Forward(candidate) if *candidate == id))
}

/// The next or previous row that holds a forward. Group captions are not selectable, so Up and
/// Down step over them instead of landing the selection on a heading.
fn step_forward_row(rows: &[Row], from: Option<usize>, delta: isize) -> Option<usize> {
    if delta >= 0 {
        let start = from.map_or(0, |row| row + 1);
        return (start..rows.len()).find(|row| row_forward(&rows[*row]).is_some());
    }
    let end = from.unwrap_or(rows.len());
    (0..end)
        .rev()
        .find(|row| row_forward(&rows[*row]).is_some())
}

/// `Stop All` can only end forwards that are still starting or running.
fn is_stoppable(phase: ForwardPhase) -> bool {
    matches!(phase, ForwardPhase::Starting | ForwardPhase::Running)
}

fn stoppable_count(snapshots: &[ForwardSnapshot]) -> usize {
    snapshots
        .iter()
        .filter(|snapshot| is_stoppable(snapshot.phase))
        .count()
}

/// `pod:8080`, the Pod and remote port this forward targets.
fn target_text(snapshot: &ForwardSnapshot) -> String {
    format!("{}:{}", snapshot.label, snapshot.remote_port)
}

/// Spoken text for one row. It carries the address, the substituted port, and the failure reason,
/// which a tooltip alone does not reach.
fn row_aria_label(snapshot: &ForwardSnapshot) -> String {
    let mut label = format!(
        "Port forward for {}. {}.",
        target_text(snapshot),
        phase_presentation(snapshot.phase).1
    );
    if let Some(url) = forward_url(snapshot) {
        label.push_str(&format!(" {url}."));
    }
    if let Some(substitution) = port_substitution(snapshot) {
        label.push(' ');
        label.push_str(&substitution);
    }
    if let Some(error) = snapshot
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        label.push(' ');
        label.push_str(error);
    }
    label
}

/// Shape, words, severity and next step for a forward's phase.
///
/// The shape is `design::health_icon`, the app's only health vocabulary
/// (`DESIGN.md` §4), so a failed forward wears the same filled `×` as a failed
/// pod and a running one wears the same `✓`. This function used to carry its own
/// phase-to-glyph map — `Failed` drew a warning triangle, so the same failure
/// looked like a warning in this panel and like an error everywhere else — and
/// `shell/status_bar.rs` kept a second copy of it for the chip in the bar.
///
/// `Starting` and `Stopping` share a shape on purpose. Both are in progress, so
/// they are one health class, and the shared vocabulary gives one shape per class;
/// the words beside the shape say which phase it is. That is the same answer the
/// status bar's `Connecting` and `Reconnecting` already give.
pub fn phase_presentation(phase: ForwardPhase) -> (IconName, &'static str, Severity, &'static str) {
    let (label, severity, next_step) = match phase {
        ForwardPhase::Stopped => (
            "Stopped",
            Severity::Muted,
            "Select Start to run this forward again.",
        ),
        ForwardPhase::Starting => (
            "Starting…",
            Severity::Warning,
            "Select Stop to cancel startup.",
        ),
        ForwardPhase::Running => (
            "Running",
            Severity::Success,
            "Select Stop to end this forward.",
        ),
        ForwardPhase::Stopping => (
            "Stopping…",
            Severity::Warning,
            "Waiting for the forward to stop.",
        ),
        ForwardPhase::Failed => ("Failed", Severity::Error, "Select Retry to reconnect."),
    };
    (design::health_icon(severity), label, severity, next_step)
}

fn summary_text(summary: ForwardSummary) -> String {
    format!(
        "{} active, {} failed, {} pending, {} stopped",
        summary.active, summary.failed, summary.pending, summary.stopped
    )
}

fn id_key(id: ForwardId) -> SharedString {
    SharedString::from(format!("{id:?}"))
}

fn focused_index(
    selected: Option<usize>,
    root_focused: bool,
    snapshot_count: usize,
) -> Option<usize> {
    if !root_focused || snapshot_count == 0 {
        return None;
    }
    Some(
        selected
            .filter(|index| *index < snapshot_count)
            .unwrap_or_default(),
    )
}

/// One `containerPort` a Pod declares, offered as a choice instead of a value to type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerPort {
    pub port: u16,
    /// The container that declares the port, so a multi-container Pod stays unambiguous.
    pub container: Option<SharedString>,
}

impl ContainerPort {
    /// Label for the choice, such as `8080 (app)`.
    pub fn label(&self) -> String {
        match self.container.as_deref() {
            Some(container) => format!("{} ({container})", self.port),
            None => self.port.to_string(),
        }
    }
}

/// The `containerPort` values a Pod declares, in document order and without duplicates.
///
/// Only TCP ports are offered: a port forward opens a TCP stream, so a UDP-only port would fail
/// after the user picked it. A Pod that declares nothing returns an empty list, and the caller
/// keeps the free-text field for that case.
pub fn container_ports(object: &DynamicObject) -> Vec<ContainerPort> {
    let mut ports: Vec<ContainerPort> = Vec::new();
    let containers = object
        .data
        .get("spec")
        .and_then(|spec| spec.get("containers"))
        .and_then(serde_json::Value::as_array);
    let Some(containers) = containers else {
        return ports;
    };
    for container in containers {
        let name = container
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(SharedString::from);
        let Some(declared) = container.get("ports").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in declared {
            let tcp = entry
                .get("protocol")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|protocol| protocol.eq_ignore_ascii_case("TCP"));
            let Some(port) = entry
                .get("containerPort")
                .and_then(serde_json::Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
            else {
                continue;
            };
            if !tcp || ports.iter().any(|existing| existing.port == port) {
                continue;
            }
            ports.push(ContainerPort {
                port,
                container: name.clone(),
            });
        }
    }
    ports
}

pub struct ForwardsView {
    dock: Entity<DockPanel>,
    selected: Option<ForwardId>,
    focus_handle: FocusHandle,
    new_focus: FocusHandle,
    new_callback: Option<NewForwardCallback>,
    filter_input: Entity<TextInput>,
    filter: String,
    sort: Sort,
    feedback: Option<Feedback>,
    horizontal_scroll: ScrollHandle,
    list_scroll: UniformListScrollHandle,
    _dock_observation: Subscription,
    _filter_observation: Subscription,
}

/// A confirmation for an action the user cannot see in the row itself, such as a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Feedback {
    id: ForwardId,
    message: SharedString,
    severity: Severity,
    at: Instant,
}

/// What the editing chord asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UrlChord {
    Copy,
    Open,
}

/// How one row presents itself. The three flags travel together, so a caller cannot pass the
/// focus ring on a row that is not selected.
#[derive(Clone, Copy)]
struct RowState {
    selected: bool,
    focused: bool,
    copied: bool,
}

impl Feedback {
    fn is_current(&self, now: Instant) -> bool {
        now.duration_since(self.at) < FEEDBACK_DURATION
    }
}

impl ForwardsView {
    pub fn new(
        dock: Entity<DockPanel>,
        new_callback: Option<NewForwardCallback>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&dock, |view, _dock, cx| {
            // The selection follows the rows the user can see, so a filter that hides it moves
            // the selection instead of leaving the keyboard on a row that is not drawn.
            let forwards = view.visible_forwards(cx);
            view.reconcile_selection(&forwards);
            cx.notify();
        });
        let filter_input = cx.new(|cx| {
            TextInput::new("Filter forwards…", cx, |_text, _cx| {})
                .with_accessibility(
                    "Filter Port Forwards",
                    "Type text to match a target, namespace, address, or state. Press Escape to clear the filter.",
                    "Clear Port Forward Filter",
                )
                .with_width(px(FILTER_WIDTH))
        });
        let filter_observation = cx.observe(&filter_input, |view, input, cx| {
            let query = input.read(cx).text().to_owned();
            view.set_filter(query, cx);
        });
        Self {
            dock,
            selected: None,
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            new_focus: cx.focus_handle().tab_stop(true).tab_index(1isize),
            new_callback,
            filter_input,
            filter: String::new(),
            sort: Sort::default(),
            feedback: None,
            horizontal_scroll: ScrollHandle::new(),
            list_scroll: UniformListScrollHandle::new(),
            _dock_observation: observation,
            _filter_observation: filter_observation,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_new_callback(
        &mut self,
        callback: Option<NewForwardCallback>,
        cx: &mut Context<Self>,
    ) {
        self.new_callback = callback;
        cx.notify();
    }

    fn snapshots(&self, cx: &App) -> Vec<ForwardSnapshot> {
        self.dock.read(cx).forward_snapshots()
    }

    /// Forwards that match the filter, in the sorted order the user chose.
    fn visible_forwards(&self, cx: &App) -> Vec<ForwardSnapshot> {
        let mut forwards: Vec<ForwardSnapshot> = self
            .snapshots(cx)
            .into_iter()
            .filter(|snapshot| matches_filter(snapshot, self.filter.as_str()))
            .collect();
        forwards.sort_by(|left, right| compare_forwards(left, right, self.sort));
        forwards
    }

    /// Move keyboard focus to the filter field, so `/` and Tab reach the same control.
    fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.filter_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn set_filter(&mut self, query: String, cx: &mut Context<Self>) {
        if self.filter == query {
            return;
        }
        self.filter = query;
        cx.notify();
    }

    fn set_sort(&mut self, column: Column, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled_to(column);
        cx.notify();
    }

    fn selected_index(&self, snapshots: &[ForwardSnapshot]) -> Option<usize> {
        self.selected.and_then(|selected| {
            snapshots
                .iter()
                .position(|snapshot| snapshot.id == selected)
        })
    }

    fn reconcile_selection(&mut self, snapshots: &[ForwardSnapshot]) {
        if self.selected.is_some() && self.selected_index(snapshots).is_none() {
            self.selected = snapshots.first().map(|snapshot| snapshot.id);
            // The row the selection followed is gone. Keep the list anchored to the top
            // instead of leaving it scrolled past the end.
            self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
    }

    fn select(&mut self, id: ForwardId, cx: &mut Context<Self>) {
        let forwards = self.visible_forwards(cx);
        let Some(index) = forwards.iter().position(|snapshot| snapshot.id == id) else {
            return;
        };
        if self.selected != Some(id) {
            self.selected = Some(id);
            let rows = build_rows(&forwards);
            let row = row_of_forward(&rows, id).unwrap_or(index);
            self.list_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let forwards = self.visible_forwards(cx);
        if forwards.is_empty() {
            return;
        }
        let rows = build_rows(&forwards);
        let current = self
            .selected
            .and_then(|selected| row_of_forward(&rows, selected));
        let Some(next) = step_forward_row(&rows, current, delta) else {
            return;
        };
        let Some(id) = row_forward(&rows[next]) else {
            return;
        };
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
        self.selected = Some(id);
        cx.notify();
    }

    /// Move the selection to the first or last forward, skipping the group captions. Returns
    /// whether there was a forward to move to.
    fn move_to_edge(&mut self, last: bool, cx: &mut Context<Self>) -> bool {
        let forwards = self.visible_forwards(cx);
        let rows = build_rows(&forwards);
        let Some(row) = step_forward_row(&rows, None, if last { -1 } else { 1 }) else {
            return false;
        };
        let Some(id) = row_forward(&rows[row]) else {
            return false;
        };
        self.list_scroll.scroll_to_item(
            row,
            if last {
                ScrollStrategy::Bottom
            } else {
                ScrollStrategy::Top
            },
        );
        self.selected = Some(id);
        cx.notify();
        true
    }

    fn activate(&mut self, action: RowAction, cx: &mut Context<Self>) {
        let id = action.id();
        match action {
            RowAction::Start(_) | RowAction::Retry(_) => {
                let result = self.dock.update(cx, |dock, cx| match action {
                    RowAction::Start(_) => dock.restart_forward(id, cx),
                    RowAction::Retry(_) => dock.retry_forward(id, cx),
                    RowAction::Stop(_) | RowAction::Waiting(_) => Ok(()),
                });
                if result.is_err() {
                    return;
                }
            }
            RowAction::Stop(_) => {
                self.dock.update(cx, |dock, cx| dock.stop_forward(id, cx));
            }
            RowAction::Waiting(_) => return,
        }
        self.selected = Some(id);
        cx.notify();
    }

    fn stop_all(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .snapshots(cx)
            .into_iter()
            .filter(|snapshot| is_stoppable(snapshot.phase))
            .map(|snapshot| snapshot.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        self.dock.update(cx, |dock, cx| {
            for id in ids {
                dock.stop_forward(id, cx);
            }
        });
        cx.notify();
    }

    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        let forwards = self.visible_forwards(cx);
        let Some(snapshot) = self
            .selected
            .and_then(|selected| forwards.iter().find(|snapshot| snapshot.id == selected))
            .or_else(|| forwards.first())
        else {
            return;
        };
        self.activate(row_action(snapshot), cx);
    }

    /// The forward an action applies to: the selected row, or the first one when the list has
    /// focus without a selection.
    fn addressable(&self, cx: &App) -> Option<ForwardSnapshot> {
        let forwards = self.visible_forwards(cx);
        self.selected
            .and_then(|selected| forwards.iter().find(|snapshot| snapshot.id == selected))
            .or_else(|| forwards.first())
            .cloned()
    }

    /// The address a forward answers on, or the reason it has none. A forward that holds no
    /// listener is refused in words, so no action hands over a port that answers nothing.
    fn address_of(
        &mut self,
        id: ForwardId,
        action: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let snapshot = self.forward(id, cx)?;
        if let Some(url) = forward_url(&snapshot) {
            return Some(url);
        }
        self.feedback = Some(Feedback {
            id,
            message: format!(
                "{} has no address to {action}. Start the forward first.",
                target_text(&snapshot)
            )
            .into(),
            severity: Severity::Warning,
            at: Instant::now(),
        });
        self.expire_feedback(cx);
        None
    }

    /// Copies a forward's address, so the user does not read the digits and type the URL.
    fn copy_url(&mut self, id: ForwardId, cx: &mut Context<Self>) {
        let Some(url) = self.address_of(id, "copy", cx) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        self.selected = Some(id);
        self.feedback = Some(Feedback {
            id,
            message: format!("Copied {url}.").into(),
            severity: Severity::Success,
            at: Instant::now(),
        });
        self.expire_feedback(cx);
    }

    /// Hands a forward's address to the platform browser.
    fn open_url(&mut self, id: ForwardId, cx: &mut Context<Self>) {
        let Some(url) = self.address_of(id, "open", cx) else {
            return;
        };
        cx.open_url(&url);
        self.selected = Some(id);
        self.feedback = Some(Feedback {
            id,
            message: format!("Opened {url} in the browser.").into(),
            severity: Severity::Info,
            at: Instant::now(),
        });
        self.expire_feedback(cx);
    }

    fn forward(&self, id: ForwardId, cx: &App) -> Option<ForwardSnapshot> {
        self.snapshots(cx)
            .into_iter()
            .find(|snapshot| snapshot.id == id)
    }

    /// Confirms an action in the toolbar, then lets the confirmation expire.
    fn expire_feedback(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FEEDBACK_DURATION).await;
            this.update(cx, |this, cx| {
                this.feedback = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// True while the keystroke carries the editing chord the list owns. Control+C and Control+O
    /// are free here: the log list uses the same copy chord, and the filter input keeps its own
    /// copy binding while it holds the keyboard.
    fn url_chord(keystroke: &gpui::Keystroke) -> Option<UrlChord> {
        let modifiers = keystroke.modifiers;
        if !modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
            return None;
        }
        if keystroke.key.eq_ignore_ascii_case("c") {
            return Some(UrlChord::Copy);
        }
        keystroke
            .key
            .eq_ignore_ascii_case("o")
            .then_some(UrlChord::Open)
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window)
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        if let Some(chord) = Self::url_chord(&event.keystroke) {
            let Some(id) = self.addressable(cx).map(|snapshot| snapshot.id) else {
                return;
            };
            match chord {
                UrlChord::Copy => self.copy_url(id, cx),
                UrlChord::Open => self.open_url(id, cx),
            }
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control {
            return;
        }
        let handled = match event.keystroke.key.as_str() {
            "down" => {
                self.move_selection(1, cx);
                true
            }
            "up" => {
                self.move_selection(-1, cx);
                true
            }
            "home" => self.move_to_edge(false, cx),
            "end" => self.move_to_edge(true, cx),
            "enter" | "return" => {
                self.activate_selected(cx);
                true
            }
            "/" => {
                self.focus_filter(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    /// One summary fact: a shape, a label, and a count.
    ///
    /// The shape comes from `design::health_icon`, so this toolbar and the status
    /// bar's port-forward chip give the same fact the same shape. They used to
    /// disagree: this one drew a warning triangle for a failure, where the shared
    /// vocabulary — and every failure in the table — draws a filled `×`.
    fn render_summary_item(
        label: &'static str,
        count: usize,
        severity: Severity,
        cx: &App,
    ) -> AnyElement {
        h_flex()
            .id(SharedString::from(format!("forwards-summary-{label}")))
            .flex_none()
            .gap(space::XS)
            .items_center()
            .child(
                Icon::new(design::health_icon(severity))
                    .size(IconSize::XSmall)
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(
                Label::new(format!("{label} {}", design::format::count(count)))
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_toolbar(
        &self,
        summary: ForwardSummary,
        stoppable: usize,
        visible: usize,
        total: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let new_callback = self.new_callback.clone();
        let new_enabled = new_callback.is_some();
        let new_tooltip = if new_enabled {
            "Create a port forward from a running Pod."
        } else {
            "The Shell will connect this action before a new port forward can be created."
        };
        let stop_all_aria = if stoppable == 1 {
            "Stop the only port forward that is still starting or running".to_owned()
        } else {
            format!("Stop all {stoppable} port forwards that are still starting or running")
        };
        let filtering = !self.filter.trim().is_empty();
        h_flex()
            .id("forwards-toolbar")
            .role(Role::Group)
            .aria_label(format!("{SUMMARY_LABEL}: {}", summary_text(summary)))
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .min_w(px(0.))
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .bg(colors.toolbar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border_variant)
            .child(self.filter_input.clone())
            // How much of the list the filter leaves, so a long list stays scannable and a
            // filter that hides everything is visible as a count rather than an empty table.
            .when(filtering, |this| {
                this.child(
                    h_flex()
                        .id("forwards-filter-status")
                        .debug_selector(|| "forwards-filter-status".to_owned())
                        .flex_none()
                        .role(Role::Status)
                        .aria_label(format!(
                            "{visible} of {total} port forwards match the filter"
                        ))
                        .child(
                            Label::new(format!("{visible} of {total}"))
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                h_flex()
                    .id("forwards-summary")
                    .flex_none()
                    .gap(space::MD)
                    .items_center()
                    .role(Role::Status)
                    .aria_label(summary_text(summary))
                    .child(Self::render_summary_item(
                        "Active",
                        summary.active,
                        Severity::Success,
                        cx,
                    ))
                    .child(Self::render_summary_item(
                        "Failed",
                        summary.failed,
                        Severity::Error,
                        cx,
                    ))
                    .child(Self::render_summary_item(
                        "Pending",
                        summary.pending,
                        Severity::Warning,
                        cx,
                    ))
                    .child(Self::render_summary_item(
                        "Stopped",
                        summary.stopped,
                        Severity::Muted,
                        cx,
                    )),
            )
            // A copy or an open has no visible result, so the toolbar says what happened.
            .when_some(self.feedback.as_ref(), |this, feedback| {
                this.child(
                    h_flex()
                        .id("forwards-feedback")
                        .debug_selector(|| "forwards-feedback".to_owned())
                        .flex_none()
                        .gap(space::XS)
                        .items_center()
                        .role(Role::Status)
                        .aria_label(feedback.message.to_string())
                        .child(
                            // One shape per severity, from the app's only health
                            // vocabulary. This match gave Error and Warning the
                            // same glyph, so a failure and a warning were the
                            // same shape in two colours, and Info, Neutral and
                            // Muted all drew a check.
                            Icon::new(design::health_icon(feedback.severity))
                                .size(IconSize::XSmall)
                                .color(Color::Custom(feedback.severity.marker(cx))),
                        )
                        .child(
                            Label::new(feedback.message.clone())
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Default)
                                .truncate(),
                        ),
                )
            })
            // A forward that is already stopping has nothing left to stop, so the action
            // only appears while it can still end a forward.
            .when(stoppable > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .debug_selector(|| "forwards-stop-all".to_owned())
                        .child(
                            Button::new("forwards-stop-all", "Stop All")
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Medium)
                                .label_size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::BODY,
                                ))))
                                .tab_index(1isize)
                                .tooltip(Tooltip::text(
                                    "Stop all forwards that are still starting or running",
                                ))
                                .aria_label(stop_all_aria)
                                .on_click(cx.listener(|view, _, _, cx| view.stop_all(cx))),
                        ),
                )
            })
            .child(div().flex_1().min_w(space::SM))
            .child(
                Button::new("forwards-new", "New Port Forward…")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .width(px(148.))
                    .tab_index(1isize)
                    .track_focus(&self.new_focus)
                    .tooltip(Tooltip::text(new_tooltip))
                    .aria_label("Create a new port forward")
                    .disabled(!new_enabled)
                    .on_click(move |_, window, cx| {
                        if let Some(callback) = &new_callback {
                            callback(window, cx);
                        }
                    }),
            )
            .into_any_element()
    }

    /// One column heading. Clicking it sorts the table, and the direction is spoken in the
    /// accessible name because the row of headings carries no other state.
    fn render_header_cell(
        &self,
        column: Column,
        width: Option<f32>,
        index: usize,
        panel: &gpui::WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let label = column.title();
        let sorted = self.sort.reads(column);
        let direction = self.sort.direction;
        let spoken = if sorted {
            format!("{label}, {}", direction.read_text())
        } else {
            label.to_owned()
        };
        let next_step = if sorted {
            format!(
                "Select to sort from {}.",
                match direction {
                    Direction::Ascending => "high to low",
                    Direction::Descending => "low to high",
                }
            )
        } else {
            "Select to sort from low to high.".to_owned()
        };
        let panel = panel.clone();
        let cell = div()
            .id(SharedString::from(format!("forwards-header-{}", label)))
            // The element id alone stays invisible to `VisualTestContext::debug_bounds`, which is
            // how a heading a test clicks could read as missing.
            .debug_selector(move || format!("forwards-header-{label}"))
            .role(Role::ColumnHeader)
            .aria_label(spoken)
            .aria_description(next_step)
            .aria_column_index(index)
            .when_some(width, |this, width| this.flex_none().w(px(width)))
            .when(width.is_none(), |this| {
                this.flex_1().min_w(px(TARGET_MIN_WIDTH))
            })
            .h_full()
            .flex()
            .items_center()
            .gap(space::XS)
            .cursor_pointer()
            .text_size(rems_from_px(f32::from(design::text::METADATA)))
            .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(if sorted {
                cx.theme().colors().text
            } else {
                cx.theme().colors().text_muted
            })
            .whitespace_nowrap()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label),
            )
            .when(sorted, |this| {
                this.child(
                    Icon::new(match direction {
                        Direction::Ascending => IconName::ArrowUp,
                        Direction::Descending => IconName::ArrowDown,
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
                )
            })
            .on_click(move |_, _, cx| {
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |view, cx| view.set_sort(column, cx));
                    cx.stop_propagation();
                }
            });
        cell.into_any_element()
    }

    fn render_header(&self, panel: &gpui::WeakEntity<Self>, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .id("forwards-header")
            .role(Role::Row)
            .aria_row_index(1)
            .flex_none()
            .w_full()
            .min_w(px(MIN_TABLE_WIDTH))
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(self.render_header_cell(Column::State, Some(STATE_COLUMN_WIDTH), 1, panel, cx))
            .child(self.render_header_cell(Column::Url, Some(URL_COLUMN_WIDTH), 2, panel, cx))
            .child(self.render_header_cell(Column::Target, None, 3, panel, cx))
            .child(
                div()
                    .id("forwards-header-actions")
                    .role(Role::ColumnHeader)
                    .aria_label("Actions")
                    .aria_column_index(4)
                    .flex_none()
                    .w(px(ACTIONS_COLUMN_WIDTH))
                    .text_size(rems_from_px(f32::from(design::text::METADATA)))
                    .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().colors().text_muted)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child("Actions"),
            )
            .into_any_element()
    }

    /// One data cell: the monospaced data role at the reader's configured size.
    ///
    /// It already receives the configured `DataTypography`, and read the
    /// `design::text::DATA` constant instead, so the "Data font size" setting
    /// changed the font and the line height stayed at 18px — a taller glyph in a
    /// shorter box. `DESIGN.md` §3.1 calls these logical values, not numbers in
    /// components.
    fn data_text(
        text: impl Into<SharedString>,
        typography: &DataTypography,
        color: Color,
        cx: &App,
    ) -> gpui::Div {
        div()
            .font(typography.font.clone())
            .font_features(typography.features.clone())
            .text_size(rems_from_px(f32::from(typography.size)))
            .line_height(rems_from_px(f32::from(typography.line_height)))
            .text_color(color.color(cx))
            .child(text.into())
    }

    fn state_cell(snapshot: &ForwardSnapshot, key: &str, cx: &App) -> AnyElement {
        let (icon, label, severity, next_step) = phase_presentation(snapshot.phase);
        let error = snapshot
            .error
            .clone()
            .map(|error| error.trim().to_owned())
            .filter(|error| !error.is_empty());
        let label_size = LabelSize::Custom(rems_from_px(f32::from(design::text::BODY)));
        let mut cell = h_flex()
            .id(SharedString::from(format!("forwards-state-{key}")))
            .role(Role::Cell)
            .aria_label(format!("State {label}. {next_step}"))
            .aria_column_index(1)
            .flex_none()
            .w(px(STATE_COLUMN_WIDTH))
            .min_w(px(0.))
            .gap(space::XS)
            .items_center()
            .child(
                Icon::new(icon)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(
                Label::new(label)
                    .size(label_size)
                    .color(Color::Default)
                    .truncate(),
            );
        // The raw reason belongs in a tooltip and in the spoken description, not in the
        // truncated state label.
        if let Some(reason) = error.clone() {
            cell = cell.aria_description(reason.clone());
            cell.interactivity().tooltip(Tooltip::text(reason));
        }
        cell.into_any_element()
    }

    /// The address a forward answers on. A forward with no listener shows a dash and says why,
    /// so the column never advertises a port that is gone.
    fn url_cell(
        snapshot: &ForwardSnapshot,
        key: &str,
        typography: &DataTypography,
        cx: &App,
    ) -> AnyElement {
        let url = forward_url(snapshot);
        let value = url.clone().unwrap_or_else(|| "-".to_owned());
        let substitution = port_substitution(snapshot);
        let spoken = match (&url, snapshot.phase) {
            (Some(url), _) => format!("Address {url}"),
            (None, ForwardPhase::Failed) => "No address. The forward failed.".to_owned(),
            (None, ForwardPhase::Starting) => "No address yet. The forward is starting.".to_owned(),
            (None, ForwardPhase::Stopped) => "No address. The forward is stopped.".to_owned(),
            (None, ForwardPhase::Stopping) => "No address. The forward is stopping.".to_owned(),
            (&None, ForwardPhase::Running) => {
                "No address. The forward is running but holds no port.".to_owned()
            }
        };
        let mut cell = Self::data_text(value, typography, Color::Default, cx)
            .id(SharedString::from(format!("forwards-url-{key}")))
            .role(Role::Cell)
            .aria_label(spoken)
            .aria_column_index(2)
            .flex_none()
            .w(px(URL_COLUMN_WIDTH))
            .min_w(px(0.))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis();
        // The address fits the column, so it is read in place. The substitution is longer than
        // the cell and belongs in the tooltip and the spoken description. An element carries one
        // tooltip, so the substitution wins: the address is already in the cell.
        if let Some(substitution) = substitution {
            cell = cell.aria_description(substitution.clone());
            cell.interactivity().tooltip(Tooltip::text(substitution));
        } else if let Some(url) = url {
            cell.interactivity().tooltip(Tooltip::text(url));
        }
        cell.into_any_element()
    }

    fn target_cell(
        snapshot: &ForwardSnapshot,
        key: &str,
        typography: &DataTypography,
        cx: &App,
    ) -> AnyElement {
        let value = target_text(snapshot);
        let mut cell = Self::data_text(value.clone(), typography, Color::Default, cx)
            .id(SharedString::from(format!("forwards-target-{key}")))
            .role(Role::Cell)
            .aria_label(value.clone())
            .aria_column_index(3)
            .flex_1()
            .min_w(px(TARGET_MIN_WIDTH))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis();
        cell.interactivity().tooltip(Tooltip::text(value));
        cell.into_any_element()
    }

    fn render_action(
        action: RowAction,
        target: &str,
        panel: &gpui::WeakEntity<Self>,
    ) -> AnyElement {
        let id = action.id();
        let name = SharedString::from(format!("{}-{}", action.name(), id_key(id)));
        let control = match action {
            RowAction::Start(_) => {
                let panel = panel.clone();
                Button::new(name.clone(), "Start")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .width(px(ACTION_TEXT_WIDTH))
                    .tab_index(2isize)
                    .tooltip(Tooltip::text("Start this port forward again"))
                    .aria_label(format!("Start port forward for {target}"))
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |view, cx| {
                                view.activate(RowAction::Start(id), cx);
                                cx.stop_propagation();
                            });
                        }
                    })
                    .into_any_element()
            }
            RowAction::Stop(_) => {
                let panel = panel.clone();
                IconButton::new(name.clone(), IconName::Stop)
                    .style(ButtonStyle::OutlinedGhost)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .width(design::size::CONTROL)
                    .tab_index(2isize)
                    .tooltip(Tooltip::text("Stop this port forward"))
                    .aria_label(format!("Stop port forward for {target}"))
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |view, cx| {
                                view.activate(RowAction::Stop(id), cx);
                                cx.stop_propagation();
                            });
                        }
                    })
                    .into_any_element()
            }
            RowAction::Retry(_) => {
                let panel = panel.clone();
                Button::new(name.clone(), "Retry")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .label_size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .width(px(ACTION_TEXT_WIDTH))
                    .tab_index(2isize)
                    .tooltip(Tooltip::text("Retry this port forward"))
                    .aria_label(format!("Retry port forward for {target}"))
                    .on_click(move |_, _, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |view, cx| {
                                view.activate(RowAction::Retry(id), cx);
                                cx.stop_propagation();
                            });
                        }
                    })
                    .into_any_element()
            }
            RowAction::Waiting(_) => IconButton::new(name.clone(), IconName::Stop)
                .style(ButtonStyle::OutlinedGhost)
                .size(ButtonSize::Medium)
                .icon_size(IconSize::XSmall)
                .width(design::size::CONTROL)
                .tab_index(2isize)
                .disabled(true)
                .tooltip(Tooltip::text("Waiting for the port forward to stop"))
                .aria_label(format!("Stopping port forward for {target}"))
                .into_any_element(),
        };
        // The control is wrapped so it carries a debug selector. An element id alone stays
        // invisible to `VisualTestContext::debug_bounds` and to the inspector, which is how a
        // Retry that rendered for real could still read as missing.
        div()
            .flex_none()
            .debug_selector(move || name.to_string())
            .child(control)
            .into_any_element()
    }

    /// The Copy URL control. A forward with no address disables it, so the button never offers
    /// a value that answers nothing.
    fn render_copy_url(
        id: ForwardId,
        target: &str,
        available: bool,
        copied: bool,
        panel: &gpui::WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let name = SharedString::from(format!("forwards-copy-url-{}", id_key(id)));
        let panel_for_click = panel.clone();
        let control = IconButton::new(name.clone(), IconName::Copy)
            .style(ButtonStyle::OutlinedGhost)
            .size(ButtonSize::Medium)
            .icon_size(IconSize::XSmall)
            .width(design::size::CONTROL)
            .tab_index(2isize)
            .disabled(!available)
            // The icon carries the confirmation, so a copy reads as done without a toast.
            .icon_color(if copied {
                Color::Custom(Severity::Success.marker(cx))
            } else {
                Color::Muted
            })
            .tooltip(Tooltip::text(if copied {
                "Copied to the clipboard"
            } else if available {
                "Copy this address to the clipboard"
            } else {
                NO_ADDRESS_TOOLTIP
            }))
            .aria_label(if copied {
                format!("Copied the address for {target}")
            } else {
                format!("Copy the address for {target}")
            })
            .on_click(move |_, _, cx| {
                if let Some(panel) = panel_for_click.upgrade() {
                    panel.update(cx, |view, cx| {
                        view.copy_url(id, cx);
                        cx.stop_propagation();
                    });
                }
            });
        div()
            .flex_none()
            .debug_selector(move || name.to_string())
            .child(control)
            .into_any_element()
    }

    /// The Open in Browser control. It shares the Copy URL availability, because both hand over
    /// the same address.
    fn render_open_url(
        id: ForwardId,
        target: &str,
        available: bool,
        panel: &gpui::WeakEntity<Self>,
    ) -> AnyElement {
        let name = SharedString::from(format!("forwards-open-url-{}", id_key(id)));
        let panel_for_click = panel.clone();
        let control = IconButton::new(name.clone(), IconName::Link)
            .style(ButtonStyle::OutlinedGhost)
            .size(ButtonSize::Medium)
            .icon_size(IconSize::XSmall)
            .width(design::size::CONTROL)
            .tab_index(2isize)
            .disabled(!available)
            .tooltip(Tooltip::text(if available {
                "Open this address in the browser"
            } else {
                NO_ADDRESS_TOOLTIP
            }))
            .aria_label(format!("Open the address for {target} in the browser"))
            .on_click(move |_, _, cx| {
                if let Some(panel) = panel_for_click.upgrade() {
                    panel.update(cx, |view, cx| {
                        view.open_url(id, cx);
                        cx.stop_propagation();
                    });
                }
            });
        div()
            .flex_none()
            .debug_selector(move || name.to_string())
            .child(control)
            .into_any_element()
    }

    fn action_cell(
        snapshot: &ForwardSnapshot,
        target: &str,
        copied: bool,
        panel: &gpui::WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let available = forward_url(snapshot).is_some();
        h_flex()
            .id(SharedString::from(format!(
                "forwards-actions-{}",
                id_key(snapshot.id)
            )))
            .role(Role::Cell)
            .aria_column_index(4)
            .flex_none()
            .w(px(ACTIONS_COLUMN_WIDTH))
            .min_w(px(0.))
            .gap(space::XS)
            .items_center()
            .child(Self::render_copy_url(
                snapshot.id,
                target,
                available,
                copied,
                panel,
                cx,
            ))
            .child(Self::render_open_url(snapshot.id, target, available, panel))
            .child(Self::render_action(row_action(snapshot), target, panel))
            .into_any_element()
    }

    fn row_content(
        snapshot: &ForwardSnapshot,
        key: &str,
        typography: &DataTypography,
        copied: bool,
        panel: &gpui::WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .min_w(px(MIN_TABLE_WIDTH - 2. * f32::from(space::SM)))
            .h(design::size::ROW)
            .gap(space::SM)
            .items_center()
            .child(Self::state_cell(snapshot, key, cx))
            .child(Self::url_cell(snapshot, key, typography, cx))
            .child(Self::target_cell(snapshot, key, typography, cx))
            .child(Self::action_cell(
                snapshot,
                &target_text(snapshot),
                copied,
                panel,
                cx,
            ))
            .into_any_element()
    }

    fn render_row(
        row: usize,
        snapshot: &ForwardSnapshot,
        state: RowState,
        typography: &DataTypography,
        panel: &gpui::WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let RowState {
            selected,
            focused,
            copied,
        } = state;
        let key = id_key(snapshot.id);
        let aria_label = row_aria_label(snapshot);
        // The row is set in the data role, so it follows the reader's data font
        // instead of cropping what that setting just made larger. At the default
        // size this is still `design::size::ROW`.
        let row_height = common::data_row_height(typography);
        // The row owns every one of its states, and the `ListItem` inside it is told nothing
        // about them. `ListItem` has no `selected_style`, so a `selectable` item paints
        // `ghost_element.selected` — an opaque token from the theme's control ramp, not the solved
        // accent wash every other row in this app draws — when it is told it is selected, and
        // `ghost_element.hover` over `design::row_hover_bg` whenever the pointer is over it, which
        // for a full-width item is always. `selectable` defaults to true upstream, so the row has
        // to say no rather than merely stay quiet. The row keeps the background, the rail and
        // `aria_selected`, and the item is only the focusable, clickable box inside it.
        let mut item = ListItem::new(SharedString::from(format!("forwards-item-{key}")))
            .height(row_height)
            .spacing(ListItemSpacing::ExtraDense)
            .selectable(false)
            .focused(focused)
            .aria_role(Role::ListItem)
            .aria_label(aria_label.clone())
            .aria_keyshortcuts("Enter Control+C Control+O");
        if focused {
            item = item.aria_active_descendant();
        }
        let row_panel = panel.clone();
        let row_id = snapshot.id;
        let item = item.on_click(move |_, window, cx| {
            if let Some(panel) = row_panel.upgrade() {
                panel.update(cx, |view, cx| {
                    view.select(row_id, cx);
                    window.focus(&view.focus_handle, cx);
                    cx.stop_propagation();
                });
            }
        });
        let content = Self::row_content(snapshot, &key, typography, copied, panel, cx);
        h_flex()
            .id(SharedString::from(format!("forwards-row-{key}")))
            .debug_selector(move || format!("forwards-row-{row}"))
            .role(Role::Row)
            .aria_row_index(row + 2)
            .aria_label(aria_label)
            .aria_selected(selected)
            .when(focused, |this| this.aria_active_descendant())
            .relative()
            .w_full()
            .min_w(px(MIN_TABLE_WIDTH))
            .h(row_height)
            .cursor_pointer()
            .when(selected, |this| this.bg(design::row_selected_bg(cx)))
            .when(focused && !selected, |this| {
                this.bg(design::row_hover_bg(cx))
            })
            .when(!selected && !focused, |this| {
                this.hover(|this| this.bg(design::row_hover_bg(cx)))
                    .active(|this| this.bg(cx.theme().colors().element_active))
            })
            .when(selected || focused, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(design::border::FOCUS_RAIL)
                        .bg(if selected {
                            cx.theme().colors().text_accent
                        } else {
                            cx.theme().colors().border_focused
                        }),
                )
            })
            .child(item.child(content))
            .into_any_element()
    }

    /// The caption that introduces the forwards of one namespace. It keeps the shared row
    /// rhythm, so grouping costs no extra vertical space.
    fn render_group_row(row: usize, namespace: &SharedString, count: usize) -> AnyElement {
        let label = if count == 1 {
            format!("{namespace} · 1 port forward")
        } else {
            format!("{namespace} · {count} port forwards")
        };
        h_flex()
            .id(SharedString::from(format!("forwards-group-{namespace}")))
            .debug_selector(move || format!("forwards-group-{row}"))
            .role(Role::Row)
            .aria_row_index(row + 2)
            .aria_label(label.clone())
            .w_full()
            .min_w(px(MIN_TABLE_WIDTH))
            .h(design::size::ROW)
            .px(space::SM)
            .items_center()
            .child(
                Label::new(label)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_empty(&self, filtering: bool, _cx: &Context<Self>) -> AnyElement {
        let (title, hint) = if filtering {
            (FILTERED_EMPTY_TITLE, FILTERED_EMPTY_HINT)
        } else {
            (EMPTY_TITLE, EMPTY_HINT)
        };
        v_flex()
            .id("forwards-empty")
            .debug_selector(|| "forwards-empty".to_owned())
            .flex_1()
            .min_h(px(f32::from(design::size::ROW) * 4.))
            .w_full()
            .items_center()
            .justify_center()
            .gap(space::SM)
            .px(space::XL)
            .role(Role::Status)
            .aria_label(title)
            .aria_description(hint)
            .child(
                // The shared empty state draws `design::size::ICON_LARGE`, and the
                // problems glyph has one owner, so this one does not keep a second
                // size or a second funnel.
                Icon::new(if filtering {
                    design::problems_filter_icon(true)
                } else {
                    IconName::Box
                })
                .size(IconSize::Custom(rems_from_px(f32::from(
                    design::size::ICON_LARGE,
                ))))
                .color(Color::Muted),
            )
            .child(
                Label::new(title)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::BODY,
                    ))))
                    .color(Color::Default),
            )
            .child(
                Label::new(hint)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_grid(
        &self,
        forwards: &[ForwardSnapshot],
        focused_id: Option<ForwardId>,
        typography: &DataTypography,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filtering = !self.filter.trim().is_empty();
        let rows = build_rows(forwards);
        // The row that was just copied or opened keeps its confirmation color while the
        // confirmation is on screen.
        let copied_id = self
            .feedback
            .as_ref()
            .filter(|feedback| feedback.is_current(Instant::now()))
            .map(|feedback| feedback.id);
        let panel = cx.entity().downgrade();
        let header = self.render_header(&panel, cx);
        let body = if rows.is_empty() {
            self.render_empty(filtering, cx)
        } else {
            let selected = self.selected;
            let forwards = forwards.to_vec();
            let rows = rows.to_vec();
            let typography = typography.clone();
            let list = uniform_list("forwards-rows", rows.len(), move |range, _window, cx| {
                range
                    .filter_map(|row| match rows.get(row)? {
                        Row::Group { namespace, count } => {
                            Some(Self::render_group_row(row, namespace, *count))
                        }
                        Row::Forward(id) => {
                            let snapshot = forwards.iter().find(|snapshot| snapshot.id == *id)?;
                            Some(Self::render_row(
                                row,
                                snapshot,
                                RowState {
                                    selected: selected == Some(snapshot.id),
                                    focused: focused_id == Some(snapshot.id),
                                    copied: copied_id == Some(snapshot.id),
                                },
                                &typography,
                                &panel,
                                cx,
                            ))
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(Some(0))
            .track_scroll(&self.list_scroll)
            .size_full();
            div()
                .id("forwards-rows-scroll")
                .flex_1()
                .h_full()
                .min_h(px(0.))
                .w_full()
                .child(list)
                .into_any_element()
        };
        v_flex()
            .id("forwards-grid")
            .role(Role::Grid)
            .aria_label("Port forward table")
            .aria_description(
                "Use Up and Down to select a port forward. Home and End move to the first and last row. Enter runs the available action. Control C copies the address and Control O opens it.",
            )
            .aria_keyshortcuts("ArrowUp ArrowDown Home End Enter Control+C Control+O /")
            .aria_row_count(rows.len() + 1)
            .aria_column_count(4)
            .tab_group()
            .key_context("Forwards")
            // The grid needs the panel height, otherwise the row list has no bounded
            // height and cannot scroll.
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .w_full()
            .min_w(px(MIN_TABLE_WIDTH))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_footer(&self, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .id("forwards-footer")
            .flex_none()
            .w_full()
            .min_w(px(0.))
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .role(Role::Group)
            .aria_label("Port forward keyboard shortcuts")
            .child(
                Icon::new(IconName::Keyboard)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(FOOTER_TEXT)
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    }
}

impl Render for ForwardsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (snapshots, summary) = {
            let dock = self.dock.read(cx);
            (dock.forward_snapshots(), dock.forward_summary())
        };
        let forwards = self.visible_forwards(cx);
        // A selection the filter or a sort moved out of view follows the first visible forward,
        // so the keyboard never rests on a row that is not drawn.
        self.reconcile_selection(&forwards);
        let root_focused = self.focus_handle.is_focused(window);
        let focused_id =
            focused_index(self.selected_index(&forwards), root_focused, forwards.len())
                .and_then(|index| forwards.get(index).map(|snapshot| snapshot.id));
        let typography = settings::data_typography(cx);
        let colors = cx.theme().colors();
        v_flex()
            .id("forwards-view")
            .role(Role::Region)
            .aria_label("Port forwards")
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(design::surface::canvas(cx).alpha(1.0))
            .text_color(colors.text)
            .font_ui(cx)
            .key_context("Forwards")
            .track_focus(&self.focus_handle)
            .tab_index(0)
            .focus_visible(|style| style.border_l_2().border_color(colors.border_focused))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_toolbar(
                summary,
                stoppable_count(&snapshots),
                forwards.len(),
                snapshots.len(),
                cx,
            ))
            .child(
                div()
                    .id("forwards-table-scroll")
                    .debug_selector(|| "forwards-table-scroll".to_owned())
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.horizontal_scroll)
                    .child(self.render_grid(&forwards, focused_id, &typography, cx)),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Context, Render, TestAppContext, Window, div};
    use theme::LoadThemes;

    use super::*;
    use crate::panels::terminal::{
        ForwardHandle, ForwardRequest, PortForwardFactory, StartedForward, TerminalFactory,
        TerminalServices,
    };

    /// The row's states come from `design`, not from the `ListItem` inside it.
    ///
    /// `ListItem` has no `selected_style`: it paints `ghost_element.selected` when it is told it
    /// is selected, and `ghost_element.hover` whenever the pointer is over it, both opaque control
    /// -ramp tokens that land on top of the `design::row_selected_bg` and `design::row_hover_bg`
    /// the row already set. The result is invisible in a bounds assertion and wrong in both
    /// appearances, so the calls are banned here instead of trusted to review.
    #[test]
    fn the_row_states_are_the_rows_own() {
        let rendered = include_str!("forwards.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("this file has one cfg(test) block");
        for (call, what) in [
            (
                ".toggle_state(",
                "a `ListItem` told it is selected paints `ghost_element.selected` over \
                 `design::row_selected_bg`",
            ),
            (
                ".selectable(true)",
                "`selectable` defaults to true upstream, so asking for it paints \
                 `ghost_element.hover` over `design::row_hover_bg` on every hover",
            ),
        ] {
            assert!(
                !rendered.contains(call),
                "{what}. The row owns its selection, its rail and its hover."
            );
        }
    }

    struct FakeForwardHandle;

    impl ForwardHandle for FakeForwardHandle {
        fn stop(&mut self) {}
    }

    #[derive(Default)]
    struct ForwardFactoryState {
        bindings: Vec<tokio::sync::oneshot::Sender<Result<u16, String>>>,
        errors: Vec<tokio::sync::mpsc::UnboundedSender<String>>,
        /// Every request the Dock asked the factory to start, so a test can follow the port the
        /// user asked for across a restart.
        requests: Vec<ForwardRequest>,
    }

    fn test_services(state: Rc<RefCell<ForwardFactoryState>>) -> TerminalServices {
        let terminals: TerminalFactory = Rc::new(|_, _, _| Err("unused".to_owned()));
        let forwards: PortForwardFactory = Rc::new(move |request, _cx| {
            let (binding_sender, binding) = tokio::sync::oneshot::channel();
            let (error_sender, errors) = tokio::sync::mpsc::unbounded_channel();
            let mut state = state.borrow_mut();
            state.bindings.push(binding_sender);
            state.errors.push(error_sender);
            state.requests.push(request);
            drop(state);
            Ok(StartedForward {
                handle: Box::new(FakeForwardHandle),
                binding,
                errors,
            })
        });
        TerminalServices {
            terminals,
            forwards,
            context: Some("test-cluster".to_owned()),
            namespace: Some("default".to_owned()),
        }
    }

    struct ForwardsHarness {
        view: Entity<ForwardsView>,
    }

    impl Render for ForwardsHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.view.clone())
        }
    }

    fn forwards_harness(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ForwardsView>,
        Entity<DockPanel>,
        Rc<RefCell<ForwardFactoryState>>,
        &mut gpui::VisualTestContext,
    ) {
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            dock.update(cx, |dock, cx| {
                dock.set_terminal_services(Some(services), cx)
            });
            dock
        });
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| ForwardsView::new(dock.clone(), None, cx));
            ForwardsHarness { view }
        });
        cx.run_until_parked();
        let view = harness.read_with(cx, |harness, _| harness.view.clone());
        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        (view, dock, state, cx)
    }

    /// `debug_bounds` takes a `&'static str`, so a per-forward selector is leaked once.
    fn leaked(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    #[test]
    fn summary_text_covers_all_forward_states() {
        assert_eq!(
            summary_text(ForwardSummary {
                active: 2,
                failed: 1,
                pending: 3,
                stopped: 4,
            }),
            "2 active, 1 failed, 3 pending, 4 stopped"
        );
    }

    /// Every phase's shape comes from the one health vocabulary, and the words
    /// beside it name the phase.
    ///
    /// The panel used to keep a private map in which a failure drew a warning
    /// triangle — so the same failure looked like a warning here and like an
    /// error in the table and the status bar — and `shell/status_bar.rs` kept a
    /// second copy of it. `DESIGN.md` §4 reserves the glyphs for
    /// `design::health_icon`.
    #[test]
    fn every_phase_shape_comes_from_the_shared_health_vocabulary() {
        for phase in [
            ForwardPhase::Stopped,
            ForwardPhase::Starting,
            ForwardPhase::Running,
            ForwardPhase::Stopping,
            ForwardPhase::Failed,
        ] {
            let (icon, label, severity, next_step) = phase_presentation(phase);
            assert_eq!(
                icon,
                design::health_icon(severity),
                "{phase:?} draws a glyph outside the shared vocabulary"
            );
            assert!(!label.is_empty(), "{phase:?} prints no name");
            assert!(!next_step.is_empty(), "{phase:?} offers no next step");
        }
        assert_eq!(
            phase_presentation(ForwardPhase::Failed).0,
            design::health_icon(Severity::Error),
            "a failed forward is an error, not a warning"
        );
        assert_eq!(
            phase_presentation(ForwardPhase::Running).0,
            design::health_icon(Severity::Success)
        );
        // Two phases in the same health class share a shape, and the label is what
        // separates them. That is the answer `Connecting` and `Reconnecting`
        // already give in the status bar.
        let starting = phase_presentation(ForwardPhase::Starting);
        let stopping = phase_presentation(ForwardPhase::Stopping);
        assert_eq!(starting.0, stopping.0);
        assert_ne!(starting.1, stopping.1);
        assert_ne!(starting.3, stopping.3);
    }

    #[test]
    fn focused_index_uses_the_first_row_when_focused() {
        assert_eq!(focused_index(None, true, 3), Some(0));
        assert_eq!(focused_index(Some(2), true, 3), Some(2));
        assert_eq!(focused_index(Some(2), false, 3), None);
        assert_eq!(focused_index(Some(0), true, 0), None);
    }

    fn request(
        namespace: &str,
        name: &str,
        remote_port: u16,
        local_port: Option<u16>,
    ) -> ForwardRequest {
        ForwardRequest {
            context: Some("test-cluster".to_owned()),
            namespace: Some(namespace.to_owned()),
            name: name.into(),
            remote_port,
            local_port,
        }
    }

    fn snapshot_for(
        id: u64,
        namespace: &str,
        name: &str,
        remote_port: u16,
        phase: ForwardPhase,
        local_port: Option<u16>,
        error: Option<&str>,
    ) -> ForwardSnapshot {
        ForwardSnapshot {
            id: ForwardId(id),
            phase,
            request: request(namespace, name, remote_port, None),
            label: format!("{name}:{remote_port}").into(),
            remote_port,
            local_port,
            error: error.map(SharedString::from),
        }
    }

    fn snapshot(
        phase: ForwardPhase,
        local_port: Option<u16>,
        error: Option<&str>,
    ) -> ForwardSnapshot {
        snapshot_for(1, "default", "pod-a", 8080, phase, local_port, error)
    }

    #[test]
    fn failed_forward_hides_the_port_it_used_to_bind() {
        let failed = snapshot(
            ForwardPhase::Failed,
            Some(34_567),
            Some("connection refused"),
        );
        assert_eq!(
            live_local_port(&failed),
            None,
            "a failed forward has no listener, so the port column must be empty"
        );
        assert_eq!(
            live_local_port(&snapshot(ForwardPhase::Running, Some(34_567), None)),
            Some(34_567)
        );
        assert_eq!(
            live_local_port(&snapshot(ForwardPhase::Stopping, Some(34_567), None)),
            Some(34_567),
            "a stopping forward still owns its listener"
        );
        assert_eq!(
            live_local_port(&snapshot(ForwardPhase::Starting, Some(34_567), None)),
            None,
            "a new attempt does not own the previous port yet"
        );
        assert_eq!(
            live_local_port(&snapshot(ForwardPhase::Stopped, None, None)),
            None
        );
    }

    #[test]
    fn only_a_forward_that_holds_a_listener_exposes_an_address() {
        let mut running = snapshot(ForwardPhase::Running, Some(8081), None);
        assert_eq!(
            forward_url(&running),
            Some("http://localhost:8081".to_owned()),
            "a running forward is one keystroke from the clipboard"
        );
        assert_eq!(
            forward_url(&snapshot(ForwardPhase::Stopping, Some(8081), None)),
            Some("http://localhost:8081".to_owned())
        );
        for phase in [
            ForwardPhase::Failed,
            ForwardPhase::Stopped,
            ForwardPhase::Starting,
        ] {
            running.phase = phase;
            assert_eq!(
                forward_url(&running),
                None,
                "{phase:?} keeps no listener, so it has no address to hand over"
            );
        }
    }

    #[test]
    fn a_taken_local_port_is_reported_rather_than_hidden() {
        let mut running = snapshot(ForwardPhase::Running, Some(34_567), None);
        assert_eq!(
            port_substitution(&running),
            None,
            "a forward on the port the user asked for has nothing to report"
        );
        running.request.local_port = Some(8080);
        assert_eq!(
            port_substitution(&running),
            Some(
                "Local port 8080 was already in use, so this forward listens on 34567.".to_owned()
            ),
            "the user may have pointed something at 8080, so the swap must be named"
        );
        running.phase = ForwardPhase::Failed;
        assert_eq!(
            port_substitution(&running),
            None,
            "a failed forward holds no port at all, so there is nothing to substitute"
        );
        // A failed forward holds no port, so it has no substitution to speak. The forward that
        // still listens is the one that must say it out loud.
        running.phase = ForwardPhase::Running;
        let label = row_aria_label(&running);
        assert!(
            label.contains("already in use"),
            "the substitution is spoken, not only hovered: {label}"
        );
    }

    #[test]
    fn the_filter_matches_the_target_namespace_address_and_failure() {
        let running = snapshot(ForwardPhase::Running, Some(8081), None);
        assert!(
            matches_filter(&running, ""),
            "an empty filter keeps every row"
        );
        assert!(matches_filter(&running, "POD-A"), "the target matches");
        assert!(matches_filter(&running, "8081"), "the address matches");
        assert!(matches_filter(&running, "localhost"), "the address matches");
        assert!(matches_filter(&running, "default"), "the namespace matches");
        assert!(matches_filter(&running, "running"), "the state matches");
        assert!(
            !matches_filter(&running, "kube-dns"),
            "an absent value does not match"
        );
        let failed = snapshot(ForwardPhase::Failed, None, Some("connection refused"));
        assert!(
            matches_filter(&failed, "refused"),
            "the failure reason is searchable, so a broken forward can be found"
        );
    }

    #[test]
    fn rows_are_grouped_by_namespace_with_one_caption_per_group() {
        let forwards = vec![
            snapshot_for(
                1,
                "default",
                "pod-a",
                8080,
                ForwardPhase::Running,
                Some(1),
                None,
            ),
            snapshot_for(
                2,
                "default",
                "pod-b",
                9090,
                ForwardPhase::Running,
                Some(2),
                None,
            ),
            snapshot_for(
                3,
                "kube-system",
                "dns",
                53,
                ForwardPhase::Running,
                Some(3),
                None,
            ),
        ];
        assert_eq!(
            build_rows(&forwards),
            vec![
                Row::Group {
                    namespace: "default".into(),
                    count: 2
                },
                Row::Forward(ForwardId(1)),
                Row::Forward(ForwardId(2)),
                Row::Group {
                    namespace: "kube-system".into(),
                    count: 1
                },
                Row::Forward(ForwardId(3)),
            ]
        );
        assert!(build_rows(&[]).is_empty());
    }

    #[test]
    fn a_forward_without_a_namespace_is_grouped_under_the_kubernetes_default() {
        let forwards = vec![snapshot_for(
            1,
            "  ",
            "pod-a",
            8080,
            ForwardPhase::Running,
            Some(1),
            None,
        )];
        assert_eq!(
            build_rows(&forwards),
            vec![
                Row::Group {
                    namespace: "default".into(),
                    count: 1
                },
                Row::Forward(ForwardId(1)),
            ]
        );
    }

    #[test]
    fn up_and_down_step_over_the_group_caption() {
        let forwards = vec![
            snapshot_for(
                1,
                "default",
                "pod-a",
                8080,
                ForwardPhase::Running,
                Some(1),
                None,
            ),
            snapshot_for(
                2,
                "default",
                "pod-b",
                9090,
                ForwardPhase::Running,
                Some(2),
                None,
            ),
            snapshot_for(
                3,
                "kube-system",
                "dns",
                53,
                ForwardPhase::Running,
                Some(3),
                None,
            ),
        ];
        let rows = build_rows(&forwards);
        assert_eq!(row_of_forward(&rows, ForwardId(1)), Some(1));
        assert_eq!(row_of_forward(&rows, ForwardId(3)), Some(4));
        assert_eq!(
            step_forward_row(&rows, None, 1),
            Some(1),
            "Down starts on the first row"
        );
        assert_eq!(
            step_forward_row(&rows, None, -1),
            Some(4),
            "Up starts on the last row"
        );
        assert_eq!(
            step_forward_row(&rows, Some(4), 1),
            None,
            "the last forward has nowhere to go"
        );
        assert_eq!(
            step_forward_row(&rows, Some(1), -1),
            None,
            "the first forward has nowhere to go"
        );
        assert_eq!(
            step_forward_row(&rows, Some(1), 1),
            Some(2),
            "Down crosses the group boundary without landing on the caption"
        );
        assert_eq!(step_forward_row(&rows, Some(2), 1), Some(4));
    }

    #[test]
    fn sorting_keeps_the_namespace_frame_and_orders_the_rows_inside_it() {
        let forwards = vec![
            snapshot_for(
                1,
                "kube-system",
                "dns",
                53,
                ForwardPhase::Failed,
                None,
                None,
            ),
            snapshot_for(
                2,
                "default",
                "pod-b",
                9090,
                ForwardPhase::Running,
                Some(20_000),
                None,
            ),
            snapshot_for(
                3,
                "default",
                "pod-a",
                8080,
                ForwardPhase::Running,
                Some(30_000),
                None,
            ),
        ];
        let by_target = Sort::default();
        assert_eq!(by_target.column, Column::Target);
        let mut sorted = forwards.clone();
        sorted.sort_by(|left, right| compare_forwards(left, right, by_target));
        assert_eq!(
            sorted.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            vec![3, 2, 1],
            "namespaces frame the list, then the target orders each group"
        );

        let by_url = by_target.toggled_to(Column::Url);
        assert_eq!(
            by_url,
            Sort {
                column: Column::Url,
                direction: Direction::Ascending
            }
        );
        let mut by_port = forwards.clone();
        by_port.sort_by(|left, right| compare_forwards(left, right, by_url));
        assert_eq!(
            by_port.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            vec![2, 3, 1],
            "a running forward with a low port leads its own group"
        );
        let reversed = by_url.toggled_to(Column::Url);
        assert_eq!(reversed.direction, Direction::Descending);
        let mut high_first = forwards.clone();
        high_first.sort_by(|left, right| compare_forwards(left, right, reversed));
        assert_eq!(
            high_first.iter().map(|s| s.id.0).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_eq!(
            by_target.toggled_to(Column::State),
            Sort {
                column: Column::State,
                direction: Direction::Ascending
            },
            "another column starts ascending"
        );
    }

    #[test]
    fn a_running_forward_sorts_above_a_broken_one() {
        let forwards = vec![
            snapshot_for(
                1,
                "default",
                "pod-a",
                8080,
                ForwardPhase::Failed,
                None,
                None,
            ),
            snapshot_for(
                2,
                "default",
                "pod-b",
                9090,
                ForwardPhase::Running,
                Some(8081),
                None,
            ),
            snapshot_for(
                3,
                "default",
                "pod-c",
                7070,
                ForwardPhase::Stopped,
                None,
                None,
            ),
        ];
        let mut sorted = forwards.clone();
        sorted.sort_by(|left, right| {
            compare_forwards(left, right, Sort::default().toggled_to(Column::State))
        });
        assert_eq!(
            sorted.iter().map(|s| s.phase).collect::<Vec<_>>(),
            vec![
                ForwardPhase::Running,
                ForwardPhase::Failed,
                ForwardPhase::Stopped
            ]
        );
    }

    fn pod_object(json: serde_json::Value) -> DynamicObject {
        serde_json::from_value(json).expect("pod object")
    }

    #[test]
    fn the_container_ports_of_a_pod_are_offered_as_choices() {
        let object = pod_object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "web-0", "namespace": "default" },
            "spec": {
                "containers": [
                    {
                        "name": "app",
                        "ports": [
                            { "containerPort": 8080, "protocol": "TCP" },
                            { "containerPort": 8080, "protocol": "TCP" },
                            { "containerPort": 53, "protocol": "UDP" },
                            { "name": "metrics", "containerPort": 9100 }
                        ]
                    },
                    { "name": "sidecar", "ports": [{ "containerPort": 15000, "protocol": "TCP" }] }
                ]
            }
        }));
        let ports = container_ports(&object);
        assert_eq!(
            ports,
            vec![
                ContainerPort {
                    port: 8080,
                    container: Some("app".into())
                },
                ContainerPort {
                    port: 9100,
                    container: Some("app".into())
                },
                ContainerPort {
                    port: 15_000,
                    container: Some("sidecar".into())
                },
            ],
            "duplicates collapse, a UDP port is skipped, and the container names the port"
        );
        assert_eq!(ports[0].label(), "8080 (app)");
    }

    #[test]
    fn a_pod_without_declared_ports_leaves_the_field_free_text() {
        let object = pod_object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "web-0" },
            "spec": { "containers": [{ "name": "app" }] }
        }));
        assert!(
            container_ports(&object).is_empty(),
            "no declared port means the dialog keeps its free-text field"
        );
        assert!(
            container_ports(&pod_object(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": { "name": "web-0" }
            })))
            .is_empty(),
            "an object without a spec offers nothing"
        );
    }

    #[test]
    fn row_aria_reports_the_address_and_the_failure_reason() {
        let running = row_aria_label(&snapshot(ForwardPhase::Running, Some(8081), None));
        assert_eq!(
            running,
            "Port forward for pod-a:8080:8080. Running. http://localhost:8081."
        );

        let failed = row_aria_label(&snapshot(
            ForwardPhase::Failed,
            Some(8081),
            Some("Port forward ended unexpectedly."),
        ));
        assert_eq!(
            failed,
            "Port forward for pod-a:8080:8080. Failed. \
Port forward ended unexpectedly."
        );
        assert!(
            !failed.contains("localhost"),
            "a failed forward must not announce an address: {failed}"
        );
        assert_eq!(
            row_aria_label(&snapshot(ForwardPhase::Failed, None, Some("   "))),
            "Port forward for pod-a:8080:8080. Failed.",
            "a blank error adds no spoken text"
        );
    }

    #[test]
    fn stop_all_counts_only_forwards_it_can_end() {
        let snapshots = [
            snapshot(ForwardPhase::Running, Some(1), None),
            snapshot(ForwardPhase::Starting, None, None),
            snapshot(ForwardPhase::Stopping, Some(2), None),
            snapshot(ForwardPhase::Stopped, None, None),
            snapshot(ForwardPhase::Failed, None, Some("boom")),
        ];
        assert_eq!(stoppable_count(&snapshots), 2);
        assert!(is_stoppable(ForwardPhase::Running));
        assert!(is_stoppable(ForwardPhase::Starting));
        assert!(!is_stoppable(ForwardPhase::Stopping));
        assert!(!is_stoppable(ForwardPhase::Failed));
        assert!(!is_stoppable(ForwardPhase::Stopped));
        assert_eq!(
            stoppable_count(&[snapshot(ForwardPhase::Stopping, Some(1), None)]),
            0,
            "a forward that is already stopping leaves Stop All with nothing to do"
        );
    }

    #[gpui::test]
    fn keyboard_selection_keeps_forward_ids(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            dock.update(cx, |dock, cx| {
                dock.set_terminal_services(Some(services), cx)
            });
            dock
        });
        let (harness, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| ForwardsView::new(dock.clone(), None, cx));
            let focus = view.read(cx).focus_handle();
            window.focus(&focus, cx);
            ForwardsHarness { view }
        });
        cx.run_until_parked();
        let view = harness.read_with(cx, |harness, _| harness.view.clone());
        assert!(cx.debug_bounds("forwards-empty").is_some());

        let ids = dock.update(cx, |dock, cx| {
            vec![
                dock.create_forward(request("default", "pod-a", 8080, None), cx)
                    .expect("first forward"),
                dock.create_forward(request("default", "pod-b", 9090, None), cx)
                    .expect("second forward"),
            ]
        });
        cx.run_until_parked();
        // Grouping puts the namespace caption first, so the first forward is the second row.
        assert!(cx.debug_bounds("forwards-group-0").is_some());
        let row = cx.debug_bounds("forwards-row-1").expect("forward row");
        assert!((f32::from(row.size.height) - 28.).abs() <= 1.);

        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[0]));
        cx.simulate_keystrokes("end");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[1]));
        cx.simulate_keystrokes("home");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[0]));
    }

    #[gpui::test]
    fn a_running_forward_is_one_keystroke_from_the_clipboard_and_the_browser(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(|cx| DockPanel::new(cx));
            dock.update(cx, |dock, cx| {
                dock.set_terminal_services(Some(services), cx)
            });
            dock
        });
        // The window view borrows the app, so the platform state is read once that scope ends.
        {
            let (harness, cx) = cx.add_window_view(|window, cx| {
                let view = cx.new(|cx| ForwardsView::new(dock.clone(), None, cx));
                let focus = view.read(cx).focus_handle();
                window.focus(&focus, cx);
                ForwardsHarness { view }
            });
            cx.run_until_parked();
            let view = harness.read_with(cx, |harness, _| harness.view.clone());
            let id = dock.update(cx, |dock, cx| {
                dock.create_forward(request("default", "pod-a", 8080, None), cx)
                    .expect("forward")
            });
            state
                .borrow_mut()
                .bindings
                .pop()
                .expect("binding channel")
                .send(Ok(34_567))
                .expect("send the bound port");
            cx.run_until_parked();

            let focus = view.read_with(cx, |view, _| view.focus_handle());
            cx.update(|window, cx| window.focus(&focus, cx));
            cx.simulate_keystrokes("ctrl-c");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("http://localhost:34567".to_owned()),
                "Control C puts the whole address on the clipboard"
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.selected),
                Some(id),
                "the copied row becomes the selection, so the next copy repeats it"
            );
            assert!(
                cx.debug_bounds("forwards-feedback").is_some(),
                "a copy with no visible result is confirmed in the toolbar"
            );
            cx.simulate_keystrokes("ctrl-o");
        }
        assert_eq!(
            cx.opened_url(),
            Some("http://localhost:34567".to_owned()),
            "Control O hands the address to the platform browser"
        );
    }

    #[gpui::test]
    fn a_forward_with_no_listener_offers_no_address_to_copy_or_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_view, dock, _state, cx) = forwards_harness(cx);
        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, None), cx)
                .expect("forward")
        });
        // A stopped forward keeps no listener, so it has no address to hand over.
        dock.update(cx, |dock, cx| dock.stop_forward(id, cx));
        cx.run_until_parked();
        let copy = cx
            .debug_bounds(leaked(format!("forwards-copy-url-{:?}", id)))
            .expect("copy control");
        assert!(
            f32::from(copy.size.width) > 0.,
            "the control stays visible so the row does not change shape between phases"
        );
        cx.simulate_click(copy.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            None,
            "a forward with no listener puts nothing on the clipboard"
        );
    }

    #[gpui::test]
    fn the_filter_narrows_the_list_and_reports_how_much_is_left(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(600.)));
        for (name, port) in [("pod-a", 8080u16), ("pod-b", 9090)] {
            dock.update(cx, |dock, cx| {
                dock.create_forward(request("default", name, port, None), cx)
                    .expect("forward")
            });
        }
        for _ in 0..2 {
            state
                .borrow_mut()
                .bindings
                .pop()
                .expect("binding channel")
                .send(Ok(34_567))
                .expect("send the bound port");
        }
        cx.run_until_parked();
        assert!(cx.debug_bounds("forwards-row-1").is_some());
        assert!(cx.debug_bounds("forwards-row-2").is_some());
        assert!(
            cx.debug_bounds("forwards-filter-status").is_none(),
            "an unfiltered list has nothing to report"
        );

        // `/` hands the keyboard to the filter, so the field is reachable without a pointer.
        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("/");
        cx.simulate_input("pod-b");
        cx.run_until_parked();
        // `read_with` cannot hand back a borrow, so the filter text is copied out to be compared.
        assert_eq!(view.read_with(cx, |view, _| view.filter.clone()), "pod-b");
        assert!(
            cx.debug_bounds("forwards-row-1").is_some(),
            "the matching forward keeps the first row after its caption"
        );
        assert!(
            cx.debug_bounds("forwards-row-2").is_none(),
            "a forward that does not match is not drawn"
        );
        assert!(
            cx.debug_bounds("forwards-filter-status").is_some(),
            "the user is told how much of the list the filter left"
        );

        view.update(cx, |view, cx| {
            view.set_filter("nothing matches this".to_owned(), cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("forwards-empty").is_some());
    }

    #[gpui::test]
    fn clicking_a_column_heading_sorts_and_says_which_way(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        let (view, dock, _state, cx) = forwards_harness(cx);
        for (name, port) in [("pod-a", 8080u16), ("pod-b", 9090)] {
            dock.update(cx, |dock, cx| {
                dock.create_forward(request("default", name, port, None), cx)
                    .expect("forward")
            });
        }
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.sort),
            Sort::default(),
            "the target is the default order"
        );
        let heading = cx.debug_bounds("forwards-header-URL").expect("URL heading");
        assert!(
            f32::from(heading.size.width) > 0.,
            "the heading is laid out"
        );
        cx.simulate_click(heading.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.sort),
            Sort {
                column: Column::Url,
                direction: Direction::Ascending
            }
        );
        cx.simulate_click(heading.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.sort),
            Sort {
                column: Column::Url,
                direction: Direction::Descending
            },
            "a second click reverses the sorted column"
        );
    }

    #[gpui::test]
    fn a_requested_local_port_is_reported_and_asked_for_again_on_retry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(600.)));
        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, Some(8081)), cx)
                .expect("forward")
        });
        assert_eq!(
            state.borrow().requests[0].local_port,
            Some(8081),
            "the port the user asked for reaches the forward"
        );
        // The requested port was taken, so the forward landed on a free one.
        state
            .borrow_mut()
            .bindings
            .pop()
            .expect("binding channel")
            .send(Ok(34_567))
            .expect("send the bound port");
        cx.run_until_parked();
        let snapshot = dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].clone());
        assert_eq!(snapshot.local_port, Some(34_567));
        let label = row_aria_label(&snapshot);
        assert!(
            label.contains("Local port 8081 was already in use"),
            "the substitution is spoken: {label}"
        );

        state
            .borrow_mut()
            .errors
            .pop()
            .expect("error channel")
            .send("connection refused by the pod".to_owned())
            .expect("send the failure");
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.activate(RowAction::Retry(id), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            state.borrow().requests[1].local_port,
            Some(8081),
            "a retry asks for the same port again, so the address does not move on every restart"
        );
    }

    #[gpui::test]
    fn stop_all_only_appears_while_a_forward_can_still_be_stopped(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_view, dock, _state, cx) = forwards_harness(cx);
        assert!(
            cx.debug_bounds("forwards-stop-all").is_none(),
            "an empty list has nothing for Stop All to stop"
        );

        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, None), cx)
                .expect("forward")
        });
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Starting
        );
        assert!(
            cx.debug_bounds("forwards-stop-all").is_some(),
            "a starting forward can still be stopped"
        );

        dock.update(cx, |dock, cx| dock.stop_forward(id, cx));
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Stopping
        );
        assert!(
            cx.debug_bounds("forwards-stop-all").is_none(),
            "a forward that is already stopping leaves Stop All with nothing to do"
        );
    }

    #[gpui::test]
    fn a_failed_forward_drops_its_port_and_explains_itself(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        let (_view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(600.)));
        cx.run_until_parked();

        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, None), cx)
                .expect("forward")
        });
        // The forward binds a port, so the row can only be judged after a real bind.
        state
            .borrow_mut()
            .bindings
            .pop()
            .expect("binding channel")
            .send(Ok(34_567))
            .expect("send the bound port");
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Running
        );
        assert!(
            cx.debug_bounds("forwards-stop-all").is_some(),
            "a running forward can be stopped"
        );

        state
            .borrow_mut()
            .errors
            .pop()
            .expect("error channel")
            .send("connection refused by the pod".to_owned())
            .expect("send the failure");
        cx.run_until_parked();

        let snapshot = dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].clone());
        assert_eq!(snapshot.phase, ForwardPhase::Failed);
        assert_eq!(snapshot.local_port, None, "the listener is gone");
        let label = row_aria_label(&snapshot);
        assert!(
            !label.contains("Local port"),
            "no port is announced: {label}"
        );
        assert!(
            label.contains("connection refused by the pod"),
            "the reason is spoken, not only hovered: {label}"
        );
        assert_eq!(row_action(&snapshot), RowAction::Retry(id));
        assert_eq!(
            stoppable_count(std::slice::from_ref(&snapshot)),
            0,
            "a failed forward cannot be stopped"
        );
        assert!(
            cx.debug_bounds("forwards-stop-all").is_none(),
            "Stop All disappears once nothing can be stopped"
        );
        // The name carries the id the dock issued, so the assertion follows the forward under
        // test instead of a constant the counter never produces.
        let retry = cx
            .debug_bounds(leaked(format!("{}-{:?}", RowAction::Retry(id).name(), id)))
            .expect("retry");
        assert!(f32::from(retry.size.width) > 0.);
        // The control is live, not only laid out: a click runs the retry.
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Starting,
            "clicking Retry starts a new attempt"
        );
    }
}
