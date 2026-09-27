//! Renders resource tables, toolbar controls, states, and row actions.

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::{
    AnyElement, AnyView, App, AppContext as _, ClickEvent, ClipboardItem, Context, DismissEvent,
    Entity, FocusHandle, Focusable, Font, FontFeatures, FontWeight, Hsla, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, ParentElement, Pixels, Point, Render, Role, ScrollStrategy,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, TextRun, WeakEntity,
    Window, div, px,
};
use k8s_core::machines::TableEvent;
use k8s_core::projection::{CellValue, Filter, IndexSnapshot, Row, Sort};
use kube_core::DynamicObject;
use serde_json::Value;
use ui::prelude::*;
use ui::{
    ColumnWidthConfig, ContextMenu, ContextMenuEntry, ResizableColumnsState, ScrollAxes,
    Scrollbars, Table, TableInteractionState, TableResizeBehavior, TintColor, Tooltip,
    right_click_menu,
};

use super::actions::{
    ClearFilter, DeleteSelection, FocusFilter, OpenDetails, OpenRowActions, Refresh, SelectNext,
    SelectNextColumn, SelectPrevious, SelectPreviousColumn, SortSelectedColumn, ToggleChurn,
    ToggleUpdates,
};
use super::columns::{ResourceColumn, columns_for, is_known_kind};
use super::host::{CachedRows, PendingOp, SourceFactory, TableHost, TableStatus};
use super::source::{ChurnHandle, ObjectOps};
use crate::design::{self, Severity};
use crate::session::{
    InspectorBinding, InspectorBindingInput, InspectorSelection, InspectorUpdate, LogRequest,
    ObjectRef, OpsFuture, ResourceSpec, ServiceAccountTarget, TextInput,
    pod_service_account_target,
};
use crate::settings::DataTypography;
use crate::shell::OpenNamespaceSwitcher;

const FILTER_DEBOUNCE: Duration = Duration::from_millis(300);
/// Debounces column-width writes after a resize.
const WIDTH_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// Sets the display time for non-error notices.
const NOTICE_DURATION: Duration = Duration::from_secs(8);
/// Sorts by Name by default.
const DEFAULT_SORT_COLUMN: usize = 0;
const COLUMN_SETTINGS_ERROR: &str =
    "Column settings did not save. Check settings.json and try again.";
const RETRY_LIVE_UPDATES: &str = "Retry Live Updates";
const RETRY_LOADING_RESOURCES: &str = "Retry Loading Resources";
/// Reloads a table that has no rows to show.
const REFRESH_RESOURCES: &str = "Refresh Resources";
const RETRY_LIVE_UPDATES_GUIDANCE: &str =
    "Retry live updates. If the problem continues, check the cluster connection.";
const RETRY_LOADING_RESOURCES_GUIDANCE: &str =
    "Retry loading resources. If the problem continues, check the cluster connection.";
const START_PORT_FORWARD: &str = "Start Port Forward";
/// Estimates the average glyph width as a fraction of the font size.
const CHAR_WIDTH_RATIO: f32 = 0.6;
/// Shortest usable width for a data column.
const COLUMN_MIN_WIDTH: f32 = 48.0;
/// Longest width a column can reach from the menu.
const MAX_COLUMN_WIDTH: f32 = 640.0;
/// Step used by the Wider and Narrower column entries.
const COLUMN_WIDTH_STEP: f32 = 24.0;
/// Caps the visible failure reason to one line.
const USER_REASON_MAX: usize = 96;
/// Reason fragments that mean the cluster connection failed.
const CONNECTION_FAILURES: [&str; 9] = [
    "connect",
    "refused",
    "dns",
    "timeout",
    "timed out",
    "unreachable",
    "no route",
    "certificate",
    "tls",
];
/// Explains a kind the app has no columns for.
const UNKNOWN_KIND_TITLE: &str = "No columns for this kind";
const UNKNOWN_KIND_GUIDANCE: &str = "The CRD is not installed or not listable in this cluster.";

/// Names the filter the `NoProblems` empty state is actually showing, and the
/// one action that clears it.
///
/// The state used to say "Clear the filter" for a filter the reader never typed.
/// The filter that is on is the status one, so the copy names that.
const NO_PROBLEMS_ACTION: &str = "Show all rows";
const NO_PROBLEMS_GUIDANCE: &str = "Turn off the status filter to see every row.";
/// Defines the key context for table actions.
const TABLE_CONTEXT: &str = "Table";
const TABLE_ACCESSIBILITY_DESCRIPTION: &str = "Use Up and Down to move between rows. Use Tab to select a data column or Actions. Use Ctrl+Tab to leave the table. Use Shift+Enter to sort the selected column. Press F10, Shift+F10, or Menu to open Row Actions. Press Enter to open the details for the selected row. Scroll horizontally to view more columns.";
const TABLE_NAV_CONTEXT: &str = TABLE_CONTEXT;
/// Names the shortcut that opens the row menu for the focused row.
const ROW_ACTIONS_KEYS: &str = "F10, Shift+F10, or Menu";
/// Lists the row-menu keys in the `aria-keyshortcuts` form.
const ROW_ACTIONS_KEYSHORTCUTS: &str = "Enter F10 Shift+F10 Menu";
/// Explains what the row's trailing marker means, next to the health glyph that
/// leads the status cell.
const CONFIDENCE_MARKER_DESCRIPTION: &str = "Observation confidence. Health says how the resource is doing; this says whether the cluster answered at all.";
/// Lists the column-header keys, including the menu that the header opens.
const COLUMN_KEYS: &str = "Enter Space Shift+Enter F10 Shift+F10 Menu";
/// Trailing slot for one medium icon button plus the cell padding on both
/// sides. The observation confidence marker takes the rest of the cell, and a
/// test holds the two together.
/// Gutter between the table's leading edge and the selection rail.
///
/// The rail is absolutely positioned at the row's own leading edge, so without
/// this it butts straight into the sidebar divider and reads as a border of the
/// window rather than a mark on the row. The first cell carries the matching
/// padding so the rail never sits under a value, and the header's first cell
/// carries the same padding so the columns stay aligned with the body.
const RAIL_GUTTER: Pixels = design::space::SM;

/// Slack the horizontal scroll range gets before the edge stops signalling.
///
/// The fade is the only thing that says a column is still clipped, so the range
/// has to be treated as finished only once the last column is fully in view.
/// This absorbs the rounding in the layout arithmetic that produced the range,
/// which is why the fade does not flicker on the last frame of a scroll. A half
/// pixel of slack would instead swallow a half-pixel remainder and the clipped
/// cell would read as the end of the data again.
const SCROLL_RANGE_SLACK: f32 = 0.05;

/// Leading pad the first cell carries so its text clears the widest rail.
fn first_cell_leading_pad() -> Pixels {
    RAIL_GUTTER + design::border::TABLE_FOCUS_RAIL + design::space::XS
}
/// Keeps a tooltip from growing past a comfortable reading measure.
const TOOLTIP_MAX_WIDTH: Pixels = px(520.0);
/// Caps the tooltip height so a long value stays scrollable.
const TOOLTIP_MAX_HEIGHT: Pixels = px(240.0);
/// Shapes a value only when its width estimate is within this factor of the
/// cell width, which is the only range where the estimate can be wrong.
const TOOLTIP_SHAPE_FACTOR: f32 = 2.0;
/// Caps how much of one value is shaped for measurement.
const TOOLTIP_SHAPE_MAX_CHARS: usize = 512;
const COMPACT_WIDTH: Pixels = px(1200.0);
/// Opens a row's detail view. Distinct from a click: a click only selects, so
/// this is the one path a pointer, Enter, and the row menu all share.
type RowActivated = Rc<dyn Fn(Row, &mut Window, &mut App)>;
type SelectionChanged = Rc<dyn Fn(Option<Row>, &mut App)>;
/// Opens the Dock and starts a log stream.
type LogsRequested = Rc<dyn Fn(&LogRequest, &mut Window, &mut App)>;
/// Requests confirmation for a delete operation.
type DeleteRequested = Rc<dyn Fn(DeleteTarget, &mut Window, &mut App)>;
type ScaleRequested = Rc<dyn Fn(ScaleTarget, &mut Window, &mut App)>;
/// Opens a shell for a selected container.
type ExecRequested = Rc<dyn Fn(ExecTarget, &mut Window, &mut App)>;
/// Opens the port-forward dialog.
type ForwardRequested = Rc<dyn Fn(PortForwardTarget, &mut Window, &mut App)>;
type ServiceAccountRequested = Rc<dyn Fn(ServiceAccountTarget, &mut Window, &mut App)>;

/// Stores the target for delete confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteTarget {
    pub object: ObjectRef,
}

/// Stores the objects a multi-delete confirmation covers.
#[derive(Clone, Debug)]
struct MultiDeleteRequest {
    /// Objects in the order the table shows them.
    objects: Vec<ObjectRef>,
    /// Names shown in the confirmation, capped for one line.
    label: String,
}

/// Stores a Pod and its container names for Exec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecTarget {
    pub namespace: Option<String>,
    pub name: SharedString,
    pub containers: Vec<SharedString>,
}

/// Stores the Pod and namespace for Port Forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardTarget {
    pub namespace: Option<String>,
    pub name: SharedString,
    /// The `containerPort` values the Pod declares, so the dialog can offer them as choices.
    pub ports: Vec<crate::panels::forwards::ContainerPort>,
}

/// Stores the target and replica count for Scale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaleTarget {
    pub object: ObjectRef,
    pub replicas: i32,
}

/// Lists resource kinds that support Scale.
const SCALABLE_KINDS: [&str; 4] = [
    "Deployment",
    "StatefulSet",
    "ReplicaSet",
    "ReplicationController",
];

impl ResourceSpec {
    fn label_lower(&self) -> String {
        self.label.to_lowercase()
    }
}

/// Stores a non-blocking table notice.
#[derive(Clone, Debug)]
struct Notice {
    message: SharedString,
    detail: Option<SharedString>,
    severity: Severity,
    epoch: u64,
}

/// Defines keyboard navigation moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusTarget {
    Row,
    Column,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortAffordance {
    Unsorted,
    Ascending,
    Descending,
    Relevance,
}

/// Identifies the row a menu acts on. A UID survives snapshot rebuilds, so a
/// rebuild that reorders rows cannot move an action onto a neighbour.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowTarget {
    Position(usize),
    Uid(SharedString),
}

impl RowTarget {
    fn for_row(row: &Row) -> Option<Self> {
        row.obj
            .metadata
            .uid
            .as_deref()
            .map(|uid| Self::Uid(SharedString::from(uid)))
    }

    /// Returns the current row index for this target.
    fn index(&self, snapshot: &IndexSnapshot) -> Option<usize> {
        match self {
            Self::Position(index) => (*index < snapshot.rows.len()).then_some(*index),
            Self::Uid(uid) => snapshot.by_uid.get(uid.as_ref()).copied(),
        }
    }
}

pub struct PodsView {
    spec: ResourceSpec,
    host: Entity<TableHost>,
    /// Stores all columns used for values and sorting.
    columns: Arc<Vec<ResourceColumn>>,
    /// Stores visible column indexes in display order.
    visible_columns: Vec<usize>,
    /// Stores hidden column IDs.
    hidden_columns: HashSet<String>,
    columns_state: Entity<ResizableColumnsState>,
    /// Stores the last saved width for each column.
    saved_widths: BTreeMap<String, f32>,
    widths_epoch: u64,
    widths_task: Option<Task<()>>,
    interaction: Entity<TableInteractionState>,
    filter: Entity<TextInput>,
    filter_epoch: Rc<Cell<u64>>,
    filter_task: Option<Task<()>>,
    /// Indicates that a debounced filter update is pending.
    filter_pending: bool,
    /// Keeps the selected row across snapshot rebuilds.
    selected_uid: Option<SharedString>,
    /// Keeps every selected row. It always contains `selected_uid`.
    selected_uids: Arc<HashSet<SharedString>>,
    /// Marks where a Shift range selection starts.
    selection_anchor: Option<SharedString>,
    /// Stores the keyboard- or pointer-selected column.
    selected_column: usize,
    filter_active: bool,
    sort_before_filter: Option<Sort>,
    relevance_sort: bool,
    /// Hides rows whose status is healthy.
    problems_only: bool,
    focus_target: FocusTarget,
    /// Moves focus back to the table after a control that it repairs goes away.
    restore_table_focus: bool,
    pending_column_reveal: bool,
    context_menu: Option<Entity<ContextMenu>>,
    context_menu_previous_focus: Option<FocusHandle>,
    context_menu_position: Point<Pixels>,
    /// Sends the selected object to the Inspector.
    inspector: Option<InspectorBinding>,
    /// Avoids serializing the same object again.
    inspected_object: Option<Arc<DynamicObject>>,
    on_activate_row: Option<RowActivated>,
    on_selection_changed: Option<SelectionChanged>,
    on_logs: Option<LogsRequested>,
    on_delete_requested: Option<DeleteRequested>,
    on_scale_requested: Option<ScaleRequested>,
    on_exec_requested: Option<ExecRequested>,
    on_forward_requested: Option<ForwardRequested>,
    on_service_account_requested: Option<ServiceAccountRequested>,
    churn: Option<ChurnHandle>,
    /// Runs mutating resource operations.
    ops: Option<Rc<dyn ObjectOps>>,
    operation_tasks: HashMap<String, (u64, Task<()>)>,
    operation_epoch: u64,
    notice: Option<Notice>,
    notice_epoch: u64,
    notice_focus: FocusHandle,
    /// Focuses the Status header's problems filter, so the control that hides
    /// healthy rows is reachable without a pointer.
    problems_filter_focus: FocusHandle,
    /// Shows the high-latency notice once.
    latency_notified: bool,
    latency_auto_paused: bool,
    focus_handle: FocusHandle,
    /// Tracks keyboard focus for the Retry button.
    retry_focus: FocusHandle,
    /// Makes the failure reason reachable by keyboard and screen readers.
    error_reason_focus: FocusHandle,
    /// Holds the pending multi-object delete confirmation.
    multi_delete: Option<MultiDeleteRequest>,
    /// Tracks keyboard focus for the toolbar Retry button.
    stale_retry_focus: FocusHandle,
    empty_action_focus: FocusHandle,
    empty_namespace_focus: FocusHandle,
    /// Focuses the multi-delete confirm button.
    multi_delete_confirm_focus: FocusHandle,
    /// Focuses the multi-delete cancel button.
    multi_delete_cancel_focus: FocusHandle,
    _host_observation: Subscription,
}

impl PodsView {
    pub fn new(
        source_factory: SourceFactory,
        churn: Option<ChurnHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_inspector(source_factory, churn, None::<InspectorBinding>, cx)
    }

    /// Creates a table connected to an Inspector.
    pub fn new_with_inspector<I: InspectorBindingInput>(
        source_factory: SourceFactory,
        churn: Option<ChurnHandle>,
        inspector: I,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::for_resource(source_factory, churn, inspector, ResourceSpec::pods(), cx)
    }

    /// Creates a table for any resource kind.
    pub fn for_resource<I: InspectorBindingInput>(
        source_factory: SourceFactory,
        churn: Option<ChurnHandle>,
        inspector: I,
        spec: ResourceSpec,
        cx: &mut Context<Self>,
    ) -> Self {
        install_fallback_keys(cx);

        let columns = Arc::new(columns_for(&spec.kind, spec.namespaced));
        let host = cx.new(|cx| TableHost::new(columns.as_slice(), source_factory, cx));
        let host_observation = cx.observe(&host, |view, _host, cx| view.on_host_changed(cx));

        // Tests do not read or write user column settings.
        let (mut hidden_columns, saved_widths): (HashSet<String>, BTreeMap<String, f32>) =
            if cfg!(test) {
                (HashSet::new(), BTreeMap::new())
            } else {
                (
                    crate::settings::hidden_columns(&spec.kind)
                        .into_iter()
                        .collect(),
                    crate::settings::column_widths(&spec.kind)
                        .into_iter()
                        .collect(),
                )
            };
        let mut visible_columns: Vec<usize> = visible_indices(&columns, &hidden_columns);
        if visible_columns.is_empty() {
            let first = columns[0].column.id.clone();
            hidden_columns.remove(&first);
            visible_columns.push(0);
        }
        let default_sort_column = fallback_sort_column(&visible_columns);

        // Store the default sort before starting the source.
        host.update(cx, |host, cx| {
            host.dispatch(
                TableEvent::SortChanged {
                    sort: Some(Sort::ascending(default_sort_column)),
                },
                cx,
            );
            host.dispatch(TableEvent::Start, cx);
        });

        let typography = DataTypography::from_theme_settings(cx);
        let initial_widths: Vec<Pixels> = visible_columns
            .iter()
            .map(|&index| {
                let column = &columns[index];
                let width = saved_widths
                    .get(&column.column.id)
                    .copied()
                    .unwrap_or_else(|| column_default_width(column, &typography));
                px(width)
            })
            .collect();
        // Every column stays draggable. The last one used to opt out, which
        // left the one column that actually clips mid-glyph with no pointer way
        // out of it, and the shared component only insets that divider by 1px to
        // keep `overflow_hidden` from clipping it — a 1px rule does not need
        // resize turned off.
        let columns_state = cx.new(|_| {
            let count = initial_widths.len();
            let resize_behavior = vec![TableResizeBehavior::MinSize(COLUMN_MIN_WIDTH); count];
            ResizableColumnsState::new(count, initial_widths, resize_behavior)
        });
        // Make the table focus handle part of the Tab order.
        let interaction = cx.new(|cx| {
            let mut state = TableInteractionState::new(cx)
                .with_custom_scrollbar(Scrollbars::new(ScrollAxes::Vertical));
            state.focus_handle = state.focus_handle.clone().tab_stop(true).tab_index(0);
            state
        });

        let filter_epoch = Rc::new(Cell::new(0));
        let selected_column = default_sort_column;
        let weak_view = cx.weak_entity();
        let placeholder = format!("Filter {} by name…", spec.label_lower());
        let filter = cx.new(|cx| {
            TextInput::new(placeholder, cx, move |text, cx| {
                if let Some(view) = weak_view.upgrade() {
                    view.update(cx, |view, cx| view.on_filter_input(text.to_owned(), cx));
                }
            })
        });

        Self {
            spec,
            host,
            columns,
            visible_columns,
            hidden_columns,
            columns_state,
            saved_widths,
            widths_epoch: 0,
            widths_task: None,
            interaction,
            filter,
            filter_epoch,
            filter_task: None,
            filter_pending: false,
            selected_uid: None,
            selected_uids: Arc::new(HashSet::new()),
            selection_anchor: None,
            selected_column,
            filter_active: false,
            sort_before_filter: Some(Sort::ascending(default_sort_column)),
            relevance_sort: false,
            problems_only: false,
            focus_target: FocusTarget::Row,
            restore_table_focus: false,
            pending_column_reveal: false,
            context_menu: None,
            context_menu_previous_focus: None,
            context_menu_position: gpui::point(px(0.), px(0.)),
            inspector: inspector.into_binding(),
            inspected_object: None,
            on_activate_row: None,
            on_selection_changed: None,
            on_logs: None,
            on_delete_requested: None,
            on_scale_requested: None,
            on_exec_requested: None,
            on_forward_requested: None,
            on_service_account_requested: None,
            churn,
            ops: None,
            operation_tasks: HashMap::new(),
            operation_epoch: 0,
            notice: None,
            notice_epoch: 0,
            notice_focus: cx.focus_handle(),
            problems_filter_focus: cx.focus_handle().tab_stop(true).tab_index(4),
            latency_notified: false,
            latency_auto_paused: false,
            focus_handle: cx.focus_handle(),
            retry_focus: cx.focus_handle(),
            error_reason_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            multi_delete: None,
            stale_retry_focus: cx.focus_handle(),
            empty_action_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            empty_namespace_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            multi_delete_confirm_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            multi_delete_cancel_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            _host_observation: host_observation,
        }
    }

    // Column visibility and width persistence.

    /// Toggles one column and keeps at least one column visible.
    fn toggle_column_visibility(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(column) = self.columns.get(index) else {
            return;
        };
        let id = column.column.id.clone();
        if self.hidden_columns.contains(&id) {
            self.hidden_columns.remove(&id);
        } else {
            if self.visible_columns.len() <= 1 {
                self.notify(
                    "Open Columns to show a hidden column.",
                    Severity::Warning,
                    cx,
                );
                return;
            }
            self.hidden_columns.insert(id);
        }
        if !cfg!(test) {
            let hidden: Vec<String> = self
                .columns
                .iter()
                .filter(|column| self.hidden_columns.contains(&column.column.id))
                .map(|column| column.column.id.to_string())
                .collect();
            match crate::settings::set_hidden_columns(cx, self.spec.kind.as_ref(), hidden) {
                Ok(()) => self.clear_column_settings_error(cx),
                Err(_) => self.notify(COLUMN_SETTINGS_ERROR.to_owned(), Severity::Error, cx),
            }
        }
        self.rebuild_columns_state(cx);
        cx.notify();
    }

    /// Rebuilds column state while preserving known widths.
    fn rebuild_columns_state(&mut self, cx: &mut Context<Self>) {
        let current = self.current_widths(cx);
        self.visible_columns = visible_indices(&self.columns, &self.hidden_columns);
        if self.visible_columns.is_empty() {
            // Recover from an invalid empty visibility set.
            self.visible_columns.push(0);
            self.hidden_columns.remove(&self.columns[0].column.id);
        }
        let typography = DataTypography::from_theme_settings(cx);
        let widths: Vec<Pixels> = self
            .visible_columns
            .iter()
            .map(|&index| {
                let column = &self.columns[index];
                let width = current
                    .get(&column.column.id)
                    .copied()
                    .or_else(|| self.saved_widths.get(&column.column.id).copied())
                    .unwrap_or_else(|| column_default_width(column, &typography));
                px(width)
            })
            .collect();
        let fallback = self.default_sort_column();
        self.columns_state = cx.new(|_| {
            let count = widths.len();
            let resize_behavior = vec![TableResizeBehavior::MinSize(COLUMN_MIN_WIDTH); count];
            ResizableColumnsState::new(count, widths, resize_behavior)
        });
        if !self.visible_columns.contains(&self.selected_column) {
            self.selected_column = fallback;
        }
        if self
            .host
            .read(cx)
            .sort()
            .is_some_and(|sort| !self.visible_columns.contains(&sort.column))
        {
            // The sorted column is hidden, so the order moves to the default
            // column and the header keeps showing a real sort.
            self.apply_sort(Some(Sort::ascending(fallback)), cx);
        }
        self.pending_column_reveal = true;
    }

    /// Applies an explicit column sort. An explicit sort always leaves
    /// relevance mode, otherwise the ranked filter would hide the new order.
    fn apply_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        self.relevance_sort = false;
        if self.filter_active {
            self.sort_before_filter = sort;
        }
        self.host.update(cx, |host, cx| {
            host.set_relevance(false);
            host.dispatch(TableEvent::SortChanged { sort }, cx);
        });
    }

    /// Returns the column that provides the default row order. Sorting a third
    /// time returns here, so the state the user lands in is a real sort.
    fn default_sort_column(&self) -> usize {
        fallback_sort_column(&self.visible_columns)
    }

    /// Returns each visible column width in pixels.
    fn current_widths(&self, cx: &App) -> BTreeMap<String, f32> {
        let rem_size = px(16.0);
        let state = self.columns_state.read(cx);
        (0..self.visible_columns.len())
            .filter_map(|position| {
                let index = *self.visible_columns.get(position)?;
                let column = self.columns.get(index)?;
                let width = f32::from(
                    state.pinned_width(position + 1, rem_size)
                        - state.pinned_width(position, rem_size),
                );
                Some((column.column.id.clone(), width))
            })
            .collect()
    }

    fn reveal_selected_column(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(position) = self
            .visible_columns
            .iter()
            .position(|&index| index == self.selected_column)
        else {
            return;
        };
        let widths: Vec<f32> = self
            .visible_widths(window, cx)
            .into_iter()
            .map(f32::from)
            .collect();
        let Some(span) = column_span(&widths, position) else {
            return;
        };
        let (handle, viewport) = {
            let interaction = self.interaction.read(cx);
            (
                interaction.horizontal_scroll_handle.clone(),
                f32::from(interaction.horizontal_scroll_handle.bounds().size.width),
            )
        };
        if !viewport.is_finite() || viewport <= 0.0 {
            self.pending_column_reveal = true;
            return;
        }
        let content_width: f32 = widths.iter().sum();
        let current = -f32::from(handle.offset().x);
        let next = reveal_scroll_position(current, viewport, content_width, span);
        if !current.is_finite() || (next - current).abs() > 0.5 {
            handle.set_offset(gpui::point(px(-next), px(0.0)));
        }
        self.pending_column_reveal = false;
    }

    fn context_menu_anchor(
        &self,
        window: &Window,
        cx: &Context<Self>,
        row: Option<usize>,
    ) -> Point<Pixels> {
        let widths: Vec<f32> = self
            .visible_widths(window, cx)
            .into_iter()
            .map(f32::from)
            .collect();
        let position = self
            .visible_columns
            .iter()
            .position(|&index| index == self.selected_column)
            .unwrap_or(0);
        let column_start = column_span(&widths, position)
            .map(|span| span.0)
            .unwrap_or(0.0);
        let (horizontal_bounds, vertical_bounds, horizontal_offset, vertical_offset) = {
            let interaction = self.interaction.read(cx);
            let horizontal = interaction.horizontal_scroll_handle.bounds();
            let vertical = interaction.scroll_handle.0.borrow().base_handle.bounds();
            (
                horizontal,
                vertical,
                interaction.horizontal_scroll_handle.offset().x,
                interaction.scroll_handle.0.borrow().base_handle.offset().y,
            )
        };
        // Row geometry follows the data font size.
        let row_pixels = f32::from(row_height(cx));
        let fallback_x = f32::from(design::space::XL);
        let x = if horizontal_bounds.size.width > px(0.) {
            if row.is_some() {
                f32::from(horizontal_bounds.right()) - row_pixels
            } else {
                f32::from(horizontal_bounds.left()) + column_start - f32::from(horizontal_offset)
            }
        } else {
            fallback_x
        };
        let y = if vertical_bounds.size.height > px(0.) {
            let content_y = f32::from(vertical_bounds.top()) - f32::from(vertical_offset);
            match row {
                // The anchor follows the listed position, not the snapshot index.
                Some(index) => {
                    let position = self
                        .host
                        .read(cx)
                        .snapshot()
                        .map(|snapshot| self.shown_position(&snapshot, index))
                        .unwrap_or(index);
                    content_y + position as f32 * row_pixels + row_pixels
                }
                None => content_y - row_pixels,
            }
        } else {
            f32::from(design::size::TOOLBAR) + row_pixels
        };
        let window = window.viewport_size();
        gpui::point(
            px(x.clamp(f32::from(design::space::SM), f32::from(window.width))),
            px(y.clamp(f32::from(design::space::SM), f32::from(window.height))),
        )
    }

    /// Saves changed column widths after the resize debounce.
    fn note_column_widths(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        // Keep saved widths for hidden columns.
        let mut next = self.saved_widths.clone();
        for (id, width) in self.current_widths(cx) {
            next.insert(id, width);
        }
        if next == self.saved_widths {
            return;
        }
        self.pending_column_reveal = true;
        self.saved_widths = next.clone();
        let epoch = self.widths_epoch.wrapping_add(1);
        self.widths_epoch = epoch;
        let kind = self.spec.kind.to_string();
        self.widths_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(WIDTH_SAVE_DEBOUNCE).await;
            this.update(cx, |view, cx| {
                if view.widths_epoch != epoch {
                    return;
                }
                match crate::settings::set_column_widths(cx, &kind, next) {
                    Ok(()) => view.clear_column_settings_error(cx),
                    Err(_) => view.notify(COLUMN_SETTINGS_ERROR.to_owned(), Severity::Error, cx),
                }
            })
            .ok();
        }));
    }

    fn present_context_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_menu.is_some() {
            return;
        }
        let previous_focus = window.focused(cx);
        let focus = menu.read(cx).focus_handle(cx).clone();
        let view = cx.weak_entity();
        self.context_menu_previous_focus = previous_focus;
        self.context_menu_position = position;
        self.context_menu = Some(menu.clone());
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.close_context_menu(window, cx));
                }
            })
            .detach();
        window.on_next_frame(move |window, _cx| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu_focused = self
            .context_menu
            .as_ref()
            .is_some_and(|menu| menu.read(cx).focus_handle(cx).contains_focused(window, cx));
        if self.context_menu.take().is_none() {
            return;
        }
        let focus = self
            .context_menu_previous_focus
            .take()
            .unwrap_or_else(|| self.table_focus_handle(cx));
        if menu_focused || window.focused(cx).is_none() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn open_header_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.is_some() {
            return;
        }
        self.focus_target = FocusTarget::Column;
        self.pending_column_reveal = true;
        self.reveal_selected_column(window, cx);
        let position = self.context_menu_anchor(window, cx, None);
        let menu = self.column_context_menu(self.selected_column, window, cx);
        self.present_context_menu(menu, position, window, cx);
    }

    fn open_row_context_menu(
        &mut self,
        target: Option<RowTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_menu.is_some() {
            return;
        }
        let Some(index) = self.resolve_row_target(target, cx) else {
            return;
        };
        let previous_focus = window.focused(cx);
        let menu = self.row_context_menu(RowTarget::Position(index), window, cx);
        let position = self.context_menu_anchor(window, cx, Some(index));
        if let Some(previous_focus) = previous_focus {
            window.focus(&previous_focus, cx);
        }
        self.present_context_menu(menu, position, window, cx);
    }

    /// Resolves a row target to the current snapshot. Without a target the
    /// keyboard uses the selection, then the first row.
    fn resolve_row_target(&self, target: Option<RowTarget>, cx: &App) -> Option<usize> {
        let snapshot = self.host.read(cx).snapshot()?;
        let index = match target {
            Some(target) => return target.index(&snapshot),
            None => self.selected_index(&snapshot).unwrap_or(0),
        };
        (index < snapshot.rows.len()).then_some(index)
    }

    /// Opens the column visibility and width menu.
    fn column_context_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        self.focus_target = FocusTarget::Column;
        if self.columns.get(index).is_some() {
            self.selected_column = index;
            cx.notify();
        }
        let view = cx.weak_entity();
        let columns: Vec<(usize, SharedString, String)> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                (
                    index,
                    SharedString::from(column.title),
                    column.column.id.clone(),
                )
            })
            .collect();
        let hidden = self.hidden_columns.clone();
        // The menu opens on one column, and the width actions act on the
        // selected one. A menu that said only `Wider` left the reader guessing
        // which column it was about, which matters as soon as the list of columns
        // is on screen too.
        let column_title = self
            .columns
            .get(index)
            .map(|column| column.title.to_owned())
            .unwrap_or_default();
        // The Status header owns the filter; the menu keeps it keyboard reachable.
        let problems_filter = self.problems_filter_column().is_some();
        let problems_only = self.problems_only;
        ContextMenu::build(window, cx, move |menu, _, _| {
            // Width stays reachable without a pointer drag.
            let menu = menu
                .item(
                    ContextMenuEntry::new(format!("Wider {column_title}"))
                        .icon(IconName::ArrowRight)
                        .handler({
                            let view = view.clone();
                            move |_window, cx| {
                                view.update(cx, |view, cx| {
                                    view.resize_selected_column(COLUMN_WIDTH_STEP, cx)
                                })
                                .ok();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new(format!("Narrower {column_title}"))
                        .icon(IconName::ArrowLeft)
                        .handler({
                            let view = view.clone();
                            move |_window, cx| {
                                view.update(cx, |view, cx| {
                                    view.resize_selected_column(-COLUMN_WIDTH_STEP, cx)
                                })
                                .ok();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Reset Column Widths")
                        .icon(IconName::RotateCcw)
                        .handler({
                            let view = view.clone();
                            move |_window, cx| {
                                view.update(cx, |view, cx| {
                                    let typography = DataTypography::from_theme_settings(cx);
                                    let widths: Vec<Pixels> = view
                                        .visible_columns
                                        .iter()
                                        .map(|&index| {
                                            px(column_default_width(
                                                &view.columns[index],
                                                &typography,
                                            ))
                                        })
                                        .collect();
                                    view.columns_state = cx.new(|_| {
                                        let count = widths.len();
                                        let resize_behavior =
                                            vec![
                                                TableResizeBehavior::MinSize(COLUMN_MIN_WIDTH);
                                                count
                                            ];
                                        ResizableColumnsState::new(count, widths, resize_behavior)
                                    });
                                    view.saved_widths.clear();
                                    if !cfg!(test) {
                                        match crate::settings::set_column_widths(
                                            cx,
                                            view.spec.kind.as_ref(),
                                            BTreeMap::new(),
                                        ) {
                                            Ok(()) => view.clear_column_settings_error(cx),
                                            Err(_) => view.notify(
                                                COLUMN_SETTINGS_ERROR.to_owned(),
                                                Severity::Error,
                                                cx,
                                            ),
                                        }
                                    }
                                    cx.notify();
                                })
                                .ok();
                            }
                        }),
                )
                // Three unrelated groups in one flat list read as one list.
                .separator();
            let menu = if problems_filter {
                menu.toggleable_entry(
                    "Show Only Problems",
                    problems_only,
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |view, cx| view.toggle_problems_filter(cx))
                                .ok();
                        }
                    },
                )
            } else {
                menu
            };
            menu.separator().submenu("Columns", move |menu, _, _| {
                let mut menu = menu;
                for (index, title, id) in &columns {
                    let view = view.clone();
                    let index = *index;
                    let toggled = !hidden.contains(id);
                    menu = menu.toggleable_entry(
                        title.clone(),
                        toggled,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            view.update(cx, |view, cx| view.toggle_column_visibility(index, cx))
                                .ok();
                        },
                    );
                }
                menu
            })
        })
    }

    /// Grows or shrinks the selected data column by one step.
    fn resize_selected_column(&mut self, delta: f32, cx: &mut Context<Self>) {
        let index = self.selected_column;
        let Some(column) = self.columns.get(index) else {
            return;
        };
        let title = column.title;
        let id = column.column.id.clone();
        let default = column_default_width(column, &DataTypography::from_theme_settings(cx));
        let Some(position) = self
            .visible_columns
            .iter()
            .position(|&visible| visible == index)
        else {
            return;
        };
        let current = self.current_widths(cx).get(&id).copied().unwrap_or(default);
        let next = (current + delta).clamp(COLUMN_MIN_WIDTH, MAX_COLUMN_WIDTH);
        if (next - current).abs() < f32::EPSILON {
            self.notify(
                format!("{title} is already at its width limit."),
                Severity::Muted,
                cx,
            );
            return;
        }
        self.columns_state.update(cx, |state, _| {
            state.set_column_configuration(
                position,
                px(next),
                TableResizeBehavior::MinSize(COLUMN_MIN_WIDTH),
            );
        });
        // Keep the saved width in step with the menu.
        self.saved_widths.insert(id, next);
        self.pending_column_reveal = true;
        self.note_column_widths(cx);
        cx.notify();
    }

    /// Registers the action behind Enter, `Open Details`, and the row menu's
    /// `Open Details` entry. A click deliberately does not route here.
    pub fn on_activate_row(&mut self, handler: impl Fn(Row, &mut Window, &mut App) + 'static) {
        self.on_activate_row = Some(Rc::new(handler));
    }

    pub(crate) fn on_selection_changed(
        &mut self,
        handler: impl Fn(Option<Row>, &mut App) + 'static,
    ) {
        self.on_selection_changed = Some(Rc::new(handler));
    }

    /// Sets the handler for the Logs action.
    pub fn on_logs_requested(
        &mut self,
        handler: impl Fn(&LogRequest, &mut Window, &mut App) + 'static,
    ) {
        self.on_logs = Some(Rc::new(handler));
    }

    /// Sets the handler for delete confirmation.
    pub fn on_delete_requested(
        &mut self,
        handler: impl Fn(DeleteTarget, &mut Window, &mut App) + 'static,
    ) {
        self.on_delete_requested = Some(Rc::new(handler));
    }

    /// Sets the handler for the Scale dialog.
    pub fn on_scale_requested(
        &mut self,
        handler: impl Fn(ScaleTarget, &mut Window, &mut App) + 'static,
    ) {
        self.on_scale_requested = Some(Rc::new(handler));
    }

    /// Sets the handler for Exec.
    pub fn on_exec_requested(
        &mut self,
        handler: impl Fn(ExecTarget, &mut Window, &mut App) + 'static,
    ) {
        self.on_exec_requested = Some(Rc::new(handler));
    }

    /// Sets the handler for Port Forward.
    pub fn on_forward_requested(
        &mut self,
        handler: impl Fn(PortForwardTarget, &mut Window, &mut App) + 'static,
    ) {
        self.on_forward_requested = Some(Rc::new(handler));
    }

    pub fn on_service_account_requested(
        &mut self,
        handler: impl Fn(ServiceAccountTarget, &mut Window, &mut App) + 'static,
    ) {
        self.on_service_account_requested = Some(Rc::new(handler));
    }

    fn service_account_target_for_object(
        &self,
        object: &DynamicObject,
    ) -> Option<Result<ServiceAccountTarget, &'static str>> {
        (self.spec.kind.as_ref() == "Pod").then(|| pod_service_account_target(object))
    }

    pub fn service_account_target(
        &self,
        cx: &App,
    ) -> Option<Result<ServiceAccountTarget, &'static str>> {
        let object = self.selected_object(cx)?;
        self.service_account_target_for_object(&object)
    }

    fn exec_target_for_object(&self, object: &DynamicObject) -> Option<ExecTarget> {
        Some(ExecTarget {
            namespace: object.metadata.namespace.clone(),
            name: object.metadata.name.clone()?.into(),
            containers: pod_containers(object),
        })
    }

    /// Requests Exec for the selected Pod.
    pub fn request_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(handler) = self.on_exec_requested.clone() else {
            return;
        };
        if self.spec.kind.as_ref() != "Pod" {
            self.notify("Exec is available only for Pods.", Severity::Warning, cx);
            return;
        }
        let Some(object) = self.selected_object(cx) else {
            self.notify(
                "Select a pod before you choose Exec.",
                Severity::Warning,
                cx,
            );
            return;
        };
        let Some(target) = self.exec_target_for_object(&object) else {
            return;
        };
        handler(target, window, cx);
    }

    fn request_exec_target(
        &mut self,
        target: ExecTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.spec.kind.as_ref() != "Pod" {
            self.notify("Exec is available only for Pods.", Severity::Warning, cx);
            return;
        }
        if let Some(handler) = self.on_exec_requested.clone() {
            handler(target, window, cx);
        }
    }

    fn port_forward_target_for_object(&self, object: &DynamicObject) -> Option<PortForwardTarget> {
        Some(PortForwardTarget {
            namespace: object.metadata.namespace.clone(),
            name: object.metadata.name.clone()?.into(),
            ports: crate::panels::forwards::container_ports(object),
        })
    }

    /// Requests Port Forward for the selected Pod.
    pub fn request_port_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(handler) = self.on_forward_requested.clone() else {
            return;
        };
        if self.spec.kind.as_ref() != "Pod" {
            self.notify(
                "Start Port Forward is available only for Pods.",
                Severity::Warning,
                cx,
            );
            return;
        }
        let Some(object) = self.selected_object(cx) else {
            self.notify(
                "Select a Pod before you start a port forward.",
                Severity::Warning,
                cx,
            );
            return;
        };
        let Some(target) = self.port_forward_target_for_object(&object) else {
            return;
        };
        handler(target, window, cx);
    }

    fn request_port_forward_target(
        &mut self,
        target: PortForwardTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.spec.kind.as_ref() != "Pod" {
            self.notify(
                "Start Port Forward is available only for Pods.",
                Severity::Warning,
                cx,
            );
            return;
        }
        if let Some(handler) = self.on_forward_requested.clone() {
            handler(target, window, cx);
        }
    }

    /// Sets the cluster operation handler.
    pub fn set_ops(&mut self, ops: Option<Rc<dyn ObjectOps>>) {
        self.ops = ops;
    }

    #[cfg(test)]
    pub fn select_uid_for_test(&mut self, uid: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.select_only(Some(uid.into()));
        cx.notify();
    }

    /// Returns true when the resource kind supports Scale.
    pub fn supports_scale(&self) -> bool {
        SCALABLE_KINDS.contains(&self.spec.kind.as_ref())
    }

    fn scale_target_for_object(&self, object: &DynamicObject) -> Option<ScaleTarget> {
        if !self.supports_scale() {
            return None;
        }
        let replicas = object
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .clamp(0, i64::from(i32::MAX)) as i32;
        Some(ScaleTarget {
            object: self.object_ref(object)?,
            replicas,
        })
    }

    /// Returns the selected Scale target.
    pub fn scale_target(&self, cx: &App) -> Option<ScaleTarget> {
        let object = self.selected_object(cx)?;
        self.scale_target_for_object(&object)
    }

    fn delete_target(&self, cx: &App) -> Option<DeleteTarget> {
        Some(DeleteTarget {
            object: self.selection_ref(cx)?,
        })
    }

    pub fn request_delete_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Delete acts on the highlighted row, so the table needs no prior
        // selection and the Inspector follows the row that is affected.
        if self.selected_uid.is_none() {
            self.focused_row_target(window, cx);
        }
        // More than one selected row needs its own confirmation step.
        if self.selection_count() > 1 {
            self.request_multi_delete_confirmation(window, cx);
            return;
        }
        let Some(target) = self.delete_target(cx) else {
            self.notify(
                "Select a row before you choose Delete.",
                Severity::Warning,
                cx,
            );
            return;
        };
        self.request_delete_target_confirmation(target, window, cx);
    }

    /// Holds a multi-object delete until the user confirms it.
    fn request_multi_delete_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let objects = self.selection_refs(cx);
        if objects.len() < 2 {
            return;
        }
        let label = delete_summary(&objects);
        self.multi_delete = Some(MultiDeleteRequest { objects, label });
        let confirm_focus = self.multi_delete_confirm_focus.clone();
        cx.notify();
        // Confirm and Cancel are the only safe next steps.
        window.on_next_frame(move |window, cx| window.focus(&confirm_focus, cx));
    }

    /// Cancels a pending multi-object delete.
    pub fn cancel_multi_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.multi_delete = None;
        let table = self.table_focus_handle(cx);
        window.focus(&table, cx);
        cx.notify();
    }

    /// Confirms a pending multi-object delete.
    pub fn confirm_multi_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.multi_delete.take() else {
            return;
        };
        let count = request.objects.len();
        // Confirm and Cancel disappear with the bar, so the focus cannot stay
        // on the button the user just pressed.
        let table = self.table_focus_handle(cx);
        window.focus(&table, cx);
        if self.ops.is_none() {
            self.notify(
                "Connect to a cluster, then try Delete again.",
                Severity::Warning,
                cx,
            );
            return;
        }
        // The bar is the confirmation for a multi-object delete, so the table
        // runs the confirmed deletes itself.
        for object in request.objects {
            self.request_delete_target(object, cx);
        }
        self.notify(
            format!(
                "Delete requested for {count} objects. Each row updates when the server confirms the change."
            ),
            Severity::Info,
            cx,
        );
    }

    fn request_delete_target_confirmation(
        &mut self,
        target: DeleteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.multi_delete = None;
        match self.on_delete_requested.clone() {
            Some(handler) => handler(target, window, cx),
            None => self.notify(
                "Delete is unavailable because this view cannot confirm the action. Use the main app to delete a resource.",
                Severity::Error,
                cx,
            ),
        }
    }

    /// Opens Scale for the selected row.
    pub fn request_scale_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.supports_scale() {
            return;
        }
        // Scale sets one replica count, so it acts on the active row only.
        // Opening the dialog anyway would show a form that looks like it covers
        // the selection while it changes a single row, so stop here and name
        // the row instead.
        if self.selection_count() > 1 {
            let count = self.selection_count();
            let row = self
                .selected_object(cx)
                .and_then(|object| object.metadata.name.clone())
                .unwrap_or_else(|| "the active row".to_owned());
            self.notify(
                format!(
                    "Scale sets the replica count of {row} only, so no dialog opens for {count} selected rows. Select a single row, then choose Scale."
                ),
                Severity::Warning,
                cx,
            );
            return;
        }
        let Some(target) = self.scale_target(cx) else {
            self.notify(
                "Select a row before you choose Scale.",
                Severity::Warning,
                cx,
            );
            return;
        };
        self.request_scale_target_dialog(target, window, cx);
    }

    fn request_scale_target_dialog(
        &mut self,
        target: ScaleTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.on_scale_requested.clone() {
            Some(handler) => handler(target, window, cx),
            None => self.notify(
                "Connect to a cluster, then try Scale again.",
                Severity::Warning,
                cx,
            ),
        }
    }

    /// Runs Delete for a confirmed target.
    pub fn request_delete_target(&mut self, target: ObjectRef, cx: &mut Context<Self>) {
        let Some(ops) = self.ops.clone() else {
            self.notify(
                "Connect to a cluster, then try Delete again.",
                Severity::Warning,
                cx,
            );
            return;
        };
        let label = format!("Delete {}", target.name);
        let uid = target.uid.clone();
        self.begin_op(
            uid,
            PendingOp::Delete,
            label,
            move || ops.delete(target),
            cx,
        );
    }

    /// Starts a rolling restart for the selected workload.
    pub fn request_restart(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selection_ref(cx) else {
            self.notify(
                "Select a row before you choose Restart.",
                Severity::Warning,
                cx,
            );
            return;
        };
        self.request_restart_target(target, cx);
    }

    fn request_restart_target(&mut self, target: ObjectRef, cx: &mut Context<Self>) {
        if !matches!(
            self.spec.kind.as_ref(),
            "Deployment" | "StatefulSet" | "DaemonSet"
        ) {
            self.notify(
                "Restart is available only for workload resources.",
                Severity::Warning,
                cx,
            );
            return;
        }
        let Some(ops) = self.ops.clone() else {
            self.notify(
                "Connect to a cluster, then try Restart again.",
                Severity::Warning,
                cx,
            );
            return;
        };
        let label = format!("Restart {}", target.name);
        let uid = target.uid.clone();
        self.begin_op(
            uid,
            PendingOp::Restart,
            label,
            move || ops.restart(target),
            cx,
        );
    }

    /// Runs Scale for a confirmed target.
    pub fn request_scale_target(
        &mut self,
        target: ObjectRef,
        replicas: i32,
        cx: &mut Context<Self>,
    ) {
        let Some(ops) = self.ops.clone() else {
            self.notify(
                "Connect to a cluster, then try Scale again.",
                Severity::Warning,
                cx,
            );
            return;
        };
        let label = format!("Scale {} to {replicas}", target.name);
        let uid = target.uid.clone();
        self.begin_op(
            uid,
            PendingOp::Scale { replicas },
            label,
            move || ops.scale(target, replicas),
            cx,
        );
    }

    fn begin_op<F>(
        &mut self,
        uid: String,
        op: PendingOp,
        label: String,
        future: F,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce() -> OpsFuture<()>,
    {
        if self.host.read(cx).pending(&uid).is_some() {
            self.notify(
                format!("{label} is already pending for this resource."),
                Severity::Warning,
                cx,
            );
            return;
        }
        let future = future();
        let marked = self.host.update(cx, |host, cx| {
            let marked = host.mark_pending(&uid, op.clone());
            if marked {
                host.ensure_pending_check(cx);
            }
            marked
        });
        if !marked {
            return;
        }
        cx.notify();
        self.operation_epoch = self.operation_epoch.wrapping_add(1);
        let task_epoch = self.operation_epoch;
        let task_uid = uid.clone();
        let task_op = op;
        let host = self.host.downgrade();
        let task = cx.spawn(async move |this, cx| {
            let result = future.await;
            this.update(cx, |view, cx| {
                if !view
                    .operation_tasks
                    .get(&task_uid)
                    .is_some_and(|(epoch, _)| *epoch == task_epoch)
                {
                    return;
                }
                if result.is_err()
                    && let Some(host) = host.upgrade()
                {
                    host.update(cx, |host, _| {
                        if host
                            .pending(&task_uid)
                            .is_some_and(|pending| pending == &task_op)
                        {
                            host.resolve_pending(&task_uid);
                        }
                    });
                }
                view.on_op_finished(&label, result, cx);
            })
            .ok();
        });
        self.operation_tasks.insert(uid, (task_epoch, task));
    }

    fn operation_result_unknown_message(label: &str) -> String {
        format!("The result is unknown for {label}. Refresh the table to confirm the server state.")
    }

    fn on_op_finished(&mut self, label: &str, result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => self.notify(
                "Request sent. The table updates after the server confirms the change.",
                Severity::Info,
                cx,
            ),
            Err(reason) => {
                eprintln!("k8s-gpui: {label} request failed: {reason}");
                self.notify_with_detail(
                    Self::operation_result_unknown_message(label),
                    Severity::Error,
                    Some(reason),
                    cx,
                );
                self.refresh(cx);
            }
        }
    }

    /// Shows a notice until dismissed or replaced.
    pub fn notify(
        &mut self,
        message: impl Into<SharedString>,
        severity: Severity,
        cx: &mut Context<Self>,
    ) {
        self.notify_with_detail(message, severity, None, cx);
    }

    fn notify_with_detail(
        &mut self,
        message: impl Into<SharedString>,
        severity: Severity,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.notice_epoch.wrapping_add(1);
        self.notice_epoch = epoch;
        self.notice = Some(Notice {
            message: message.into(),
            detail: detail.map(SharedString::from),
            severity,
            epoch,
        });
        cx.notify();
        if severity != Severity::Error {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(NOTICE_DURATION).await;
                this.update(cx, |view, cx| {
                    if view.notice_epoch == epoch {
                        view.notice = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Reports whether a recovery control holds the focus. Every recovery button
    /// disappears together with the state it repairs, so the focus must move
    /// back to the table instead of staying on a removed element.
    fn focus_restore_for_recovery(&self, window: &Window) -> bool {
        self.retry_focus.is_focused(window)
            || self.stale_retry_focus.is_focused(window)
            || self.empty_action_focus.is_focused(window)
    }

    fn clear_column_settings_error(&mut self, cx: &mut Context<Self>) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.message.as_ref() == COLUMN_SETTINGS_ERROR)
        {
            self.notice = None;
            cx.notify();
        }
    }

    fn dismiss_notice(&mut self, epoch: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.epoch == epoch)
        {
            let focused = self.notice_focus.is_focused(window);
            self.notice = None;
            if focused {
                let filter_focus = self.filter.read(cx).focus_handle(cx);
                window.focus(&filter_focus, cx);
            }
            cx.notify();
        }
    }

    fn maybe_announce_latency(&mut self, cx: &mut Context<Self>) {
        if self.latency_notified {
            return;
        }
        let (high_latency, rtt) = {
            let host = self.host.read(cx);
            (host.is_high_latency(), host.latency_rtt())
        };
        if !high_latency {
            return;
        }
        self.latency_notified = true;
        let detail = rtt
            .map(|duration| format!(" (round-trip time {} ms)", duration.as_millis()))
            .unwrap_or_default();
        self.notify(
            format!(
                "High cross-region latency detected{detail}. Live updates and compression are enabled."
            ),
            Severity::Info,
            cx,
        );
    }

    fn log_request_for_object(&self, object: &DynamicObject) -> Option<LogRequest> {
        if self.spec.kind.as_ref() != "Pod" {
            return None;
        }
        Some(LogRequest {
            namespace: object.metadata.namespace.clone().map(SharedString::from),
            name: object.metadata.name.clone().map(SharedString::from)?,
            containers: pod_containers(object),
        })
    }

    /// Returns the log request for the selected Pod.
    pub fn log_request(&self, cx: &App) -> Option<LogRequest> {
        let object = self.selected_object(cx)?;
        self.log_request_for_object(&object)
    }

    fn object_ref(&self, object: &DynamicObject) -> Option<ObjectRef> {
        Some(ObjectRef {
            resource: self.spec.resource.clone()?,
            namespace: object.metadata.namespace.clone(),
            name: object.metadata.name.clone()?,
            uid: object.metadata.uid.clone()?,
        })
    }

    /// Returns the selected object reference for this resource view.
    pub fn selection_ref(&self, cx: &App) -> Option<ObjectRef> {
        let object = self.selected_object(cx)?;
        self.object_ref(&object)
    }

    /// Returns the current table state.
    pub fn status(&self, cx: &App) -> TableStatus {
        self.host.read(cx).status()
    }

    /// Returns the number of unconfirmed operations.
    pub fn pending_count(&self, cx: &App) -> usize {
        self.host.read(cx).pending_count()
    }

    /// Returns the number of rows the table currently shows.
    pub fn row_count(&self, cx: &App) -> usize {
        self.host.read(cx).row_count()
    }

    /// Returns the selected resource name.
    pub fn selected_name(&self, cx: &App) -> Option<SharedString> {
        let object = self.selected_object(cx)?;
        object.metadata.name.clone().map(SharedString::from)
    }

    /// Pauses live updates.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.latency_auto_paused = false;
        if !matches!(self.host.read(cx).status(), TableStatus::Paused) {
            self.host
                .update(cx, |host, cx| host.dispatch(TableEvent::Pause, cx));
        }
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.latency_auto_paused = false;
        if matches!(self.host.read(cx).status(), TableStatus::Paused) {
            self.host
                .update(cx, |host, cx| host.dispatch(TableEvent::Resume, cx));
        }
    }

    pub fn pause_for_latency(&mut self, cx: &mut Context<Self>) -> bool {
        if matches!(self.host.read(cx).status(), TableStatus::Paused) {
            return self.latency_auto_paused;
        }
        self.latency_auto_paused = true;
        self.host
            .update(cx, |host, cx| host.dispatch(TableEvent::Pause, cx));
        let paused = matches!(self.host.read(cx).status(), TableStatus::Paused);
        if !paused {
            self.latency_auto_paused = false;
        }
        paused
    }

    pub fn resume_from_latency(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.latency_auto_paused {
            return false;
        }
        self.latency_auto_paused = false;
        if matches!(self.host.read(cx).status(), TableStatus::Paused) {
            self.host
                .update(cx, |host, cx| host.dispatch(TableEvent::Resume, cx));
        }
        true
    }

    fn toggle_updates(&mut self, cx: &mut Context<Self>) {
        let status = self.host.read(cx).status();
        let status_label = status.label().to_lowercase();
        let event = match status {
            TableStatus::Paused => {
                self.latency_auto_paused = false;
                TableEvent::Resume
            }
            TableStatus::Listing | TableStatus::Streaming => {
                self.latency_auto_paused = false;
                TableEvent::Pause
            }
            _ => {
                self.notify(
                    format!(
                        "Live updates are not available while the table is {}. Refresh the table to reconnect.",
                        status_label
                    ),
                    Severity::Warning,
                    cx,
                );
                return;
            }
        };
        self.host.update(cx, |host, cx| host.dispatch(event, cx));
    }

    fn toggle_churn(&mut self, cx: &mut Context<Self>) {
        if let Some(churn) = &self.churn {
            churn.toggle();
            cx.notify();
        }
    }

    fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn clear_filter(&mut self, cx: &mut Context<Self>) {
        // Clear Filter clears every filter, including the status one.
        if self.problems_only {
            self.apply_problems_only(false, cx);
        }
        if self.filter.read(cx).text().is_empty() {
            return;
        }
        let filter = self.filter.clone();
        cx.defer(move |cx| {
            filter.update(cx, |input, cx| input.clear(cx));
        });
    }

    fn prune_operation_tasks(&mut self, cx: &App) {
        let host = self.host.read(cx);
        self.operation_tasks
            .retain(|uid, _| host.pending(uid.as_str()).is_some());
    }

    /// Clears selection when the selected row leaves the snapshot.
    fn on_host_changed(&mut self, cx: &mut Context<Self>) {
        let expired = self.host.update(cx, |host, _| host.take_expired_pending());
        if !expired.is_empty() {
            // The deadline notice must name the rows it could not confirm.
            let labels = self.host.read(cx).pending_labels(&expired).join(" and ");
            self.notify(
                Self::operation_result_unknown_message(&labels),
                Severity::Error,
                cx,
            );
            self.refresh(cx);
        }
        self.prune_operation_tasks(cx);
        // A rebuild or a delete can drop rows that were part of the selection.
        if let Some(snapshot) = self.host.read(cx).snapshot() {
            self.prune_selection(&snapshot);
        }
        if let Some(uid) = self.selected_uid.clone() {
            let still_present = self
                .host
                .read(cx)
                .snapshot()
                .is_some_and(|snapshot| snapshot.by_uid.contains_key(uid.as_ref()));
            if !still_present {
                self.select_only(None);
                self.inspected_object = None;
                self.push_inspector_selection(None, cx);
                self.notify_selection(None, cx);
            } else {
                let current = self.selected_row(cx);
                let changed = Self::inspection_changed(
                    self.inspected_object.as_ref(),
                    current.as_ref().map(|row| &row.obj),
                ) || self.inspector.is_none();
                self.sync_inspector(cx);
                if changed {
                    self.notify_selection(current, cx);
                }
            }
        }
        self.maybe_announce_latency(cx);
        cx.notify();
    }

    fn selected_object(&self, cx: &App) -> Option<Arc<DynamicObject>> {
        let snapshot = self.host.read(cx).snapshot()?;
        let uid = self.selected_uid.as_ref()?;
        snapshot
            .row_by_uid(uid.as_ref())
            .map(|row| Arc::clone(&row.obj))
    }

    fn selected_row(&self, cx: &App) -> Option<Row> {
        let snapshot = self.host.read(cx).snapshot()?;
        let uid = self.selected_uid.as_ref()?;
        snapshot.row_by_uid(uid.as_ref()).cloned()
    }

    fn inspection_changed(
        inspected: Option<&Arc<DynamicObject>>,
        current: Option<&Arc<DynamicObject>>,
    ) -> bool {
        match (inspected, current) {
            (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        }
    }

    /// Sends the selected object to the Inspector when it changes.
    fn sync_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            return;
        }
        let Some(obj) = self.selected_object(cx) else {
            self.inspected_object = None;
            self.push_inspector_selection(None, cx);
            return;
        };
        if self
            .inspected_object
            .as_ref()
            .is_some_and(|inspected| Arc::ptr_eq(inspected, &obj))
        {
            return;
        }
        self.inspected_object = Some(Arc::clone(&obj));
        let yaml = serde_yaml_ng::to_string(obj.as_ref()).ok();
        let reference = self.spec.resource.as_ref().and_then(|resource| {
            Some(ObjectRef {
                resource: resource.clone(),
                namespace: obj.metadata.namespace.clone(),
                name: obj.metadata.name.clone()?,
                uid: obj.metadata.uid.clone()?,
            })
        });
        match (reference, yaml) {
            (Some(object), Some(yaml)) => {
                self.push_inspector_selection(Some(InspectorSelection { object, yaml }), cx)
            }
            (None, yaml) => {
                if let Some(inspector) = self.inspector.clone() {
                    inspector.apply(InspectorUpdate::Yaml(yaml), cx);
                }
            }
            (Some(_), None) => self.push_inspector_selection(None, cx),
        }
    }

    fn push_inspector_selection(
        &mut self,
        selection: Option<InspectorSelection>,
        cx: &mut Context<Self>,
    ) {
        if let Some(inspector) = self.inspector.clone() {
            inspector.apply(InspectorUpdate::Selection(selection), cx);
        }
    }

    fn notify_selection(&self, row: Option<Row>, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_selection_changed.clone() {
            handler(row, cx);
        }
    }

    pub(crate) fn table_focus_handle(&self, cx: &App) -> FocusHandle {
        self.interaction.read(cx).focus_handle.clone()
    }

    /// Returns the filter focus handle.
    pub fn filter_focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }

    fn selected_index(&self, snapshot: &IndexSnapshot) -> Option<usize> {
        self.selected_uid
            .as_ref()
            .and_then(|uid| snapshot.by_uid.get(uid.as_ref()).copied())
    }

    /// Reports whether a row UID is part of the selection.
    fn is_selected_uid(&self, uid: Option<&str>) -> bool {
        uid.is_some_and(|uid| self.selected_uids.contains(uid))
    }

    /// Returns the number of selected rows.
    pub fn selection_count(&self) -> usize {
        self.selected_uids.len()
    }

    /// Returns every selected object in the order the table shows them.
    pub fn selection_refs(&self, cx: &App) -> Vec<ObjectRef> {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return Vec::new();
        };
        let mut refs = Vec::with_capacity(self.selected_uids.len());
        for row in &snapshot.rows {
            if !self.is_selected_uid(row.obj.metadata.uid.as_deref()) {
                continue;
            }
            if let Some(object) = self.object_ref(&row.obj) {
                refs.push(object);
            }
        }
        refs
    }

    /// Replaces the selection with one row and resets the range anchor.
    fn select_only(&mut self, uid: Option<SharedString>) {
        self.selected_uid = uid.clone();
        self.selection_anchor = uid.clone();
        self.selected_uids = match uid {
            Some(uid) => Arc::new(HashSet::from([uid])),
            None => Arc::new(HashSet::new()),
        };
    }

    /// Drops selected rows that the snapshot no longer holds.
    fn prune_selection(&mut self, snapshot: &IndexSnapshot) {
        if self.selected_uids.is_empty() {
            return;
        }
        let mut kept = HashSet::with_capacity(self.selected_uids.len());
        for uid in self.selected_uids.iter() {
            if snapshot.by_uid.contains_key(uid.as_ref()) {
                kept.insert(uid.clone());
            }
        }
        if kept.len() == self.selected_uids.len() {
            return;
        }
        if self
            .selected_uid
            .as_ref()
            .is_some_and(|uid| !kept.contains(uid.as_ref()))
        {
            // Keep one active row so Enter and the Inspector still have a target.
            let next = kept.iter().min().cloned();
            self.selected_uid = next.clone();
            self.selection_anchor = next;
        }
        self.selected_uids = Arc::new(kept);
    }

    /// Selects one row, focuses the table, and reveals the row.
    fn select_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let Some(row) = snapshot.rows.get(index) else {
            return;
        };
        self.select_only(row.obj.metadata.uid.clone().map(SharedString::from));
        self.focus_target = FocusTarget::Row;
        self.pending_column_reveal = false;
        self.sync_inspector(cx);
        self.notify_selection(Some(row.clone()), cx);
        // The scrollbar counts listed rows, which the problems filter changes.
        let position = self.shown_position(&snapshot, index);
        self.reveal_row(position, window, cx);
    }

    /// Extends the selection from the anchor to one row without clearing it.
    fn extend_selection_to(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let Some(row) = snapshot.rows.get(index) else {
            return;
        };
        let Some(uid) = row.obj.metadata.uid.clone().map(SharedString::from) else {
            return;
        };
        let anchor = self
            .selection_anchor
            .clone()
            .or_else(|| self.selected_uid.clone());
        let anchor_index = anchor
            .as_ref()
            .and_then(|anchor| snapshot.by_uid.get(anchor.as_ref()).copied())
            .unwrap_or(index);
        let (start, end) = (anchor_index.min(index), anchor_index.max(index));
        let mut next = HashSet::with_capacity(end - start + 1);
        for row in snapshot.rows.iter().take(end + 1).skip(start) {
            if let Some(uid) = row.obj.metadata.uid.as_deref() {
                next.insert(SharedString::from(uid));
            }
        }
        // Ctrl+click then Shift+arrow keeps the toggled rows in the range.
        for uid in self.selected_uids.iter() {
            next.insert(uid.clone());
        }
        next.insert(uid.clone());
        self.selected_uids = Arc::new(next);
        self.selected_uid = Some(uid);
        self.selection_anchor = anchor;
        self.focus_target = FocusTarget::Row;
        self.pending_column_reveal = false;
        self.sync_inspector(cx);
        self.notify_selection(Some(row.clone()), cx);
        self.reveal_row(index, window, cx);
    }

    /// Adds or removes one row from the selection.
    fn toggle_selection_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let Some(row) = snapshot.rows.get(index) else {
            return;
        };
        let Some(uid) = row.obj.metadata.uid.clone().map(SharedString::from) else {
            return;
        };
        let mut next = self.selected_uids.as_ref().clone();
        let removed = !next.insert(uid.clone());
        if removed {
            next.remove(&uid);
        }
        // The last remaining row cannot be unselected, so Enter always has a target.
        if next.is_empty() {
            next.insert(uid.clone());
        }
        self.selected_uids = Arc::new(next);
        self.selected_uid = Some(uid);
        self.selection_anchor = self.selected_uid.clone();
        self.focus_target = FocusTarget::Row;
        self.pending_column_reveal = false;
        self.sync_inspector(cx);
        self.notify_selection(Some(row.clone()), cx);
        self.reveal_row(index, window, cx);
    }

    /// Selects every listed row and leaves the first one active.
    fn select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        // A hidden row stays out of the selection, so the problems filter and
        // the name filter behave the same way.
        let rows: Vec<(usize, SharedString)> = self
            .shown_rows(&snapshot)
            .into_iter()
            .filter_map(|index| {
                snapshot.rows.get(index).and_then(|row| {
                    row.obj
                        .metadata
                        .uid
                        .as_deref()
                        .map(|uid| (index, SharedString::from(uid)))
                })
            })
            .collect();
        let Some((first, first_uid)) = rows.first() else {
            return;
        };
        let first = *first;
        self.selected_uids = Arc::new(rows.iter().map(|(_, uid)| uid.clone()).collect());
        self.selected_uid = Some(first_uid.clone());
        self.selection_anchor = self.selected_uid.clone();
        self.focus_target = FocusTarget::Row;
        self.pending_column_reveal = false;
        self.sync_inspector(cx);
        self.notify_selection(snapshot.rows.get(first).cloned(), cx);
        self.reveal_row(first, window, cx);
    }

    /// Focuses the table and scrolls one row into view. The index is a snapshot
    /// index, so the scroll position follows the listed rows.
    fn reveal_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (focus_handle, scroll_handle, position) = {
            let interaction = self.interaction.read(cx);
            let position = self
                .host
                .read(cx)
                .snapshot()
                .map(|snapshot| self.shown_position(&snapshot, index))
                .unwrap_or(index);
            (
                interaction.focus_handle.clone(),
                interaction.scroll_handle.clone(),
                position,
            )
        };
        scroll_handle.scroll_to_item(position, ScrollStrategy::Nearest);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// Moves column selection within the visible columns.
    fn move_column(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible_columns.is_empty() {
            return;
        }
        self.focus_target = FocusTarget::Column;
        self.pending_column_reveal = true;
        let position = self
            .visible_columns
            .iter()
            .position(|&index| index == self.selected_column)
            .unwrap_or(0);
        let next = target_column(position, delta, self.visible_columns.len());
        self.selected_column = self.visible_columns[next];
        self.reveal_selected_column(window, cx);
        cx.notify();
    }

    fn set_selected_column(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_target = FocusTarget::Column;
        self.pending_column_reveal = true;
        if self.selected_column != index {
            self.selected_column = index;
        }
        self.reveal_selected_column(window, cx);
        cx.notify();
    }

    fn sort_selected_column(&mut self, cx: &mut Context<Self>) {
        self.focus_target = FocusTarget::Column;
        let index = self.selected_column;
        let current = if self.relevance_sort {
            None
        } else {
            self.host.read(cx).sort()
        };
        let next = next_sort(current, index, self.default_sort_column());
        self.apply_sort(next, cx);
    }

    /// Restarts the source for the current view.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.host.update(cx, |host, cx| host.refresh(cx));
    }

    fn focused_row_index(&self, snapshot: &IndexSnapshot) -> Option<usize> {
        self.selected_index(snapshot)
            .or_else(|| self.shown_rows(snapshot).first().copied())
    }

    /// Returns the row the table acts on. The highlighted row becomes the
    /// selection when the table has focus and nothing is selected yet, so every
    /// row action has a target right after the table takes focus.
    fn focused_row_target(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let snapshot = self.host.read(cx).snapshot()?;
        if self.selected_uid.is_some() {
            return self.selected_index(&snapshot);
        }
        let index = self.focused_row_index(&snapshot)?;
        self.select_index(index, window, cx);
        Some(index)
    }

    /// Opens the selected column menu or row details.
    /// Returns the action the current empty state offers. The button and the
    /// keyboard path both ask here, so Enter always runs the action the user
    /// can see.
    fn empty_state_action(&self, cx: &App) -> Box<dyn gpui::Action> {
        let status = self.host.read(cx).status();
        // A filter or a problems-only view hides rows, and clearing it is the
        // way back to the full list.
        let narrowed = self.problems_only || !self.filter.read(cx).text().is_empty();
        if matches!(status, TableStatus::Paused) {
            Box::new(ToggleUpdates)
        } else if matches!(status, TableStatus::Failed(_))
            || (matches!(status, TableStatus::Stale(_)) && !narrowed)
        {
            Box::new(Refresh)
        } else if narrowed {
            Box::new(ClearFilter)
        } else {
            Box::new(Refresh)
        }
    }

    fn open_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.empty_namespace_focus.is_focused(window) {
            window.dispatch_action(Box::new(OpenNamespaceSwitcher), cx);
            cx.stop_propagation();
            return;
        }
        if self.empty_action_focus.is_focused(window) {
            let action = self.empty_state_action(cx);
            window.dispatch_action(action, cx);
            cx.stop_propagation();
            return;
        }
        // The Status header's problems filter is a button in its own right, so
        // Enter and Space toggle the filter instead of opening a detail pane.
        if self.problems_filter_focus.is_focused(window) {
            self.toggle_problems_filter(cx);
            cx.stop_propagation();
            return;
        }
        if self.focus_target == FocusTarget::Column {
            self.open_header_context_menu(window, cx);
            return;
        }
        if let Some(index) = self.focused_row_target(window, cx) {
            self.activate_index(index, window, cx);
        }
    }

    fn move_selection(&mut self, movement: Move, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection_with(movement, false, window, cx);
    }

    /// Moves the active row. Shift keeps the rows in between selected.
    fn move_selection_with(
        &mut self,
        movement: Move,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        // Movement stays inside the listed rows, so the problems filter does
        // not move the selection onto a row the table hides.
        let shown = self.shown_rows(&snapshot);
        let len = shown.len();
        if len == 0 {
            return;
        }
        let page = self.page_rows(cx);
        let current = self
            .selected_index(&snapshot)
            .and_then(|index| shown.iter().position(|&shown| shown == index));
        let position = target_index(movement, current, len, page);
        let Some(&index) = shown.get(position) else {
            return;
        };
        if extend {
            self.extend_selection_to(index, window, cx);
        } else {
            self.select_index(index, window, cx);
        }
    }

    /// Handles the multi-select keys: Shift extends, Control or Command plus A
    /// selects every loaded row. Reports whether the key was consumed.
    fn handle_selection_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self
            .interaction
            .read(cx)
            .focus_handle
            .contains_focused(window, cx)
        {
            return false;
        }
        if (keystroke.modifiers.control || keystroke.modifiers.platform)
            && keystroke.key.as_str() == "a"
            && !keystroke.modifiers.alt
        {
            self.select_all(window, cx);
            return true;
        }
        if !keystroke.modifiers.shift
            || keystroke.modifiers.control
            || keystroke.modifiers.platform
            || keystroke.modifiers.alt
        {
            return false;
        }
        let movement = match keystroke.key.as_str() {
            "up" => Move::Up,
            "down" => Move::Down,
            "pageup" => Move::PageUp,
            "pagedown" => Move::PageDown,
            "home" => Move::Home,
            "end" => Move::End,
            _ => return false,
        };
        self.move_selection_with(movement, true, window, cx);
        true
    }

    fn page_rows(&self, cx: &App) -> usize {
        let interaction = self.interaction.read(cx);
        let height = f32::from(
            interaction
                .scroll_handle
                .0
                .borrow()
                .base_handle
                .bounds()
                .size
                .height,
        );
        rows_in_viewport(height, row_height(cx))
    }

    fn activate_row(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_activate_row.clone() {
            handler(row, window, cx);
        }
    }

    /// Opens details for one row.
    fn activate_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let Some(row) = snapshot.rows.get(index).cloned() else {
            return;
        };
        self.activate_row(row, window, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if self.context_menu.is_some()
            && self
                .interaction
                .read(cx)
                .focus_handle
                .contains_focused(window, cx)
        {
            cx.stop_propagation();
            return;
        }
        // Escape leaves a pending multi-delete confirmation.
        if self.multi_delete.is_some() && keystroke.key.as_str() == "escape" {
            self.multi_delete = None;
            // The bar holds the focus, and it goes away with the key press.
            let table = self.table_focus_handle(cx);
            window.focus(&table, cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.handle_selection_key(keystroke, window, cx) {
            cx.stop_propagation();
            return;
        }
        if keystroke.modifiers.alt
            || keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.shift
        {
            // Shift+F10 and the Menu key are the keyboard context-menu keys.
            if !is_row_actions_key(keystroke) {
                return;
            }
        }
        if matches!(keystroke.key.as_str(), "enter" | "space") {
            let status = self.host.read(cx).status();
            if self.empty_namespace_focus.is_focused(window) {
                window.dispatch_action(Box::new(OpenNamespaceSwitcher), cx);
                cx.stop_propagation();
                return;
            }
            if self.retry_focus.is_focused(window) && matches!(&status, TableStatus::Failed(_)) {
                window.dispatch_action(Box::new(Refresh), cx);
                cx.stop_propagation();
                return;
            }
            if self.empty_action_focus.is_focused(window) {
                let action = self.empty_state_action(cx);
                window.dispatch_action(action, cx);
                cx.stop_propagation();
                return;
            }
        }
        if !self
            .interaction
            .read(cx)
            .focus_handle
            .contains_focused(window, cx)
        {
            return;
        }
        if is_row_actions_key(keystroke) {
            return self.open_row_actions(window, cx);
        }
        match keystroke.key.as_str() {
            "pageup" => self.move_selection(Move::PageUp, window, cx),
            "pagedown" => self.move_selection(Move::PageDown, window, cx),
            "home" => self.move_selection(Move::Home, window, cx),
            "end" => self.move_selection(Move::End, window, cx),
            _ => {}
        }
    }

    /// Opens the row or column menu that matches the current focus.
    fn open_row_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_target == FocusTarget::Column {
            self.open_header_context_menu(window, cx);
        } else {
            self.open_row_context_menu(None, window, cx);
        }
        cx.stop_propagation();
    }

    /// Applies the latest filter input after the debounce interval.
    fn on_filter_input(&mut self, text: String, cx: &mut Context<Self>) {
        let epoch = self.filter_epoch.get().wrapping_add(1);
        self.filter_epoch.set(epoch);
        self.filter_pending = true;
        cx.notify();
        if self.filter_task.is_some() {
            self.filter_task = None;
        }
        self.filter_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FILTER_DEBOUNCE).await;
            let _ = this.update(cx, |view, cx| {
                if view.filter_epoch.get() == epoch {
                    view.commit_filter(&text, cx);
                }
            });
        }));
    }

    fn commit_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter_pending = false;
        let needle = text.trim();
        let active = !needle.is_empty();
        let was_relevance = self.relevance_sort;
        if active && !self.filter_active {
            self.sort_before_filter = self.host.read(cx).sort();
            self.relevance_sort = true;
        } else if !active && self.filter_active {
            self.relevance_sort = false;
        }
        self.filter_active = active;
        let restore_sort = if !active && was_relevance {
            self.sort_before_filter.take()
        } else {
            None
        };
        let filter = Filter {
            name_substring: active.then(|| needle.to_owned()),
            ..Filter::default()
        };
        self.host.update(cx, |host, cx| {
            host.set_relevance(self.relevance_sort);
            if let Some(sort) = restore_sort {
                host.dispatch(TableEvent::SortChanged { sort: Some(sort) }, cx);
            }
            host.dispatch(TableEvent::FilterChanged { filter }, cx)
        });
    }

    fn toolbar(
        &self,
        status: &TableStatus,
        filtered: usize,
        total: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let paused = matches!(status, TableStatus::Paused);
        let compact = is_compact_width(window.viewport_size().width);
        let churn = self.churn.clone();
        let (latency_rtt, cached, relevance, sort) = {
            let host = self.host.read(cx);
            (
                host.latency_rtt(),
                host.cached(),
                host.relevance(),
                host.sort(),
            )
        };
        let filter_text = self.filter.read(cx).text().to_owned();
        let sort_mode = sort_mode_label(relevance, sort, &self.columns, &filter_text);
        let plural = self.spec.label_lower();
        // Show sync state until the initial list is complete.
        let count = toolbar_count(matches!(status, TableStatus::Listing), filtered, total);
        // The count carries the resource noun so it reads as a labelled value.
        let count_label = format!("{plural} {count}");
        let count_description = if filtered == total {
            format!("All {count} {plural} are shown.")
        } else {
            format!("{count} {plural} match the current filters.")
        };
        let updates_enabled = matches!(
            status,
            TableStatus::Listing | TableStatus::Streaming | TableStatus::Paused
        );
        let updates_label = if !updates_enabled {
            "Live Updates Unavailable"
        } else if paused {
            "Resume Live Updates"
        } else {
            "Pause Live Updates"
        };
        let updates_tooltip = if updates_enabled {
            action_tooltip(updates_label, &ToggleUpdates, cx)
        } else {
            updates_label.to_owned()
        };
        let churn_tooltip = action_tooltip("Toggle Test Updates", &ToggleChurn, cx);
        let stale_retry_tooltip = action_tooltip(RETRY_LIVE_UPDATES, &Refresh, cx);
        let stale_retry_focus = self.stale_retry_focus.clone();
        // A range selection used to be invisible: the member rows shared the
        // selection fill with a measured 1.018:1 against the row beside them,
        // and nothing anywhere counted them. The chip carries the count, and
        // `Role::Status` makes it a live region, so a selection change is
        // announced as well as drawn.
        let selection = self.selection_count();
        let selection_label = format!("{} selected", design::format::count(selection));
        let selection_description =
            design::format::count_with_noun(selection, "row selected", "rows selected");
        let mut filter_action = div()
            .id("table-filter-action")
            .flex_none()
            .child(self.filter.clone());
        filter_action
            .interactivity()
            .tooltip(Tooltip::text(action_tooltip(
                "Focus Resource Filter",
                &FocusFilter,
                cx,
            )));
        h_flex()
            .h(design::size::TOOLBAR)
            .gap(design::space::SM)
            .px(design::space::SM)
            // Keep the toolbar filter in the Tab order.
            .tab_group()
            .tab_index(2)
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(status_badge(status, cx))
            .when_some(cached, |this, cached| this.child(cached_chip(cached, cx)))
            .child(toolbar_chip(count_label, count_description, cx))
            .when(selection > 0, |this| {
                this.child(toolbar_chip(selection_label, selection_description, cx))
            })
            // Sort direction lives in the column header, so the toolbar only
            // explains an order no column owns.
            .when(relevance && !filter_text.is_empty(), |this| {
                this.child(toolbar_chip(
                    sort_mode,
                    "Rows are ordered by how well they match the filter.",
                    cx,
                ))
            })
            .when_some(latency_rtt.filter(|_| !compact), |this, rtt| {
                this.child(measure_chip("RTT", rtt, cx))
            })
            .when(self.filter_pending, |this| {
                this.child(toolbar_chip(
                    "Filtering…",
                    "Waiting for the filtered list.",
                    cx,
                ))
            })
            .when(
                matches!(status, TableStatus::Stale(_)) && filtered > 0,
                |this| {
                    this.child(
                        div()
                            .flex_none()
                            .on_key_down(cx.listener(
                                move |_view, event: &KeyDownEvent, window, cx| {
                                    if is_toolbar_activation_key(event) {
                                        window.dispatch_action(Box::new(Refresh), cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(
                                Button::new("stale-retry", RETRY_LIVE_UPDATES)
                                    .size(ButtonSize::Medium)
                                    .track_focus(&stale_retry_focus)
                                    .tab_index(0isize)
                                    .tooltip(Tooltip::text(stale_retry_tooltip.clone()))
                                    .on_click(cx.listener(
                                        move |_view, _event: &ClickEvent, window, cx| {
                                            window.dispatch_action(Box::new(Refresh), cx);
                                        },
                                    )),
                            ),
                    )
                },
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(design::space::XS)
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background.alpha(1.))
                    .on_key_down(cx.listener(move |_view, event: &KeyDownEvent, window, cx| {
                        if is_toolbar_activation_key(event) && updates_enabled {
                            window.dispatch_action(Box::new(ToggleUpdates), cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        IconButton::new(
                            "pause-toggle",
                            if paused {
                                IconName::PlayOutlined
                            } else {
                                IconName::DebugPause
                            },
                        )
                        .size(ButtonSize::Medium)
                        .icon_size(IconSize::XSmall)
                        .toggle_state(paused)
                        // A toggle that is on has to look on. `DESIGN.md §5`
                        // asks for a selected style, and without one the button
                        // fell through to the shared default the app never
                        // reviewed.
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(!updates_enabled)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(updates_tooltip))
                        .aria_label(if !updates_enabled {
                            "Live Updates Unavailable"
                        } else if paused {
                            "Resume Live Updates"
                        } else {
                            "Pause Live Updates"
                        })
                        .on_click(cx.listener(
                            move |_view, _event: &ClickEvent, window, cx| {
                                window.dispatch_action(Box::new(ToggleUpdates), cx);
                            },
                        )),
                    ),
            )
            .when_some(churn.filter(|_| !compact), |this, churn| {
                this.child(
                    div()
                        .flex_none()
                        .on_key_down(cx.listener(move |_view, event: &KeyDownEvent, window, cx| {
                            if is_toolbar_activation_key(event) {
                                window.dispatch_action(Box::new(ToggleChurn), cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            Button::new(
                                "churn-toggle",
                                if churn.is_enabled() {
                                    "Test Updates: On"
                                } else {
                                    "Test Updates: Off"
                                },
                            )
                            .tab_index(0isize)
                            .tooltip(Tooltip::text(churn_tooltip.clone()))
                            .on_click(cx.listener(
                                move |_view, _event: &ClickEvent, window, cx| {
                                    window.dispatch_action(Box::new(ToggleChurn), cx);
                                },
                            )),
                        ),
                )
            })
            .child(div().flex_grow_1())
            .child(filter_action)
            .into_any_element()
    }

    /// `shown` is the list of row indices the body actually renders, so the
    /// toolbar's count and the body cannot be derived from two different filters.
    #[allow(clippy::too_many_arguments)]
    fn table(
        &mut self,
        snapshot: Option<Arc<IndexSnapshot>>,
        shown: Arc<Vec<usize>>,
        sort: Option<Sort>,
        status: &TableStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let table_focus = self.table_focus_handle(cx);
        let table_focused = table_focus.contains_focused(window, cx);
        // The problems filter can hide rows, so the list maps positions to rows.
        let row_count = shown.len();
        let widths = self.visible_widths(window, cx);
        if self.pending_column_reveal {
            self.reveal_selected_column(window, cx);
        }
        self.note_column_widths(cx);
        let relevance = self.host.read(cx).relevance();
        let header = self.header_cells(sort, relevance, table_focused, &widths, window, cx);
        let rows_snapshot = snapshot.clone();
        let rows_columns = self.columns.clone();
        let rows_visible = self.visible_columns.clone();
        let map_columns = self.columns.clone();
        let map_visible = self.visible_columns.clone();
        let rows_selected = Arc::clone(&self.selected_uids);
        let rows_anchor = self.selected_uid.clone();
        let rows_host = self.host.clone();
        let has_status_column = self
            .visible_columns
            .iter()
            .any(|&index| self.columns[index].column.id == "status");
        let map_view = cx.entity().downgrade();
        let map_snapshot = snapshot.clone();
        let map_selected = Arc::clone(&self.selected_uids);
        let map_anchor = self.selected_uid.clone();
        let map_table_focused = table_focused;
        let click_focus = table_focus.clone();
        let empty_action_focus = self.empty_action_focus.clone();
        let empty_snapshot = snapshot;
        let empty_status = status.clone();
        let empty_filter = self.filter.read(cx).text().to_owned();
        let empty_spec = self.spec.clone();
        let empty_columns = self.columns.clone();
        let empty_visible = self.visible_columns.clone();
        let empty_widths = widths.clone();
        let empty_namespace_focus = self.empty_namespace_focus.clone();
        let empty_reason_focus = self.error_reason_focus.clone();
        let empty_viewport_height = window.viewport_size().height;
        // `DESIGN.md §3.4` files `surface` under tables and inputs. The table
        // used to paint its rows on `canvas`, which left the one level of the
        // ramp a reader stares at for eight hours consumed by a 34px tab strip.
        // Every row state below composites onto the same base, so a striped
        // row, a hovered row, the keyboard cursor and a selected row can never
        // end up on different surfaces.
        let row_background = table_row_surface(cx);
        let stripe_background = design::row_stripe_bg(cx);
        let selected_background = design::row_selected_bg(cx);
        let selected_muted_background = row_selected_muted_bg(cx);
        let hover_background = design::row_hover_bg(cx);
        let selected_edge = cx.theme().colors().text_accent;
        let selected_muted_edge = selected_edge.opacity(0.55);
        // The keyboard cursor used to share the hover token, so the row the
        // keyboard was on and the row the pointer was over were the same
        // colour, and in the light appearance the focus wash was quieter than
        // the zebra stripe it sat next to.
        let focused_background = design::row_focus_bg(cx);
        let focused_edge = cx.theme().colors().border_focused;
        let row_border = cx.theme().colors().border_variant;
        let map_row_count = row_count;
        let rows_typography = DataTypography::from_theme_settings(cx);
        let rows_row_height = rows_typography.row_height();
        let rows_char_width = f32::from(rows_typography.size) * CHAR_WIDTH_RATIO;
        let rows_widths = widths.clone();
        let rows_shown = Arc::clone(&shown);
        let map_shown = Arc::clone(&shown);
        // Freshness is a property of the table, so every row it still draws
        // inherits it rather than each row re-deciding.
        let rows_table_is_stale = matches!(status, TableStatus::Stale(_));

        let table = Table::new(self.visible_columns.len())
            .no_ui_font()
            .disable_base_style()
            .uniform_list("pods-rows", row_count, move |range, window, cx| {
                let Some(snapshot) = rows_snapshot.as_ref() else {
                    return Vec::new();
                };
                range
                    .filter_map(|position| {
                        let index = rows_shown.get(position).copied()?;
                        snapshot.rows.get(index).map(|row| {
                            let uid = row.obj.metadata.uid.as_deref();
                            let selected = rows_selected.contains(uid.unwrap_or_default());
                            let visual = row_visual(
                                selected,
                                selected && rows_anchor.as_deref() == uid,
                                table_focused,
                                !rows_selected.is_empty(),
                                position,
                            );
                            let background = row_visual_background(
                                visual,
                                selected_background,
                                selected_muted_background,
                                focused_background,
                                stripe_background,
                                row_background,
                            );
                            // Merge pending state into the rendered row, with
                            // the time left before the table stops waiting.
                            let pending = row.obj.metadata.uid.as_deref().and_then(|uid| {
                                let host = rows_host.read(cx);
                                let op = host.pending(uid).cloned()?;
                                Some(PendingState {
                                    op,
                                    remaining: host.pending_remaining(uid).unwrap_or_default(),
                                })
                            });
                            let trailing =
                                Some(row_confidence(rows_table_is_stale, row, &rows_columns));
                            row_cells(
                                position,
                                row,
                                &rows_columns,
                                &rows_visible,
                                &rows_widths,
                                &rows_typography,
                                rows_char_width,
                                background,
                                visual.is_selected(),
                                pending.as_ref(),
                                has_status_column,
                                trailing,
                                window,
                                cx,
                            )
                        })
                    })
                    .collect()
            })
            .header(header)
            .width_config(ColumnWidthConfig::Resizable(self.columns_state.clone()))
            .interactable(&self.interaction)
            .hide_row_hover()
            .hide_row_borders()
            .map_row(move |(position, row), _window, _cx| {
                // Selection and row actions use the snapshot index; the row
                // itself is addressed by its list position.
                let index = map_shown.get(position).copied().unwrap_or(position);
                let is_selected = map_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.rows.get(index))
                    .is_some_and(|row| {
                        map_selected.contains(row.obj.metadata.uid.as_deref().unwrap_or_default())
                    });
                let visual = row_visual(
                    is_selected,
                    is_selected
                        && map_anchor.as_deref()
                            == map_snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.rows.get(index))
                                .and_then(|row| row.obj.metadata.uid.as_deref()),
                    map_table_focused,
                    !map_selected.is_empty(),
                    position,
                );
                let row_label = map_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.rows.get(index))
                    .map(|row| {
                        format!(
                            "{}. Enter opens details. {ROW_ACTIONS_KEYS} opens Row Actions.",
                            row_accessible_label(&map_columns, &map_visible, &row.cells)
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "Row {}. Enter opens details. {ROW_ACTIONS_KEYS} opens Row Actions.",
                            position + 1
                        )
                    });
                let background = row_visual_background(
                    visual,
                    selected_background,
                    selected_muted_background,
                    focused_background,
                    stripe_background,
                    row_background,
                );
                let mut row = row
                    .debug_selector(move || format!("resource-row-{position}"))
                    .id(("resource-row", position))
                    .role(Role::Row)
                    .aria_label(row_label)
                    // The header is the grid's first row, so the data starts
                    // at 2. The loading skeleton already numbered it that way,
                    // and two states that disagree by one make a screen reader
                    // report the wrong row after an action.
                    .aria_row_index(grid_row_index(position))
                    .aria_selected(is_selected)
                    // The trailing ellipsis button used to advertise these. With
                    // it gone the row itself carries them, so a keyboard or
                    // screen-reader user can still find the row menu.
                    .aria_keyshortcuts(ROW_ACTIONS_KEYSHORTCUTS)
                    // The trailing action button belongs to this row, so it
                    // shows while the row is hovered.
                    .group(row_hover_group(position))
                    .relative()
                    .h(rows_row_height)
                    .bg(background);
                if position + 1 < map_row_count {
                    row = row.border_b_1().border_color(row_border);
                }
                // Keep selected and focused rows visually distinct from hover.
                if visual.has_rail() {
                    let (rail, rail_color) = if uses_table_focus_rail(visual) {
                        (design::border::TABLE_FOCUS_RAIL, selected_edge)
                    } else if visual.is_selected() {
                        (design::border::FOCUS_RAIL, selected_muted_edge)
                    } else {
                        (design::border::FOCUS_RAIL, focused_edge)
                    };
                    row = row.aria_active_descendant().child(
                        div()
                            .debug_selector(move || format!("resource-row-rail-{position}"))
                            .absolute()
                            .left(RAIL_GUTTER)
                            .top_0()
                            .bottom_0()
                            .w(rail)
                            .bg(rail_color),
                    );
                } else {
                    row = row.hover(move |style| style.bg(hover_background));
                }

                let view = map_view.clone();
                let focus = click_focus.clone();
                let row = row.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    window.focus(&focus, cx);
                    // Shift extends from the anchor, Control or Command toggles.
                    let extend = event.modifiers.shift;
                    let toggle = event.modifiers.control || event.modifiers.platform;
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            if extend {
                                view.extend_selection_to(index, window, cx);
                            } else if toggle {
                                view.toggle_selection_at(index, window, cx);
                            } else {
                                view.select_index(index, window, cx);
                            }
                        });
                    }
                });
                let menu_view = map_view.clone();
                let menu_target = map_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.rows.get(index))
                    .and_then(RowTarget::for_row)
                    .unwrap_or(RowTarget::Position(index));
                right_click_menu::<ContextMenu>(("pod-row-menu", position))
                    .trigger(move |_active, _window, _cx| row)
                    .menu(move |window, cx| match menu_view.upgrade() {
                        Some(view) => {
                            let target = menu_target.clone();
                            view.update(cx, |view, cx| view.row_context_menu(target, window, cx))
                        }
                        None => ContextMenu::build(window, cx, |menu, _, _| menu),
                    })
                    .into_any_element()
            });

        // Render the empty state directly so it receives the full table height.
        let body: AnyElement = if row_count == 0 {
            empty_state(
                EmptyStateContext {
                    status: &empty_status,
                    filter: &empty_filter,
                    snapshot: empty_snapshot.as_deref(),
                    spec: &empty_spec,
                    columns: &empty_columns,
                    visible: &empty_visible,
                    widths: &empty_widths,
                    action_focus: &empty_action_focus,
                    namespace_focus: &empty_namespace_focus,
                    reason_focus: &empty_reason_focus,
                    viewport_height: empty_viewport_height,
                    problems_only: self.problems_only,
                },
                window,
                cx,
            )
        } else {
            table.into_any_element()
        };

        let table_area =
            div()
                .id("resource-grid")
                .debug_selector(|| "resource-grid".to_owned())
                .role(Role::Grid)
                .aria_label(format!("{} Table", self.spec.label))
                .aria_description(TABLE_ACCESSIBILITY_DESCRIPTION)
                .font_ui(cx)
                .text_size(rems_from_px(f32::from(design::text::BODY)))
                .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
                // The header is a row of the grid, so the count includes it.
                .aria_row_count(grid_row_count(row_count))
                .aria_column_count(self.visible_columns.len())
                .relative()
                .size_full()
                .min_h_0()
                .pb(horizontal_scroll_hit_padding())
                // Keep the table after the toolbar in the Tab order.
                .tab_group()
                .tab_index(3)
                .track_focus(&table_focus)
                .key_context(TABLE_CONTEXT)
                .on_action(cx.listener(|view, _: &SelectPrevious, window, cx| {
                    view.move_selection(Move::Up, window, cx)
                }))
                .on_action(cx.listener(|view, _: &SelectNext, window, cx| {
                    view.move_selection(Move::Down, window, cx)
                }))
                .on_action(cx.listener(|view, _: &SelectNextColumn, window, cx| {
                    view.move_column(1, window, cx)
                }))
                .on_action(cx.listener(|view, _: &SelectPreviousColumn, window, cx| {
                    view.move_column(-1, window, cx)
                }))
                .on_action(cx.listener(|view, _: &SortSelectedColumn, _window, cx| {
                    view.sort_selected_column(cx)
                }))
                .on_action(
                    cx.listener(|view, _: &OpenDetails, window, cx| view.open_details(window, cx)),
                )
                .on_action(cx.listener(|view, _: &OpenRowActions, window, cx| {
                    view.open_row_context_menu(None, window, cx)
                }))
                .on_action(cx.listener(|view, _: &DeleteSelection, window, cx| {
                    view.request_delete_confirmation(window, cx)
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _event, window, cx| {
                        let handle = view.table_focus_handle(cx);
                        window.focus(&handle, cx);
                        cx.notify();
                    }),
                )
                .child(body)
                .child(self.column_resize_overlay(&widths, cx))
                .child(self.horizontal_scroll_edge(cx));

        table_area.into_any_element()
    }

    /// Paints one hover affordance per column edge, on top of the table.
    ///
    /// The shared table draws its own resize divider, but its resting rule is
    /// painted unconditionally, so the boundary it marks is always louder than
    /// the row rules and the table reads as a spreadsheet
    /// (`lists-and-tables.md > Desktop (macOS)` wants the rules gone and the
    /// handle kept as a hover affordance). This overlay is that affordance: a
    /// `design::size::HIT_MIN` band centred on the edge, invisible at rest, and
    /// on hover a rule solved to `INTERACTIVE_MIN_CONTRAST` against the surface
    /// the table is painted on. The band never occludes, so the shared
    /// component's own drag handle below it still receives the pointer.
    fn column_resize_overlay(&self, widths: &[Pixels], cx: &App) -> AnyElement {
        let content_width: Pixels = widths
            .iter()
            .copied()
            .fold(px(0.), |sum, width| sum + width);
        let edges: Vec<Pixels> = widths
            .iter()
            .scan(px(0.), |left, width| {
                *left += *width;
                Some(*left)
            })
            .collect();
        let scroll = self
            .interaction
            .read(cx)
            .horizontal_scroll_handle
            .offset()
            .x;
        let rule = design::graphic_on_with_minimum(
            table_row_surface(cx),
            cx.theme().colors().border,
            design::border::INTERACTIVE_MIN_CONTRAST,
        );
        let hit = f32::from(design::size::HIT_MIN);
        let thickness = f32::from(design::border::COLUMN_RULE);
        // The bands sit over the columns, so the whole overlay follows the
        // horizontal scroll: a band left behind on an edge that has scrolled off
        // would light up at the wrong place.
        let mut overlay = div()
            .id("column-resize-affordance")
            .debug_selector(|| "column-resize-affordance".to_owned())
            .absolute()
            .top_0()
            .bottom_0()
            .left(scroll)
            .w(content_width);
        for (position, edge) in edges.into_iter().enumerate() {
            let group: SharedString = format!("column-resize-edge-{position}").into();
            overlay = overlay.child(
                div()
                    .id(("column-resize-edge", position))
                    .debug_selector(move || format!("column-resize-edge-{position}"))
                    // The group is what the rule inside watches, so the rule
                    // lights up across the whole band and not just over the
                    // 1px line.
                    .group(group.clone())
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(edge - px(hit / 2.))
                    .w(design::size::HIT_MIN)
                    // Centre the visible rule on the edge rather than letting
                    // the hit box decide where the line lands.
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(hit / 2. - thickness))
                            .w(design::border::COLUMN_RULE)
                            .bg(rule)
                            .opacity(0.)
                            .group_hover(group, |style| style.opacity(1.)),
                    ),
            );
        }
        overlay.into_any_element()
    }

    /// A soft right edge that appears only while there is more table to the
    /// right.
    ///
    /// The last column used to be cut mid-glyph with no ellipsis, no fade and no
    /// scrollbar, so a clipped cell read as the end of the data.
    /// `scroll-views.md` asks for a signal whenever partial content sits at an
    /// edge; a permanent fade would be that signal for a table that fits, so it
    /// follows the scroll range instead. The edge stops above the scrollbar
    /// strip so it never dims the thumb.
    fn horizontal_scroll_edge(&self, cx: &App) -> AnyElement {
        let handle = self.interaction.read(cx).horizontal_scroll_handle.clone();
        let more_to_the_right =
            has_more_columns_to_the_right(handle.offset().x, handle.max_offset().x);
        let surface = table_row_surface(cx);
        div()
            .id("table-horizontal-edge")
            .debug_selector(|| "table-horizontal-edge".to_owned())
            .absolute()
            .top_0()
            .bottom(horizontal_scroll_hit_padding())
            .right_0()
            .w(design::space::XL)
            .when(more_to_the_right, |edge| {
                // 90 degrees runs left to right in the CSS convention the
                // shared gradient uses, so the table is solid and the clipped
                // edge fades into the surface behind it.
                edge.bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(surface.opacity(0.0), 0.),
                    gpui::linear_color_stop(surface, 1.),
                ))
            })
            .into_any_element()
    }

    /// Returns visible column widths in display order.
    fn visible_widths(&self, window: &Window, cx: &App) -> Vec<Pixels> {
        let rem_size = window.rem_size();
        let state = self.columns_state.read(cx);
        // One width per column. The extra trailing slot the Actions column used
        // to occupy is gone, and asking for one more reads past the end.
        (0..self.visible_columns.len())
            .map(|position| {
                state.pinned_width(position + 1, rem_size) - state.pinned_width(position, rem_size)
            })
            .collect()
    }

    fn header_cells(
        &self,
        sort: Option<Sort>,
        relevance: bool,
        table_focused: bool,
        _widths: &[Pixels],
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors();
        // The shared table header pads its cells by 4px, so the cell keeps the
        // band on the 28px row rhythm with a 20px hit target.
        let header_height = header_cell_height();
        let problems_column = self.problems_filter_column();
        // The hover underline is an interactive boundary, so it holds the
        // interactive floor against the band the header sits on, which is the
        // table's own surface.
        let hover_rule = design::graphic_on_with_minimum(
            design::surface::input(cx),
            colors.border,
            design::border::INTERACTIVE_MIN_CONTRAST,
        );
        self.visible_columns
            .iter()
            .enumerate()
            .map(|(position, &index)| {
                let column = &self.columns[index];
                let group: SharedString = format!("pod-header-{index}").into();
                let affordance = sort_affordance_with_relevance(sort, index, relevance);
                let indicator = sort_indicator(affordance);
                let sorted = indicator.is_shown();
                let selected = index == self.selected_column;
                let title = div()
                    .when(sorted, |title| {
                        title.debug_selector(|| "pod-header-sorted-label".to_owned())
                    })
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_ui(cx)
                    .text_size(rems_from_px(f32::from(design::text::METADATA)))
                    .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(header_label_color(sorted || selected, cx))
                    .child(column.title)
                    .into_any_element();
                let description = header_accessibility_description(column.title, affordance);
                let label = header_accessibility_label(column.title, affordance);
                let cell = div()
                    .id(("pod-column", index))
                    .role(Role::ColumnHeader)
                    .aria_label(label)
                    .aria_description(description.clone())
                    .aria_keyshortcuts(COLUMN_KEYS)
                    .aria_column_index(position + 1)
                    .group(group.clone())
                    // The cell fills the column the shared header already padded,
                    // so the label lines up with the data cell text.
                    .w_full()
                    .h(header_height)
                    .min_w_0()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(design::space::XS)
                    .cursor_pointer()
                    // Match the first data cell so the column label and its
                    // values share one leading edge across the rail gutter.
                    .when(position == 0, |cell| cell.pl(first_cell_leading_pad()))
                    .when(selected, |cell| {
                        cell.child(column_focus_rail(index, table_focused, cx))
                    })
                    // A 1px underline marks the column under the pointer without
                    // painting a slab over the header band.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(design::border::COLUMN_RULE)
                            .bg(hover_rule)
                            .opacity(0.)
                            .group_hover(group.clone(), |style| style.opacity(1.)),
                    )
                    .tooltip(table_tooltip(description, None))
                    .on_click(cx.listener(move |view, _event: &ClickEvent, window, cx| {
                        view.set_selected_column(index, window, cx);
                        let current = if relevance { None } else { sort };
                        let next = next_sort(current, index, view.default_sort_column());
                        view.apply_sort(next, cx);
                    }));
                // Header alignment follows the data: numbers right, text left. The
                // glyph follows the label in both, so a right-aligned number reads
                // `Ready ⌃` at its right edge and a text column reads `Status ⌄`;
                // one header cannot use one order for numbers and another for text
                // (`layout.md > Visual hierarchy`, on aligning parts so the eye can
                // scan them).
                let cell = cell
                    .when(cell_is_right_aligned(column), |cell| cell.justify_end())
                    .child(title)
                    .child(indicator_element(indicator, index));
                // The Status column offers the filter down to rows that need attention.
                let cell = if problems_column == Some(index) {
                    cell.child(self.problems_filter_control(header_height, window, cx))
                } else {
                    cell
                };
                // Right-click opens visibility and width actions.
                let menu_view = cx.entity().downgrade();
                right_click_menu::<ContextMenu>(("pod-header-menu", index))
                    .trigger(move |_active, _window, _cx| cell)
                    .menu(move |window, cx| match menu_view.upgrade() {
                        Some(view) => {
                            view.update(cx, |view, cx| view.column_context_menu(index, window, cx))
                        }
                        None => ContextMenu::build(window, cx, |menu, _, _| menu),
                    })
                    .into_any_element()
            })
            .collect::<Vec<_>>()
    }

    /// Returns the status column the problems filter reads, when it is visible.
    fn problems_filter_column(&self) -> Option<usize> {
        let index = self.status_column_index()?;
        self.visible_columns.contains(&index).then_some(index)
    }

    /// Returns the status column whether or not it is on screen.
    ///
    /// A row carries a cell for every column, so the filter can read a status the
    /// reader has hidden. Requiring the column to be visible made the filter stop
    /// working the moment it was hidden, which is how a filter that looks on
    /// silently became no filter at all.
    fn status_column_index(&self) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.column.id == "status")
    }

    /// Returns the snapshot rows the table shows, in display order. The problems
    /// filter only changes which rows are listed, so the host snapshot, the
    /// selection, and every row action keep using the same row indexes.
    fn shown_rows(&self, snapshot: &IndexSnapshot) -> Vec<usize> {
        let all = || (0..snapshot.rows.len()).collect::<Vec<_>>();
        if !self.problems_only {
            return all();
        }
        let Some(status) = self.status_column_index() else {
            return all();
        };
        (0..snapshot.rows.len())
            .filter(|&index| {
                snapshot.rows[index]
                    .cells
                    .get(status)
                    .is_some_and(|cell| !matches!(status_severity(&cell.text), Severity::Success))
            })
            .collect()
    }

    /// Returns the list position of a snapshot row, which the scrollbar needs
    /// once the problems filter hides rows.
    fn shown_position(&self, snapshot: &IndexSnapshot, index: usize) -> usize {
        if !self.problems_only {
            return index;
        }
        self.shown_rows(snapshot)
            .iter()
            .position(|&shown| shown == index)
            .unwrap_or(0)
    }

    /// Shows or hides the rows that need attention.
    ///
    /// The Overview counts a cluster's problem rows and then has to route to
    /// them, so the filter is part of the view's contract rather than a private
    /// detail of the Status header. The field has one write path so the header
    /// control, the column menu, `Clear Filter` and a caller outside this module
    /// cannot disagree about whether it is on.
    ///
    /// The filter reads each row's status, which a row carries whether or not the
    /// Status column is on screen, so a hidden column does not turn it into a
    /// no-op. It does hide the control that turns it off, so a caller that sets
    /// the filter while the column is hidden owns the way back.
    pub fn set_problems_only(&mut self, problems_only: bool, cx: &mut Context<Self>) {
        self.apply_problems_only(problems_only, cx);
    }

    /// The one place `problems_only` changes.
    fn apply_problems_only(&mut self, problems_only: bool, cx: &mut Context<Self>) {
        if self.problems_only == problems_only {
            return;
        }
        self.problems_only = problems_only;
        cx.notify();
    }

    /// Hides or shows the rows whose status is healthy.
    fn toggle_problems_filter(&mut self, cx: &mut Context<Self>) {
        self.apply_problems_only(!self.problems_only, cx);
    }

    /// Builds the Status header control that hides healthy rows. The column
    /// header click sorts, so the control keeps its own click and its own place
    /// in the Tab order.
    ///
    /// The control was mouse-only: no `FocusHandle`, no hover, no focus ring,
    /// and a 20x22px hit box, which is `design::size::HIT_MIN` with zero margin.
    /// A click one pixel off sorted the column instead of filtering it, and the
    /// only keyboard route was `Show Only Problems` buried in the column menu.
    /// Enter and Space arrive as the `OpenDetails` action, so `open_details`
    /// claims them while this control holds the focus.
    fn problems_filter_control(
        &self,
        height: Pixels,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let active = self.problems_only;
        let description = if active {
            "Showing rows that need attention. Show every row."
        } else {
            "Show only rows that need attention."
        };
        let focus = self.problems_filter_focus.clone();
        let focused = focus.is_focused(window);
        let colors = cx.theme().colors();
        div()
            .id("status-filter")
            .debug_selector(|| "status-filter".to_owned())
            .role(Role::Button)
            .aria_label(description)
            .aria_description("Column Options has the same filter.")
            .track_focus(&focus)
            // The control sits inside the table's own focus group, so this is
            // the stop a reader reaches after the table itself.
            .tab_group()
            .tab_index(0isize)
            .h(height)
            // A square hit target, so a click that lands off the glyph is still
            // a click on the control.
            .w(design::size::HIT_MIN)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .when(active || focused, |control| {
                control.bg(colors.element_hover)
            })
            // The box is a fixed size, so a focus border cannot shift what is
            // inside it the way it would on a row.
            .when(focused, |control| {
                control
                    .border_1()
                    .border_color(colors.border_focused)
                    .rounded(px(4.0))
            })
            .hover(|style| style.bg(colors.element_hover))
            .tooltip(Tooltip::text(description))
            .on_click(cx.listener(|view, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                view.toggle_problems_filter(cx);
            }))
            .child(
                Icon::new(design::problems_filter_icon(active))
                    .size(IconSize::Custom(rems_from_px(f32::from(
                        design::size::STATUS_MARKER,
                    ))))
                    .color(if active { Color::Accent } else { Color::Muted }),
            )
            .into_any_element()
    }

    /// Builds a row menu with only available actions. The menu binds to the
    /// row behind the target, never to a position that a rebuild can reuse.
    fn row_context_menu(
        &mut self,
        target: RowTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        // The row left the snapshot, so the menu must not fall back to a neighbour.
        let Some(index) = self.resolve_row_target(Some(target), cx) else {
            return ContextMenu::build(window, cx, |menu, _, _| menu);
        };
        self.select_index(index, window, cx);
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return ContextMenu::build(window, cx, |menu, _, _| menu);
        };
        let Some(row) = snapshot.rows.get(index).cloned() else {
            return ContextMenu::build(window, cx, |menu, _, _| menu);
        };
        let name: SharedString = row
            .obj
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "resource".to_owned())
            .into();
        let describe_row = row.clone();
        let object = Arc::clone(&row.obj);
        let object_ref = self.object_ref(&object);
        let log_request = self.log_request_for_object(&object);
        let exec_target = self.exec_target_for_object(&object);
        let forward_target = self.port_forward_target_for_object(&object);
        let service_account_target = self
            .service_account_target_for_object(&object)
            .and_then(Result::ok);
        let logs_handler = self.on_logs.clone();
        let exec_handler = self.on_exec_requested.clone();
        let forward_handler = self.on_forward_requested.clone();
        let service_account_handler = self.on_service_account_requested.clone();
        let ops_view = cx.weak_entity();
        let ops_available = self.ops.is_some();
        let can_restart = matches!(
            self.spec.kind.as_ref(),
            "Deployment" | "StatefulSet" | "DaemonSet"
        ) && object_ref.is_some();
        let scale_target = self.scale_target_for_object(&object);
        let can_scale = scale_target.is_some();
        let delete_target = object_ref.clone().map(|object| DeleteTarget { object });
        let delete_available = ops_available && delete_target.is_some();
        ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = menu.item(
                ContextMenuEntry::new("Copy Name")
                    .icon(IconName::Copy)
                    .handler({
                        let name = name.clone();
                        move |_window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
                        }
                    }),
            );
            // Single click only selects now, and the trailing ellipsis button is
            // gone, so the row menu is the one place a pointer reaches the
            // preview. It sits first because it is what most readers want from a
            // row they just right-clicked. A second `Describe` entry used to
            // carry a byte-identical handler: two labels, two icons, one
            // behaviour.
            let menu = menu.item(
                ContextMenuEntry::new("Open Details")
                    .icon(IconName::ChevronRight)
                    .handler({
                        let view = ops_view.clone();
                        let row = describe_row.clone();
                        move |window, cx| {
                            view.update(cx, |view, cx| view.activate_row(row.clone(), window, cx))
                                .ok();
                        }
                    }),
            );
            let menu = match (
                service_account_handler.as_ref(),
                service_account_target.clone(),
            ) {
                (Some(handler), Some(target)) => {
                    let handler = handler.clone();
                    menu.item(
                        ContextMenuEntry::new("Open Service Account")
                            .icon(IconName::UserCheck)
                            .handler(move |window, cx| {
                                handler(target.clone(), window, cx);
                            }),
                    )
                }
                _ => menu,
            };
            let menu = match (&logs_handler, &log_request) {
                (Some(handler), Some(request)) => {
                    let handler = handler.clone();
                    let request = request.clone();
                    menu.item(
                        ContextMenuEntry::new("Logs")
                            .icon(IconName::Reader)
                            .handler(move |window, cx| handler(&request, window, cx)),
                    )
                }
                _ => menu,
            };
            let menu = if let (Some(handler), Some(target)) =
                (exec_handler.as_ref(), exec_target.clone())
            {
                let view = ops_view.clone();
                let _ = handler;
                menu.item(
                    ContextMenuEntry::new("Exec")
                        .icon(IconName::Terminal)
                        .handler(move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.request_exec_target(target.clone(), window, cx)
                            })
                            .ok();
                        }),
                )
            } else {
                menu
            };
            let menu = if let (Some(handler), Some(target)) =
                (forward_handler.as_ref(), forward_target.clone())
            {
                let view = ops_view.clone();
                let _ = handler;
                menu.item(
                    ContextMenuEntry::new(START_PORT_FORWARD)
                        .icon(IconName::ArrowRight)
                        .handler(move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.request_port_forward_target(target.clone(), window, cx)
                            })
                            .ok();
                        }),
                )
            } else {
                menu
            };
            if !ops_available {
                return menu;
            }
            let menu = if can_restart {
                let view = ops_view.clone();
                let target = object_ref.clone();
                menu.item(
                    ContextMenuEntry::new("Restart")
                        .icon(IconName::RotateCw)
                        .handler(move |_, cx| {
                            if let Some(target) = target.clone() {
                                view.update(cx, |view, cx| view.request_restart_target(target, cx))
                                    .ok();
                            }
                        }),
                )
            } else {
                menu
            };
            let menu = if can_scale {
                let view = ops_view.clone();
                menu.item(
                    ContextMenuEntry::new("Scale")
                        .icon(IconName::ArrowRightLeft)
                        .handler(move |window, cx| {
                            if let Some(target) = scale_target.clone() {
                                view.update(cx, |view, cx| {
                                    view.request_scale_target_dialog(target.clone(), window, cx)
                                })
                                .ok();
                            }
                        }),
                )
            } else {
                menu
            };
            if !delete_available {
                return menu;
            }
            let view = ops_view.clone();
            menu.custom_entry(
                move |_window, cx| {
                    let marker = Severity::Error
                        .marker_on(cx, cx.theme().colors().elevated_surface_background);
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Trash)
                                .size(IconSize::Small)
                                .color(Color::Custom(marker)),
                        )
                        .child(Label::new("Delete").color(Color::Custom(marker)).truncate())
                        .into_any_element()
                },
                move |window, cx| {
                    if let Some(target) = delete_target.clone() {
                        view.update(cx, |view, cx| {
                            view.request_delete_target_confirmation(target.clone(), window, cx)
                        })
                        .ok();
                    }
                },
            )
        })
    }

    fn error_panel(&self, reason: &str, window: &Window, cx: &Context<Self>) -> AnyElement {
        let retry_focus = self.retry_focus.clone();
        let retry_tooltip = action_tooltip(RETRY_LOADING_RESOURCES, &Refresh, cx);
        let plural = self.spec.label_lower();
        let reason_focus = self.error_reason_focus.clone();
        // The reason is readable on screen; the raw text stays in the tooltip.
        let visible_reason = user_reason(reason, &plural);
        let reason_focused = reason_focus.is_focused(window);
        let mut panel = v_flex()
            .id("pods-error")
            .debug_selector(|| "pods-error".to_owned())
            .role(Role::Alert)
            .aria_label(format!("Loading {plural} failed"))
            .aria_description(RETRY_LOADING_RESOURCES_GUIDANCE)
            .size_full()
            .items_center()
            .justify_center()
            .gap(design::space::SM);
        panel
            .interactivity()
            .tooltip(Tooltip::text(reason.to_owned()));
        panel
            .child(
                div()
                    .size(px(40.0))
                    .rounded_full()
                    .bg(cx.theme().status().error_background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Custom(
                                design::Severity::Error
                                    .marker_on(cx, cx.theme().status().error_background),
                            )),
                    ),
            )
            .child(
                body_label(format!("Loading {plural} failed")).color(Color::Custom(
                    // The failure panel is drawn inside the table area, so the
                    // text has to clear the floor against the table's own
                    // surface rather than the canvas behind it.
                    design::Severity::Error.marker_on(cx, design::surface::input(cx)),
                )),
            )
            // One line that says what went wrong, reachable by keyboard.
            .child(
                div()
                    .id("pods-error-reason")
                    .debug_selector(|| "pods-error-reason".to_owned())
                    .track_focus(&reason_focus)
                    .tab_index(0isize)
                    .role(Role::Note)
                    .aria_label(visible_reason.clone())
                    .aria_description(reason.to_owned())
                    .max_w(px(480.0))
                    .text_center()
                    .child(
                        metadata_label(visible_reason.clone()).color(Color::Custom(
                            design::Severity::Error
                                .marker_on(cx, design::surface::input(cx))
                                .opacity(0.9),
                        )),
                    )
                    .when(reason_focused, |this| {
                        this.border_1()
                            .border_color(cx.theme().colors().border_focused)
                            .rounded(px(4.0))
                    }),
            )
            .child(metadata_label(RETRY_LOADING_RESOURCES_GUIDANCE).color(Color::Muted))
            // Retry receives focus when the table enters its error state.
            .child(
                Button::new("retry", RETRY_LOADING_RESOURCES)
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .track_focus(&retry_focus)
                    .tab_index(0isize)
                    .tooltip(Tooltip::text(retry_tooltip))
                    .on_click(cx.listener(move |_view, _event: &ClickEvent, window, cx| {
                        window.dispatch_action(Box::new(Refresh), cx);
                    })),
            )
            .into_any_element()
    }
}

impl Render for PodsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, snapshot, sort, total) = {
            let host = self.host.read(cx);
            (
                host.status(),
                host.snapshot(),
                host.sort(),
                host.total_count(),
            )
        };
        let table_focused = self.table_focus_handle(cx).contains_focused(window, cx);
        // A session that is still loading kubeconfig must not steal focus for a
        // Retry button that a later failure may replace.
        // Only a focus that sits on the view itself or on the table moves to
        // Retry. `contains_focused` would also match the Retry button itself,
        // and every later render would pull the focus back to it, so the user
        // could never Tab past the error.
        if matches!(status, TableStatus::Failed(_))
            && !self.host.read(cx).is_startup_loading()
            && !self.retry_focus.is_focused(window)
            && (self.focus_handle.is_focused(window) || table_focused)
        {
            let retry_focus = self.retry_focus.clone();
            window.focus(&retry_focus, cx);
        }
        if self.restore_table_focus {
            self.restore_table_focus = false;
            let focus = self.table_focus_handle(cx);
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        }
        // The toolbar counts what the table *lists*, not what the snapshot holds.
        // The problems filter narrows the body without narrowing the snapshot, so
        // counting the snapshot put `pods 10,101` above a table listing 9,900 --
        // the one number a reader checks their list against did not describe the
        // list. `shown_rows` is pure and `&self`, so it costs one pass, and
        // `self.table` below reuses this rather than recomputing.
        let shown = Arc::new(
            snapshot
                .as_deref()
                .map(|snapshot| self.shown_rows(snapshot))
                .unwrap_or_default(),
        );
        let toolbar = self.toolbar(&status, shown.len(), total, window, cx);
        let body = match &status {
            TableStatus::Failed(reason) => self.error_panel(reason, window, cx),
            _ => self.table(snapshot, shown, sort, &status, window, cx),
        };

        let multi_delete_confirm_focus = self.multi_delete_confirm_focus.clone();
        let multi_delete_cancel_focus = self.multi_delete_cancel_focus.clone();
        let multi_delete_view = cx.entity().downgrade();
        let multi_delete = self.multi_delete.clone().map(|request| {
            multi_delete_bar(
                &request,
                multi_delete_view,
                multi_delete_confirm_focus,
                multi_delete_cancel_focus,
                cx,
            )
        });

        let notice = self.notice.clone().map(|notice| {
            let epoch = notice.epoch;
            let notice_focus = self.notice_focus.clone();
            notice_banner(&notice, cx)
                .child(
                    IconButton::new("resource-notice-dismiss", IconName::Close)
                        .size(ButtonSize::Default)
                        .icon_size(IconSize::XSmall)
                        .track_focus(&notice_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text("Dismiss Message"))
                        .aria_label("Dismiss Message")
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.dismiss_notice(epoch, window, cx)
                        })),
                )
                .into_any_element()
        });
        let context_menu = self.context_menu.as_ref().map(|menu| {
            let position = self.context_menu_position;
            let menu = menu.clone();
            gpui::deferred(
                gpui::anchored()
                    .position(position)
                    .snap_to_window_with_margin(px(8.0))
                    .child(
                        div()
                            .id("resource-context-menu")
                            .occlude()
                            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                                if view.context_menu.is_none() {
                                    return;
                                }
                                match event.keystroke.key.as_str() {
                                    "escape" => {
                                        view.close_context_menu(window, cx);
                                        cx.stop_propagation();
                                    }
                                    "tab" | "shift-tab" => {
                                        if let Some(menu) = view.context_menu.as_ref() {
                                            let focus = menu.read(cx).focus_handle(cx).clone();
                                            window.focus(&focus, cx);
                                        }
                                        cx.stop_propagation();
                                    }
                                    _ => {}
                                }
                            }))
                            .child(menu),
                    ),
            )
            .with_priority(2)
            .into_any_element()
        });

        v_flex()
            .size_full()
            .bg(design::surface::input(cx))
            .text_color(cx.theme().colors().text)
            .track_focus(&self.focus_handle)
            .key_context(TABLE_CONTEXT)
            .on_action(cx.listener(|view, _: &ToggleUpdates, window, cx| {
                let from_recovery = view.empty_action_focus.is_focused(window)
                    || view.focus_restore_for_recovery(window);
                view.restore_table_focus |= from_recovery;
                view.toggle_updates(cx)
            }))
            .on_action(cx.listener(|view, _: &ToggleChurn, _window, cx| view.toggle_churn(cx)))
            .on_action(
                cx.listener(|view, _: &FocusFilter, window, cx| view.focus_filter(window, cx)),
            )
            .on_action(cx.listener(|view, _: &ClearFilter, window, cx| {
                // Clearing the filter fills the table again, so the table takes
                // the focus back instead of the filter that held the button.
                view.restore_table_focus |= view.focus_restore_for_recovery(window);
                view.clear_filter(cx)
            }))
            .on_action(cx.listener(|view, _: &Refresh, window, cx| {
                view.restore_table_focus |= view.focus_restore_for_recovery(window);
                view.refresh(cx)
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(toolbar)
            .when_some(multi_delete, |this, bar| this.child(bar))
            .when_some(notice, |this, notice| this.child(notice))
            .when_some(context_menu, |this, menu| this.child(menu))
            .child(div().flex_grow_1().min_h_0().child(body))
    }
}

/// Adds fallback table key bindings when the host has none.
fn install_fallback_keys(cx: &mut Context<PodsView>) {
    let (
        missing_previous,
        missing_next,
        missing_next_column,
        missing_previous_column,
        missing_sort,
        missing_open_enter,
        missing_open_space,
        missing_row_actions,
        missing_row_actions_shift,
        missing_row_actions_menu,
        missing_refresh,
        missing_delete,
        missing_churn,
        missing_filter,
    ) = {
        let keymap = cx.key_bindings();
        let bindings = keymap.borrow();
        let table_context = gpui::KeyContext::parse("Table").ok();
        let key_taken = |spec: &str| {
            let (Ok(key), Some(context)) = (gpui::Keystroke::parse(spec), table_context.as_ref())
            else {
                return false;
            };
            let (matches, pending) =
                bindings.bindings_for_input(&[key], std::slice::from_ref(context));
            !matches.is_empty() || pending
        };
        let missing = |action: &dyn gpui::Action, spec: &str| {
            bindings.bindings_for_action(action).next().is_none() && !key_taken(spec)
        };
        (
            missing(&SelectPrevious, "up"),
            missing(&SelectNext, "down"),
            missing(&SelectNextColumn, "tab"),
            missing(&SelectPreviousColumn, "shift-tab"),
            missing(&SortSelectedColumn, "shift-enter"),
            missing(&OpenDetails, "enter"),
            missing(&OpenDetails, "space"),
            missing(&OpenRowActions, "f10"),
            missing(&OpenRowActions, "shift-f10"),
            missing(&OpenRowActions, "menu"),
            missing(&Refresh, "f5"),
            missing(&DeleteSelection, "delete"),
            missing(&ToggleChurn, "secondary-r"),
            missing(&FocusFilter, "secondary-f"),
        )
    };
    let mut bindings = Vec::new();
    if missing_previous {
        bindings.push(KeyBinding::new(
            "up",
            SelectPrevious,
            Some(TABLE_NAV_CONTEXT),
        ));
    }
    if missing_next {
        bindings.push(KeyBinding::new("down", SelectNext, Some(TABLE_NAV_CONTEXT)));
    }
    if missing_next_column {
        bindings.push(KeyBinding::new(
            "tab",
            SelectNextColumn,
            Some(TABLE_CONTEXT),
        ));
    }
    if missing_previous_column {
        bindings.push(KeyBinding::new(
            "shift-tab",
            SelectPreviousColumn,
            Some(TABLE_CONTEXT),
        ));
    }
    if missing_sort {
        bindings.push(KeyBinding::new(
            "shift-enter",
            SortSelectedColumn,
            Some(TABLE_CONTEXT),
        ));
    }
    if missing_open_enter {
        bindings.push(KeyBinding::new("enter", OpenDetails, Some(TABLE_CONTEXT)));
    }
    if missing_open_space {
        bindings.push(KeyBinding::new("space", OpenDetails, Some(TABLE_CONTEXT)));
    }
    if missing_row_actions_shift {
        bindings.push(KeyBinding::new(
            "shift-f10",
            OpenRowActions,
            Some(TABLE_CONTEXT),
        ));
    }
    if missing_row_actions_menu {
        bindings.push(KeyBinding::new("menu", OpenRowActions, Some(TABLE_CONTEXT)));
    }
    // F10 stays last so it remains the shortcut shown in tooltips.
    if missing_row_actions {
        bindings.push(KeyBinding::new("f10", OpenRowActions, Some(TABLE_CONTEXT)));
    }
    if missing_refresh {
        bindings.push(KeyBinding::new("f5", Refresh, Some(TABLE_CONTEXT)));
    }
    if missing_delete {
        bindings.push(KeyBinding::new(
            "delete",
            DeleteSelection,
            Some(TABLE_CONTEXT),
        ));
    }
    if missing_churn {
        bindings.push(KeyBinding::new(
            "secondary-r",
            ToggleChurn,
            Some(TABLE_NAV_CONTEXT),
        ));
    }
    if missing_filter {
        bindings.push(KeyBinding::new(
            "secondary-f",
            FocusFilter,
            Some(TABLE_NAV_CONTEXT),
        ));
    }
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
}

fn body_label(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Custom(rems_from_px(f32::from(
        design::text::BODY,
    ))))
}

fn metadata_label(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Custom(rems_from_px(f32::from(
        design::text::METADATA,
    ))))
}

fn is_compact_width(width: Pixels) -> bool {
    width < COMPACT_WIDTH
}

fn action_tooltip(label: &str, action: &dyn gpui::Action, cx: &App) -> String {
    let binding = cx
        .key_bindings()
        .borrow()
        .bindings_for_action(action)
        .next_back()
        .cloned();
    let Some(binding) = binding else {
        return label.to_owned();
    };
    let keys = binding
        .keystrokes()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{label} ({keys})")
}

fn is_toolbar_activation_key(event: &KeyDownEvent) -> bool {
    matches!(event.keystroke.key.as_str(), "enter" | "space")
        && !event.keystroke.modifiers.alt
        && !event.keystroke.modifiers.platform
        && !event.keystroke.modifiers.control
        && !event.keystroke.modifiers.shift
}

/// F10, Shift+F10, and the Menu key open the row or column menu.
fn is_row_actions_key(keystroke: &gpui::Keystroke) -> bool {
    if keystroke.modifiers.alt || keystroke.modifiers.platform || keystroke.modifiers.control {
        return false;
    }
    match keystroke.key.as_str() {
        "f10" => true,
        "menu" => !keystroke.modifiers.shift,
        _ => false,
    }
}

fn format_duration(duration: Option<Duration>) -> String {
    match duration {
        Some(duration) if duration.as_secs() >= 1 => {
            format!("{:.1}s", duration.as_secs_f64())
        }
        Some(duration) => format!("{}ms", duration.as_millis()),
        None => "Not available".to_owned(),
    }
}

/// Shows one labelled toolbar value with the shared chip treatment.
fn toolbar_chip(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let label = label.into();
    let description = description.into();
    // The role and the tooltip both live on the interactive element API, so the
    // chip needs an id before the fluent chain can reach them.
    div()
        .id((ElementId::from("table-toolbar-chip"), description.clone()))
        .debug_selector({
            // One selector per chip, so a test can ask for the count chip and not
            // the sort chip beside it.
            let label = label.clone();
            move || format!("toolbar-chip-{label}")
        })
        .flex_none()
        .px(design::space::SM)
        .py_0p5()
        .rounded_sm()
        // Toolbar chips stay opaque so no row shows through them.
        .bg(cx.theme().colors().element_background.alpha(1.))
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .role(Role::Status)
        .aria_label(description.clone())
        .tooltip(Tooltip::text(description))
        .child(metadata_label(label).color(Color::Muted))
        .into_any_element()
}

/// Shows a measured duration with a unit.
fn measure_chip(label: &str, duration: Duration, cx: &App) -> AnyElement {
    let value = format_duration(Some(duration));
    let description = format!("{label}: {value}. Latest measurement from the Kubernetes API.");
    let mut chip = div()
        .id("table-measure-chip")
        .px(design::space::SM)
        .py_0p5()
        .rounded_sm()
        .bg(cx.theme().colors().element_background)
        .text_color(cx.theme().colors().text)
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .role(Role::Status)
        .aria_label(description.clone());
    chip.interactivity().tooltip(Tooltip::text(description));
    chip.child(SharedString::from(format!("{label} {value}")))
        .into_any_element()
}

/// Shows that rows came from the disk cache.
fn cached_chip(cached: CachedRows, cx: &App) -> AnyElement {
    let severity = if cached.stale {
        Severity::Warning
    } else {
        Severity::Muted
    };
    let age = cached
        .saved_at
        .elapsed()
        .map(format_age)
        .unwrap_or_else(|_| "an unknown time".to_owned());
    let label = if cached.stale {
        "Stale Cache"
    } else {
        "Cached"
    };
    let description = format!(
        "Showing cached data from {age}. Live data replaces it when the initial data load finishes."
    );
    let mut chip = h_flex()
        .id("table-cache-chip")
        .role(Role::Status)
        .aria_label(description.clone())
        .gap(design::space::XS)
        .items_center()
        .px(design::space::SM)
        .py_0p5()
        .rounded_sm()
        .bg(cx.theme().colors().element_background)
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)));
    chip.interactivity().tooltip(Tooltip::text(description));
    chip.child(
        Icon::new(if cached.stale {
            design::health_icon(Severity::Warning)
        } else {
            design::health_icon(Severity::Muted)
        })
        .size(IconSize::Custom(rems_from_px(f32::from(
            design::size::STATUS_MARKER,
        ))))
        .color(Color::Custom(
            severity.marker_on(cx, cx.theme().colors().element_background),
        )),
    )
    .child(
        div()
            .text_color(cx.theme().colors().text_muted)
            .child(SharedString::from(label)),
    )
    .into_any_element()
}

/// Formats a cache age for a tooltip.
fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        "less than a minute ago".to_owned()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Renders a non-blocking notice.
fn notice_banner(notice: &Notice, cx: &App) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    // One fill per severity, and it comes from `design`. The local map this
    // replaced had a catch-all, so a `Success` notice and an `Info` notice were
    // the same colour: one colour meaning two things, at the one place a reader
    // is being told the difference between "fine" and "informational".
    let background = notice.severity.wash(cx);
    let role = if notice.severity == Severity::Error {
        Role::Alert
    } else {
        Role::Status
    };
    let mut banner = h_flex()
        .id(ElementId::NamedInteger(
            "resource-notice".into(),
            notice.epoch,
        ))
        .role(role)
        .aria_label(notice.message.clone())
        .w_full()
        .flex_none()
        .px(design::space::MD)
        .py(design::space::XS)
        .gap(design::space::SM)
        .items_center()
        .bg(background)
        .text_color(colors.text)
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)));
    if let Some(detail) = notice.detail.clone() {
        banner = banner.aria_description(detail);
    }
    let tooltip = notice
        .detail
        .clone()
        .unwrap_or_else(|| SharedString::from("Dismiss Message"));
    banner.interactivity().tooltip(Tooltip::text(tooltip));
    banner
        .child(
            Icon::new(design::health_icon(notice.severity))
                .size(IconSize::Custom(rems_from_px(f32::from(
                    design::size::STATUS_MARKER,
                ))))
                .color(Color::Custom(notice.severity.marker_on(cx, background))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .child(notice.message.clone()),
        )
}

/// Names the objects a multi-object delete covers in one line.
fn delete_summary(objects: &[ObjectRef]) -> String {
    const MAX_NAMES: usize = 3;
    let names: Vec<&str> = objects.iter().map(|object| object.name.as_str()).collect();
    let listed = names
        .iter()
        .take(MAX_NAMES)
        .copied()
        .collect::<Vec<&str>>()
        .join(", ");
    if names.len() > MAX_NAMES {
        format!("{listed} and {} more", names.len() - MAX_NAMES)
    } else {
        listed
    }
}

/// Renders the confirmation step for a multi-object delete.
fn multi_delete_bar(
    request: &MultiDeleteRequest,
    view: WeakEntity<PodsView>,
    confirm_focus: FocusHandle,
    cancel_focus: FocusHandle,
    cx: &App,
) -> AnyElement {
    let count = request.objects.len();
    let message = format!("Delete {count} selected objects?");
    h_flex()
        .id("multi-delete-bar")
        .debug_selector(|| "multi-delete-bar".to_owned())
        .role(Role::AlertDialog)
        .aria_label(message.clone())
        .aria_description(request.label.clone())
        .w_full()
        .flex_none()
        .px(design::space::SM)
        .py(design::space::XS)
        .gap(design::space::SM)
        .items_center()
        .bg(cx.theme().status().error_background)
        .text_color(cx.theme().colors().text)
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            // The one severity-coloured mark in the bar, so it takes the status
            // marker size the rest of the table uses instead of sitting at 12px
            // beside 13px body text.
            div()
                .id("multi-delete-marker")
                .debug_selector(|| "multi-delete-marker".to_owned())
                .flex_none()
                .flex()
                .items_center()
                .w(design::size::STATUS_MARKER)
                .child(
                    Icon::new(IconName::Trash)
                        .size(IconSize::Custom(rems_from_px(f32::from(
                            design::size::STATUS_MARKER,
                        ))))
                        .color(Color::Custom(
                            Severity::Error.marker_on(cx, cx.theme().status().error_background),
                        )),
                ),
        )
        .child(div().flex_1().min_w_0().whitespace_normal().child(message))
        .child({
            let cancel_view = view.clone();
            let confirm_view = view;
            h_flex()
                .flex_none()
                .child(
                    Button::new("multi-delete-cancel", "Cancel")
                        .size(ButtonSize::Medium)
                        .track_focus(&cancel_focus)
                        .tab_index(1isize)
                        .on_click(move |_, window, cx| {
                            if let Some(view) = cancel_view.upgrade() {
                                view.update(cx, |view, cx| view.cancel_multi_delete(window, cx));
                            }
                        }),
                )
                .child(
                    Button::new("multi-delete-confirm", "Delete")
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .size(ButtonSize::Medium)
                        .track_focus(&confirm_focus)
                        .tab_index(0isize)
                        .on_click(move |_, window, cx| {
                            if let Some(view) = confirm_view.upgrade() {
                                view.update(cx, |view, cx| view.confirm_multi_delete(window, cx));
                            }
                        }),
                )
        })
        .into_any_element()
}

/// Reports that a row waits for the server, and for how much longer.
#[derive(Clone, Debug)]
struct PendingState {
    op: PendingOp,
    remaining: Duration,
}

impl PendingState {
    /// Explains the wait, including the deadline, in one sentence.
    fn detail(&self) -> String {
        let seconds = self.remaining.as_secs();
        if seconds == 0 {
            format!(
                "{} The deadline has passed, so the table is about to report an unknown result.",
                self.op.detail()
            )
        } else {
            format!(
                "{} The table stops waiting in {} seconds.",
                self.op.detail(),
                seconds
            )
        }
    }
}

/// Shows a local pending-operation badge with the time left before the
/// deadline, so a slow request does not look like a stuck one.
fn pending_badge(pending: &PendingState, row_index: usize, cx: &App) -> AnyElement {
    let op = &pending.op;
    let palette = cx.theme().status();
    let background = match op {
        PendingOp::Delete => palette.warning_background,
        PendingOp::Scale { .. } | PendingOp::Restart => palette.info_background,
    };
    let severity = match op {
        PendingOp::Delete => Severity::Warning,
        PendingOp::Scale { .. } | PendingOp::Restart => Severity::Info,
    };
    let seconds = pending.remaining.as_secs();
    let detail = pending.detail();
    let mut badge = h_flex()
        .id(ElementId::NamedInteger(
            "pending-badge".into(),
            row_index as u64,
        ))
        .aria_label(detail.clone())
        .gap(design::space::XS)
        .items_center()
        .px(design::space::XS)
        .rounded_sm()
        .bg(background)
        .text_color(cx.theme().colors().text)
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .whitespace_nowrap();
    badge.interactivity().tooltip(Tooltip::text(detail));
    badge
        .child(
            Icon::new(design::health_icon(severity))
                .size(IconSize::Custom(rems_from_px(f32::from(
                    design::size::STATUS_MARKER,
                ))))
                .color(Color::Custom(severity.marker_on(cx, background))),
        )
        .child(SharedString::from(op.label()))
        .when(seconds > 0, |badge| {
            badge.child(
                div()
                    .text_color(cx.theme().colors().text_muted)
                    .child(SharedString::from(format!("{seconds}s"))),
            )
        })
        .into_any_element()
}

fn sort_mode_label(
    relevance: bool,
    sort: Option<Sort>,
    columns: &[ResourceColumn],
    filter: &str,
) -> String {
    if relevance && !filter.is_empty() {
        return "Relevance".to_owned();
    }
    let Some(sort) = sort else {
        return String::new();
    };
    let title = columns
        .get(sort.column)
        .map(|column| column.title)
        .unwrap_or("Column");
    format!("{title} {}", if sort.descending { "↓" } else { "↑" })
}

fn sort_affordance_with_relevance(
    current: Option<Sort>,
    index: usize,
    relevance: bool,
) -> SortAffordance {
    if relevance {
        return SortAffordance::Relevance;
    }
    match current {
        Some(sort) if sort.column == index && sort.descending => SortAffordance::Descending,
        Some(sort) if sort.column == index => SortAffordance::Ascending,
        _ => SortAffordance::Unsorted,
    }
}

fn sort_description(affordance: SortAffordance) -> &'static str {
    match affordance {
        SortAffordance::Ascending => "Sorted ascending from low to high.",
        SortAffordance::Descending => "Sorted descending from high to low.",
        SortAffordance::Relevance => "Sorted by filter relevance.",
        SortAffordance::Unsorted => "Not sorted.",
    }
}

/// The sort glyph a column header draws, and when it is on screen.
///
/// Only the sorted column carries a direction. A permanent glyph on every
/// header says "sortable" once per column and reads as noise, so the rest keep
/// theirs in reserve until the pointer arrives, which is also the only moment a
/// reader can act on it. Relevance orders the whole table, so it belongs to no
/// single column and never appears as one column's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortIndicator {
    /// Stays next to the label for as long as the header is on screen.
    Shown(IconName),
    /// Stays hidden until the pointer is over the header.
    OnHover(IconName),
}

impl SortIndicator {
    /// Reports whether the glyph is on screen without a pointer.
    fn is_shown(self) -> bool {
        matches!(self, Self::Shown(_))
    }

    fn icon(self) -> IconName {
        match self {
            Self::Shown(icon) | Self::OnHover(icon) => icon,
        }
    }
}

fn sort_indicator(affordance: SortAffordance) -> SortIndicator {
    match affordance {
        SortAffordance::Ascending => SortIndicator::Shown(IconName::ArrowUp),
        SortAffordance::Descending => SortIndicator::Shown(IconName::ArrowDown),
        SortAffordance::Relevance | SortAffordance::Unsorted => {
            SortIndicator::OnHover(IconName::ChevronUpDown)
        }
    }
}

/// Builds the header's sort glyph. The reserved one takes no room until the
/// header is hovered, so the resting row of labels does not shift when it
/// appears.
fn indicator_element(indicator: SortIndicator, index: usize) -> AnyElement {
    let shown = indicator.is_shown();
    let color = if shown { Color::Accent } else { Color::Muted };
    let icon = Icon::new(indicator.icon())
        .size(IconSize::XSmall)
        .color(color);
    let mut glyph = div().flex_none();
    match indicator {
        SortIndicator::Shown(_) => {
            glyph = glyph.debug_selector(|| "pod-header-sorted-glyph".to_owned());
        }
        SortIndicator::OnHover(_) => {
            let group: SharedString = format!("pod-header-{index}").into();
            glyph = glyph.opacity(0.);
            glyph = glyph.group_hover(group, |style| style.opacity(1.));
        }
    }
    glyph.child(icon).into_any_element()
}

/// Maps one status cell to the health channel.
///
/// `Unknown` states no verdict at all: a pod the kubelet lost track of is not a
/// pod that is waiting, and painting the two identically made the app lie in the
/// one case where the reader most needs the difference. `Severity::Neutral` is
/// the shape vocabulary's "no verdict", so it draws a dash and reads as
/// "No verdict".
fn status_severity(status: &str) -> Severity {
    match status {
        "NotReady" | "Degraded" | "Unavailable" => Severity::Warning,
        // `Unknown` is handled by `design::pod_severity`, which owns the split and
        // has a test for it. A local copy of the same rule is a rule that can drift.
        _ => design::pod_severity(status),
    }
}

fn row_accessible_label(
    columns: &[ResourceColumn],
    visible: &[usize],
    cells: &[CellValue],
) -> String {
    let mut label = String::with_capacity(visible.len().saturating_mul(24));
    for &index in visible {
        let Some(cell) = cells.get(index) else {
            continue;
        };
        let text = cell.text.trim();
        if text.is_empty() {
            continue;
        }
        if !label.is_empty() {
            label.push_str(", ");
        }
        label.push_str(columns.get(index).map_or("Value", |column| column.title));
        label.push_str(": ");
        label.push_str(text);
    }
    // The health channel is not a column value, so it is announced even when
    // Status is hidden. Without it a screen reader read "Status: Pending" and
    // never learned that Pending is a warning, because the verdict only ever
    // existed as a glyph.
    let verdict = columns
        .iter()
        .position(|column| column.column.id == "status")
        .and_then(|index| cells.get(index))
        .map(|cell| design::health_label(status_severity(cell.text.trim())));
    match (label.is_empty(), verdict) {
        (false, Some(verdict)) => format!("{label}, Health: {verdict}"),
        (true, Some(verdict)) => format!("Health: {verdict}"),
        (false, None) => label,
        (true, None) => "Resource row".to_owned(),
    }
}

/// Describes how one table row is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowVisual {
    /// Not selected, no stripe.
    Plain,
    /// Not selected, zebra stripe.
    Stripe,
    /// Selected while the table holds keyboard focus.
    SelectedFocused,
    /// Selected after the table lost keyboard focus.
    SelectedUnfocused,
    /// Selected as part of a range, but not the row a command acts on.
    SelectedMember,
    /// The first row carries the focus ring before any selection exists.
    FocusRing,
}

impl RowVisual {
    /// Reports whether the row paints a rail. A range member keeps the fill but
    /// not the rail, so the rail always names one command target.
    fn has_rail(self) -> bool {
        matches!(
            self,
            Self::SelectedFocused | Self::SelectedUnfocused | Self::FocusRing
        )
    }

    /// Reports whether the row keeps the selection fill.
    fn is_selected(self) -> bool {
        matches!(
            self,
            Self::SelectedFocused | Self::SelectedUnfocused | Self::SelectedMember
        )
    }
}

/// Reports whether a row paints the thick table focus rail. Only the active row
/// of a selection does, so the rail always names the row a single-row command
/// such as Scale, Exec or Restart will act on.
fn uses_table_focus_rail(visual: RowVisual) -> bool {
    matches!(visual, RowVisual::SelectedFocused)
}

/// Resolves one row's visual state. A selected row keeps its fill when the
/// table loses focus, but the fill and the rail both step down so the active
/// row stays findable. Only the active row of a multi-row selection keeps the
/// thick table rail, so the rail always means "this is the row a command
/// acts on".
fn row_visual(
    selected: bool,
    anchor: bool,
    table_focused: bool,
    has_selection: bool,
    index: usize,
) -> RowVisual {
    if selected {
        if !table_focused {
            return RowVisual::SelectedUnfocused;
        }
        return if anchor {
            RowVisual::SelectedFocused
        } else {
            RowVisual::SelectedMember
        };
    }
    if table_focused && !has_selection && index == 0 {
        return RowVisual::FocusRing;
    }
    if index % 2 == 1 {
        RowVisual::Stripe
    } else {
        RowVisual::Plain
    }
}

/// Selected row background for a table that lost keyboard focus.
///
/// It composites on the same base as every other row state, so a selected row
/// that lost focus steps down from the focused fill without changing surface.
fn row_selected_muted_bg(cx: &App) -> Hsla {
    // A softer wash than the focused selection, but still a row state, so it
    // clears the same floor. Scaling the alpha by hand put it at 1.188:1 in the
    // light appearance -- under `ROW_STATE_MIN_CONTRAST` -- and it had no
    // contrast assertion at all, which is how that survived. The solver walks the
    // wash away from the base until it clears, so the two states stay distinct by
    // construction rather than by a constant somebody chose once.
    let base = design::surface::input(cx);
    design::graphic_on_with_minimum(
        base,
        base.blend(
            cx.theme()
                .colors()
                .text_accent
                .opacity(design::ROW_SELECTED_ALPHA * 0.55),
        ),
        design::ROW_STATE_MIN_CONTRAST,
    )
}

/// Paints one row's visual state.
///
/// A range member used to take the muted fill, which measured 1.018:1 against
/// the unselected row beside it in dark and 1.022:1 in light: the wash was the
/// only signal a member had, and neither appearance could show it. Members now
/// take the same fill as the active row and stay apart by rail alone, which
/// `uses_table_focus_rail` still reserves for the row a command acts on.
fn row_visual_background(
    visual: RowVisual,
    selected: Hsla,
    selected_muted: Hsla,
    focused: Hsla,
    stripe: Hsla,
    plain: Hsla,
) -> Hsla {
    match visual {
        RowVisual::SelectedFocused | RowVisual::SelectedMember => selected,
        RowVisual::SelectedUnfocused => selected_muted,
        RowVisual::FocusRing => focused,
        RowVisual::Stripe => stripe,
        RowVisual::Plain => plain,
    }
}

/// The surface every row of this table composites onto.
///
/// `DESIGN.md §3.4` files `surface` under tables and inputs. The table used to
/// paint on `canvas`, and the loading skeleton on the canvas too, so the one
/// level of the ramp a reader stares at for eight hours was never on screen. One
/// function for both keeps the two states from drifting apart.
fn table_row_surface(cx: &App) -> Hsla {
    design::surface::input(cx)
}

/// The grid's 1-based row index of a listed row.
///
/// The header is a row of the grid, so the first data row is 2. The loading
/// skeleton and the live table used to number it 1 and 2 respectively, and two
/// states that disagree by one make a screen reader report the wrong row.
fn grid_row_index(position: usize) -> usize {
    position + 2
}

/// The grid's row count: the header row plus the data rows.
///
/// The loading skeleton added the header to its data rows and the live table did
/// not, so the two states announced different table sizes for the same table.
fn grid_row_count(data_rows: usize) -> usize {
    data_rows + 1
}

/// Reports whether the table still has columns to the right of the viewport.
///
/// The first frame has no layout, so a table with no scroll range yet reports no
/// edge and does not dim a column that is not clipped.
fn has_more_columns_to_the_right(offset: Pixels, max_offset: Pixels) -> bool {
    let (offset, max_offset) = (f32::from(offset), f32::from(max_offset));
    max_offset > 0.0 && -offset < max_offset - SCROLL_RANGE_SLACK
}

/// Returns the observation confidence a row carries.
///
/// Freshness belongs to the table, not to one object, so a table that stopped
/// receiving updates marks every row it is still drawing. A row carries no
/// freshness of its own, so the rest comes from the status text the row already
/// shows: a status the API could not resolve states no verdict, and a reader
/// must not read that as health.
fn row_confidence(
    table_is_stale: bool,
    row: &Row,
    columns: &[ResourceColumn],
) -> design::Confidence {
    if table_is_stale {
        return design::Confidence::Stale;
    }
    columns
        .iter()
        .position(|column| column.column.id == "status")
        .and_then(|index| row.cells.get(index))
        .map_or(design::Confidence::Known, |cell| {
            design::confidence_from_text(&cell.text)
        })
}

/// Returns the glyph a row's confidence is marked with, or `None` when the app
/// got an answer. A current row needs no mark: putting one on every row would
/// invent a second thing to read.
fn row_confidence_marker(state: design::Confidence) -> Option<IconName> {
    (!state.is_definite()).then(|| design::confidence::icon(state))
}

/// Resting opacity of a control that lives in a row's trailing cell.
///
/// The cell reserves the width either way, so nothing shifts when the control
/// appears. A control that reports something the app does not know has to be up
/// at rest: it took `selected: bool`, and a marker that means "I never got an
/// answer" was fully transparent on every row that was neither selected nor
/// hovered, which is every row. `Known` still draws nothing at all, so this
/// only ever answers for a marker that is on screen.
fn row_control_reveal(state: design::Confidence) -> f32 {
    if state.is_definite() { 0.0 } else { 1.0 }
}

/// Returns the row height that follows the data font size.
fn row_height(cx: &App) -> Pixels {
    design::row_height(DataTypography::from_theme_settings(cx).line_height)
}

/// Returns how many rows fit in a viewport at the current text size.
fn rows_in_viewport(height: f32, row_height: Pixels) -> usize {
    (height / f32::from(row_height)).floor().max(1.0) as usize
}

/// Cycles column sorting from ascending to descending to the default order.
///
/// The third state is a real sort on the default column instead of a silent
/// "no sorting": an unsorted table still falls back to that order, so claiming
/// "not sorted" while the rows run by Name would be a false affordance.
fn next_sort(current: Option<Sort>, index: usize, default_column: usize) -> Option<Sort> {
    match current {
        Some(sort) if sort.column == index && !sort.descending => Some(Sort::descending(index)),
        Some(sort) if sort.column == index => Some(Sort::ascending(default_column)),
        _ => Some(Sort::ascending(index)),
    }
}

/// Returns the column that provides the default row order.
fn fallback_sort_column(visible: &[usize]) -> usize {
    if visible.contains(&DEFAULT_SORT_COLUMN) {
        DEFAULT_SORT_COLUMN
    } else {
        visible.first().copied().unwrap_or(DEFAULT_SORT_COLUMN)
    }
}

/// Clamps column navigation to the visible range.
fn target_column(current: usize, delta: isize, len: usize) -> usize {
    len.saturating_sub(1)
        .min(current.saturating_add_signed(delta))
}

/// Returns a row target for keyboard navigation.
fn target_index(movement: Move, current: Option<usize>, len: usize, page: usize) -> usize {
    let last = len.saturating_sub(1);
    match movement {
        Move::Home => 0,
        Move::End => last,
        Move::Up => current.map_or(0, |index| index.saturating_sub(1)),
        Move::Down => current.map_or(0, |index| (index + 1).min(last)),
        Move::PageUp => current.map_or(0, |index| index.saturating_sub(page)),
        Move::PageDown => current.map_or(0, |index| (index + page).min(last)),
    }
}

/// Returns the Pod container names used by Logs.
fn pod_containers(object: &DynamicObject) -> Vec<SharedString> {
    object
        .data
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map(|containers| {
            containers
                .iter()
                .filter_map(|container| container.get("name").and_then(Value::as_str))
                .map(SharedString::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Formats the toolbar count while loading and after filtering.
///
/// The count can be two numbers at once, so it cannot be a `count_with_noun`
/// value: a noun attaches to one number. Each half goes through
/// `design::format::count` so the separator reads the same as everywhere else.
fn toolbar_count(listing: bool, filtered: usize, total: usize) -> String {
    if listing {
        format!("{} / loading…", design::format::count(total))
    } else if filtered == total {
        design::format::count(total)
    } else {
        format!(
            "{} / {}",
            design::format::count(filtered),
            design::format::count(total)
        )
    }
}

/// The measured geometry of one cell value, used to size its tooltip.
#[derive(Clone, Debug)]
struct CellShape {
    width: Pixels,
    font: Font,
    features: FontFeatures,
    size: Pixels,
}

/// Returns the text area inside a cell: the column width minus both paddings.
fn cell_text_width(column: Option<&ResourceColumn>, width: Pixels) -> f32 {
    let width = if f32::from(width) > 0.0 {
        f32::from(width)
    } else {
        column.map_or(0.0, |column| column.width)
    };
    // Both sides of the cell are padded.
    (width - 2.0 * f32::from(design::space::SM)).max(0.0)
}

/// Estimates whether cell text needs a full-value tooltip. The estimate is the
/// answer for values that are clearly shorter or clearly longer than the cell.
fn needs_tooltip(
    column: Option<&ResourceColumn>,
    text: &str,
    width: Pixels,
    char_width: f32,
) -> bool {
    if column.is_none() || text.is_empty() || char_width <= 0.0 {
        return false;
    }
    let available = cell_text_width(column, width);
    text.chars().count() as f32 * char_width > available
}

/// Returns how wide the value renders. The cheap estimate decides on its own
/// unless the text sits near the cell edge, and only then is the value shaped,
/// so a clipped cell is judged by the width it really takes.
fn measure_cell_text(
    window: &mut Window,
    text: &str,
    typography: &DataTypography,
    width: Pixels,
    char_width: f32,
) -> Option<Pixels> {
    if text.is_empty() || !char_width.is_finite() || char_width <= 0.0 {
        return None;
    }
    let available = cell_text_width(None, width);
    if available <= 0.0 {
        return None;
    }
    let estimate = text.chars().count() as f32 * char_width;
    let ambiguous =
        estimate > available / TOOLTIP_SHAPE_FACTOR && estimate < available * TOOLTIP_SHAPE_FACTOR;
    if !ambiguous {
        return Some(px(estimate));
    }
    let measured: String = text.chars().take(TOOLTIP_SHAPE_MAX_CHARS).collect();
    let longest = measured
        .split('\n')
        .map(|line| shaped_line_width(window, line, typography))
        .fold(0.0f32, f32::max);
    longest.is_finite().then(|| px(longest))
}

/// Returns the shaped width of one line in the data font.
fn shaped_line_width(window: &mut Window, text: &str, typography: &DataTypography) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = TextRun {
        len: text.len(),
        font: typography.font.clone(),
        color: Hsla::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(text.to_owned()),
        typography.size,
        &[run],
        None,
    );
    f32::from(line.width())
}

/// Decides whether the cell clips its value. A measured width wins over the
/// estimate, so a wide or narrow glyph run is judged by what it renders.
fn cell_text_overflows(
    column: Option<&ResourceColumn>,
    text: &str,
    width: Pixels,
    char_width: f32,
    measured: Option<Pixels>,
) -> bool {
    if column.is_none() || text.is_empty() {
        return false;
    }
    let available = cell_text_width(column, width);
    match measured {
        Some(measured) => f32::from(measured) > available,
        None => needs_tooltip(column, text, width, char_width),
    }
}

/// Turns a raw source reason into one line the user can act on. The raw text
/// stays in the tooltip and in `aria_description`.
fn user_reason(reason: &str, plural: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        return "The table did not report a reason.".to_owned();
    }
    let lower = reason.to_ascii_lowercase();
    if lower.contains("forbidden") {
        return format!("Forbidden: needs list {plural}");
    }
    if lower.contains("unauthorized") || lower.contains("not authorized") {
        return format!("Not authorized: needs access to {plural}");
    }
    if CONNECTION_FAILURES
        .iter()
        .any(|needle| lower.contains(*needle))
    {
        return "Cannot reach context".to_owned();
    }
    if lower.contains("kubeconfig") || lower.contains("no such file") {
        return "No kubeconfig for this context".to_owned();
    }
    first_line(reason, USER_REASON_MAX)
}

/// Keeps one line of raw text within a character budget.
fn first_line(text: &str, limit: usize) -> String {
    let text = text.lines().next().unwrap_or(text).trim();
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn table_status_severity(status: &TableStatus) -> Severity {
    match status {
        TableStatus::Streaming => Severity::Success,
        TableStatus::Listing => Severity::Info,
        TableStatus::Failed(_) => Severity::Error,
        TableStatus::Paused | TableStatus::Stale(_) => Severity::Warning,
        TableStatus::Idle | TableStatus::Stopped => Severity::Muted,
    }
}

fn status_badge(status: &TableStatus, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let severity = table_status_severity(status);
    let marker: AnyElement = if matches!(status, TableStatus::Listing) {
        // The shared spinner reads the reduce-motion setting, so this table
        // stops turning when the reader asked for less motion.
        crate::panels::common::spinner(
            IconName::LoadCircle,
            Color::Accent,
            IconSize::Custom(rems_from_px(f32::from(design::size::STATUS_MARKER))),
            cx,
        )
    } else {
        Icon::new(design::health_icon(severity))
            .size(IconSize::Custom(rems_from_px(f32::from(
                design::size::STATUS_MARKER,
            ))))
            .color(Color::Custom(
                severity.marker_on(cx, colors.element_background),
            ))
            .into_any_element()
    };
    let mut badge = h_flex()
        .id("table-status")
        .debug_selector(|| "table-status".to_owned())
        .role(Role::Status)
        .aria_label(format!("Table status: {}", status.label()))
        .h(design::size::CONTROL)
        .px(design::space::SM)
        .gap(design::space::XS)
        .items_center()
        .rounded_full()
        .bg(colors.element_background.alpha(1.0))
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .child(marker)
        .child(metadata_label(status.label()).color(Color::Default));
    badge
        .interactivity()
        .tooltip(Tooltip::text(table_status_detail(status)));
    badge.into_any_element()
}

fn table_status_detail(status: &TableStatus) -> String {
    match status {
        TableStatus::Stale(reason) => {
            format!("Live updates stopped. Showing the last available rows. {reason}")
        }
        TableStatus::Failed(reason) => format!("Loading failed. {reason}"),
        TableStatus::Listing => "Receiving the initial resource list.".to_owned(),
        TableStatus::Streaming => "Receiving live resource updates.".to_owned(),
        TableStatus::Paused => "Live updates are paused.".to_owned(),
        TableStatus::Idle => "The resource table has not started loading.".to_owned(),
        TableStatus::Stopped => "The resource table is stopped.".to_owned(),
    }
}

/// Names the empty state the table shows. Each state names a different problem,
/// so an unknown kind never claims the table has no resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptyState {
    Loading,
    Paused,
    Stale,
    Failed,
    UnknownKind,
    NoFilterMatch,
    NoProblems,
    NoRows,
}

/// Resolves the empty state for the current table conditions.
fn empty_state_kind(
    status: &TableStatus,
    filter: &str,
    kind: &str,
    snapshot: Option<&IndexSnapshot>,
    problems_only: bool,
) -> EmptyState {
    match status {
        TableStatus::Listing => EmptyState::Loading,
        TableStatus::Paused if snapshot.is_none_or(|snapshot| snapshot.rows.is_empty()) => {
            EmptyState::Paused
        }
        TableStatus::Stale(_) => EmptyState::Stale,
        TableStatus::Failed(_) => EmptyState::Failed,
        _ if !is_known_kind(kind) => EmptyState::UnknownKind,
        // The status filter comes first. With a status filter on and a name
        // that happens not to match, the empty state blamed the name for a
        // filter the reader never typed.
        _ if problems_only => EmptyState::NoProblems,
        _ if !filter.is_empty() => EmptyState::NoFilterMatch,
        _ => EmptyState::NoRows,
    }
}

struct EmptyStateContext<'a> {
    status: &'a TableStatus,
    filter: &'a str,
    snapshot: Option<&'a IndexSnapshot>,
    spec: &'a ResourceSpec,
    columns: &'a [ResourceColumn],
    visible: &'a [usize],
    widths: &'a [Pixels],
    action_focus: &'a FocusHandle,
    namespace_focus: &'a FocusHandle,
    /// Makes the failure reason reachable by keyboard and screen readers.
    reason_focus: &'a FocusHandle,
    viewport_height: Pixels,
    /// Hides rows that are healthy.
    problems_only: bool,
}

fn empty_state(context: EmptyStateContext<'_>, window: &Window, cx: &App) -> AnyElement {
    let EmptyStateContext {
        status,
        filter,
        snapshot,
        spec,
        columns,
        visible,
        widths,
        action_focus,
        namespace_focus,
        reason_focus: content_focus,
        viewport_height,
        problems_only,
    } = context;
    let plural = spec.label_lower();
    let mut content = v_flex()
        .id("resource-empty")
        .debug_selector(|| "resource-empty".to_owned())
        .size_full()
        .items_center()
        .justify_center()
        .gap(design::space::SM);
    // Use one icon size for all table empty states, and the one size token the
    // rest of the app uses. This used to be Zed's `XLarge` at 48px against
    // `design::size::ICON_LARGE` at 32px, so the same empty state was 1.5x
    // bigger here than in every sibling panel.
    let icon_size = IconSize::Custom(rems_from_px(f32::from(design::size::ICON_LARGE)));
    let icon = |icon_name: IconName| {
        if icon_name == IconName::LoadCircle {
            // The shared spinner stops when the reader asked for less motion.
            crate::panels::common::spinner(icon_name, Color::Accent, icon_size, cx)
        } else {
            Icon::new(icon_name)
                .size(icon_size)
                .color(Color::Muted)
                .into_any_element()
        }
    };
    match empty_state_kind(status, filter, spec.kind.as_ref(), snapshot, problems_only) {
        EmptyState::Loading => {
            return loading_table(spec, columns, visible, widths, viewport_height, cx);
        }
        EmptyState::Paused => {
            let tooltip = action_tooltip("Resume Live Updates", &ToggleUpdates, cx);
            content = content
                .child(icon(IconName::DebugPause))
                .child(body_label("Updates are paused"))
                .child(
                    metadata_label(format!("Resume to load the {plural} list."))
                        .color(Color::Muted),
                )
                .child(
                    Button::new("empty-resume", "Resume")
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(ToggleUpdates), cx);
                        }),
                );
        }
        EmptyState::Stale => {
            let filter_empty = filter.is_empty();
            let (label, tooltip) = if filter_empty {
                (
                    RETRY_LIVE_UPDATES,
                    action_tooltip(RETRY_LIVE_UPDATES, &Refresh, cx),
                )
            } else {
                (
                    "Clear Filter",
                    action_tooltip("Clear Resource Filter", &ClearFilter, cx),
                )
            };
            let guidance = if filter_empty {
                RETRY_LIVE_UPDATES_GUIDANCE
            } else {
                "Clear the filter to view the last available rows."
            };
            content = content
                .child(icon(IconName::Warning))
                .child(body_label("Live updates stopped"))
                .child(metadata_label(guidance).color(Color::Muted))
                .child(
                    Button::new("empty-stale-action", label)
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(move |_event, window, cx| {
                            if filter_empty {
                                window.dispatch_action(Box::new(Refresh), cx);
                            } else {
                                window.dispatch_action(Box::new(ClearFilter), cx);
                            }
                        }),
                );
        }
        EmptyState::Failed => {
            let TableStatus::Failed(reason) = status else {
                unreachable!("the failed state carries a reason")
            };
            // The reason is readable on screen; the raw text stays in the tooltip.
            let visible_reason = user_reason(reason, &plural);
            let reason_focus = content_focus.clone();
            let reason_focused = reason_focus.is_focused(window);
            content = content.aria_description(RETRY_LOADING_RESOURCES_GUIDANCE);
            content
                .interactivity()
                .tooltip(Tooltip::text(reason.clone()));
            content = content
                .child(icon(IconName::Warning))
                .child(
                    body_label(format!("Loading {plural} failed")).color(Color::Custom(
                        design::Severity::Error.marker_on(cx, design::surface::input(cx)),
                    )),
                )
                .child(
                    div()
                        .id("empty-error-reason")
                        .debug_selector(|| "empty-error-reason".to_owned())
                        .track_focus(&reason_focus)
                        .tab_index(0isize)
                        .role(Role::Note)
                        .aria_label(visible_reason.clone())
                        .aria_description(reason.clone())
                        .max_w(px(480.0))
                        .text_center()
                        .child(
                            metadata_label(visible_reason).color(Color::Custom(
                                design::Severity::Error
                                    .marker_on(cx, design::surface::input(cx))
                                    .opacity(0.9),
                            )),
                        )
                        .when(reason_focused, |this| {
                            this.border_1()
                                .border_color(cx.theme().colors().border_focused)
                                .rounded(px(4.0))
                        }),
                )
                .child(metadata_label(RETRY_LOADING_RESOURCES_GUIDANCE).color(Color::Muted))
                .child(
                    Button::new("empty-error-retry", RETRY_LOADING_RESOURCES)
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(action_tooltip(
                            RETRY_LOADING_RESOURCES,
                            &Refresh,
                            cx,
                        )))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(Refresh), cx);
                        }),
                );
        }
        // An unknown kind has no column layout, so an empty list is expected.
        // The list can still arrive late, so the state offers a way forward.
        EmptyState::UnknownKind => {
            content = content
                .child(icon(IconName::Info))
                .child(body_label(UNKNOWN_KIND_TITLE))
                .child(metadata_label(UNKNOWN_KIND_GUIDANCE).color(Color::Muted))
                .child(
                    Button::new("empty-refresh", REFRESH_RESOURCES)
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(action_tooltip(
                            REFRESH_RESOURCES,
                            &Refresh,
                            cx,
                        )))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(Refresh), cx);
                        }),
                );
        }
        EmptyState::NoFilterMatch => {
            content = content
                .child(icon(IconName::MagnifyingGlass))
                .child(body_label(format!("No {plural} match \"{filter}\"")))
                .child(
                    metadata_label("Try a different name or clear the filter.").color(Color::Muted),
                )
                .child(
                    Button::new("empty-clear-filter", "Clear Filter")
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text("Clear Filter"))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(ClearFilter), cx);
                        }),
                );
        }
        EmptyState::NoProblems => {
            // The reader never typed a filter, so "Clear the filter" pointed at
            // a control that had nothing to do with the state on screen. The
            // filter that is actually on is the status one.
            content = content
                .child(icon(IconName::Check))
                .child(body_label(format!("No {plural} need attention")))
                .child(metadata_label(NO_PROBLEMS_GUIDANCE).color(Color::Muted))
                .child(
                    Button::new("empty-show-all", NO_PROBLEMS_ACTION)
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(NO_PROBLEMS_ACTION))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(ClearFilter), cx);
                        }),
                );
        }
        EmptyState::NoRows => {
            let scope_hint = if spec.namespaced {
                format!("This namespace has no {plural}. Check the namespace or refresh the table.")
            } else {
                format!(
                    "This cluster has no {plural}. Check the cluster connection or refresh the table."
                )
            };
            // A live table with no rows still needs one way forward.
            content = content
                .child(icon(design::kind_icon(spec.kind.as_ref())))
                .child(body_label(format!("No {plural} found")))
                .child(metadata_label(scope_hint).color(Color::Muted))
                .child(
                    Button::new("empty-refresh", REFRESH_RESOURCES)
                        .track_focus(action_focus)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text(action_tooltip(
                            REFRESH_RESOURCES,
                            &Refresh,
                            cx,
                        )))
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(Refresh), cx);
                        }),
                );
        }
    }
    if spec.namespaced {
        content = content.child(namespace_action(namespace_focus, cx));
    }
    let state_label =
        match empty_state_kind(status, filter, spec.kind.as_ref(), snapshot, problems_only) {
            EmptyState::Loading => format!("Loading {plural}"),
            EmptyState::Failed => format!("Loading {plural} failed"),
            EmptyState::Paused => {
                format!("Live updates are paused. The {plural} list is not loading.")
            }
            EmptyState::Stale => {
                "Live updates stopped. Showing the last available data.".to_owned()
            }
            EmptyState::UnknownKind => format!("{UNKNOWN_KIND_TITLE} for {plural}"),
            EmptyState::NoFilterMatch => format!("No {plural} match {filter}"),
            EmptyState::NoProblems => format!("No {plural} need attention"),
            EmptyState::NoRows => format!("No {plural} found"),
        };
    content
        .role(Role::Region)
        .aria_label(state_label)
        .into_any_element()
}

fn namespace_action(focus: &FocusHandle, cx: &App) -> AnyElement {
    Button::new("empty-namespace", "Switch Namespace")
        .size(ButtonSize::Medium)
        .track_focus(focus)
        .tab_index(1isize)
        .tooltip(Tooltip::text(action_tooltip(
            "Switch Namespace",
            &OpenNamespaceSwitcher,
            cx,
        )))
        .aria_label("Switch Namespace")
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(OpenNamespaceSwitcher), cx);
        })
        .into_any_element()
}

fn loading_table(
    spec: &ResourceSpec,
    columns: &[ResourceColumn],
    visible: &[usize],
    widths: &[Pixels],
    viewport_height: Pixels,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let row_height = row_height(cx);
    // Fill the viewport instead of guessing a fixed number of rows.
    let rows = rows_in_viewport(
        f32::from(viewport_height) - 2.0 * f32::from(row_height),
        row_height,
    );
    let typography = DataTypography::from_theme_settings(cx);
    let width_at = |position: usize, index: usize| {
        widths
            .get(position)
            .copied()
            .unwrap_or_else(|| px(column_default_width(&columns[index], &typography)))
    };
    let loading_status = h_flex()
        .id("table-loading-status")
        .role(Role::Status)
        .aria_label(format!("Loading {}…", spec.label))
        .flex_none()
        .w_full()
        .h(row_height)
        .px(design::space::SM)
        .gap(design::space::XS)
        .items_center()
        .border_b_1()
        .border_color(colors.border_variant)
        .child(
            Icon::new(IconName::LoadCircle)
                .size(IconSize::XSmall)
                .color(Color::Accent),
        )
        .child(metadata_label(format!("Loading {}…", spec.label)));
    let header = h_flex()
        .id("table-skeleton-header-row")
        .role(Role::Row)
        .aria_row_index(1)
        .flex_none()
        .w_full()
        .h(row_height)
        .overflow_hidden()
        .border_b_1()
        .border_color(colors.border_variant)
        // The skeleton keeps the user's column widths and hidden columns, so
        // the first rows land where the live table will put them.
        .children(visible.iter().enumerate().map(|(position, &index)| {
            let column = &columns[index];
            let column_id: SharedString = column.column.id.as_str().into();
            h_flex()
                .id(("table-skeleton-header", position))
                .debug_selector(move || format!("table-skeleton-header-{column_id}"))
                .w(width_at(position, index))
                .flex_none()
                .px(design::space::SM)
                .font_ui(cx)
                .text_size(rems_from_px(f32::from(design::text::METADATA)))
                .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                .child(metadata_label(column.title).color(Color::Muted))
        }));
    // The count of rows stays readable, so the iterator does not shadow it.
    let skeleton_rows = (0..rows).map(|row_index| {
        // The skeleton stands in for the live rows, so it sits on the same
        // surface. Painting it on the canvas made the table jump a whole ramp
        // level the moment the first row arrived.
        let background = if row_index.is_multiple_of(2) {
            table_row_surface(cx)
        } else {
            design::row_stripe_bg(cx)
        };
        h_flex()
            .id(ElementId::NamedInteger(
                "table-skeleton-row".into(),
                row_index as u64,
            ))
            .role(Role::Row)
            .aria_row_index(grid_row_index(row_index))
            .aria_label(format!("Loading row {}", row_index + 1))
            .flex_none()
            .w_full()
            .h(row_height)
            .overflow_hidden()
            .bg(background)
            .border_b_1()
            .border_color(colors.border_variant)
            .children(visible.iter().enumerate().map(|(position, &index)| {
                let column = &columns[index];
                let width = width_at(position, index);
                if column.column.id == "status" {
                    div().w(width).flex_none().px(design::space::SM).child(
                        div()
                            .size(design::size::STATUS_DOT)
                            .rounded_full()
                            .bg(colors.text.opacity(0.12)),
                    )
                } else {
                    let fraction = 0.42 + ((position * 23) % 37) as f32 / 100.0;
                    div().w(width).flex_none().px(design::space::SM).child(
                        div()
                            .h(px(8.0))
                            // The bar must stay inside its own column.
                            .w(skeleton_bar_width(width, fraction))
                            .rounded_full()
                            .bg(colors.text.opacity(0.10)),
                    )
                }
            }))
    });
    v_flex()
        .id("table-loading-skeleton")
        .debug_selector(|| "table-loading-skeleton".to_owned())
        .role(Role::Grid)
        .aria_label(format!("Loading {} Table", spec.label))
        .aria_row_count(grid_row_count(rows))
        .size_full()
        .overflow_hidden()
        .child(loading_status)
        .child(header)
        .children(skeleton_rows)
        .into_any_element()
}

/// Puts a cell on its row's rhythm, so what it holds lands on the row's center.
///
/// `ui::Table` wraps every cell in a plain block `div`, and a block container
/// stacks its child at the top and sizes it to content. A cell left to that
/// default is only as tall as what it holds, so in a `row` the content sat hard
/// against the top border with the slack dumped underneath it, and two cells
/// with different line heights never shared a center line.
///
/// For a cell that packs a glyph beside a line, the box is this height and the
/// contents are centered in it. A cell holding one line has no such choice to
/// make: it takes the row as its line height instead, which is the same center
/// without a second box to measure. See `cell_line_height`.
///
/// The height is the shared `row` token rather than a percentage of the wrapper:
/// the row draws a 1px bottom border, so a percentage resolves against 27px of a
/// 28px row and every cell lands half a pixel above the one line the whole table
/// has to agree on.
trait CellOnRowRhythm {
    fn cell_on_row_rhythm(self, row_height: Pixels) -> Self;
}

impl<T: gpui::Styled> CellOnRowRhythm for T {
    fn cell_on_row_rhythm(self, row_height: Pixels) -> Self {
        self.h(row_height)
    }
}

#[allow(clippy::too_many_arguments)]
fn row_cells(
    row_index: usize,
    row: &Row,
    columns: &[ResourceColumn],
    visible: &[usize],
    widths: &[Pixels],
    typography: &DataTypography,
    char_width: f32,
    background: Hsla,
    selected: bool,
    pending: Option<&PendingState>,
    has_status_column: bool,
    trailing: Option<design::Confidence>,
    window: &mut Window,
    cx: &App,
) -> Vec<AnyElement> {
    let colors = cx.theme().colors();
    let body_size = rems_from_px(f32::from(design::text::BODY));
    let body_line_height = rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT));
    let data_size = rems_from_px(f32::from(typography.size));
    // The row the cells belong to. The row itself is laid out by `ui::Table`, so
    // the cells measure their own height from the same token the row was given.
    let row_height = typography.row_height();
    // A data cell's line takes the row less the cell's own vertical padding,
    // which is two `space::XS / 2` bands. The cell takes no height of its own, so
    // its box is that line: the row decides the rhythm and the cell follows.
    let cell_line_height = row_height - design::space::XS;
    // The status cell renders in the UI body face, not the data face, so it has
    // to be measured in its own typography. Measuring it with the 12px monospace
    // data face under-counted the width, and the overflow check never fired for
    // a status long enough to clip.
    let status_face = status_typography(cx);
    let status_char_width = f32::from(design::text::BODY) * CHAR_WIDTH_RATIO;
    // Keep muted text readable on the selected row background.
    let muted_color = if selected {
        colors.text
    } else {
        colors.text_muted
    };
    visible
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| {
            let cell = row.cells.get(index)?;
            let column = columns.get(index);
            let is_status = column.is_some_and(|column| column.column.id == "status");
            let cell_key = ((row_index as u64) << 16) | index as u64;
            let cell_id = ElementId::NamedInteger("resource-cell".into(), cell_key);
            let cell_container_id =
                ElementId::NamedInteger("resource-cell-container".into(), cell_key);
            let element: AnyElement = if is_status {
                let severity = status_severity(&cell.text);
                let cell_row = h_flex()
                    .id(cell_container_id)
                    .debug_selector(move || format!("resource-status-cell-{row_index}-{position}"))
                    .role(Role::Cell)
                    .aria_column_index(position + 1)
                    .font_ui(cx)
                    .text_size(body_size)
                    .line_height(body_line_height)
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    // The one column that carries a long value with no room:
                    // without an ellipsis it was cut mid-glyph.
                    .text_ellipsis()
                    .px_1()
                    .py_0p5()
                    .gap(design::space::XS)
                    .items_center()
                    .cell_on_row_rhythm(row_height);
                let cell_row = match pending {
                    Some(pending) => cell_row.child(pending_badge(pending, row_index, cx)),
                    None => cell_row,
                };
                // Clear the selection rail in the leading gutter.
                let cell_row = if position == 0 {
                    cell_row.pl(first_cell_leading_pad())
                } else {
                    cell_row
                };
                let cell_row = if cell.text.is_empty() {
                    cell_row
                } else {
                    // The health glyph leads the cell and is the only place the
                    // verdict is drawn, so it carries the verdict's own name.
                    // The confidence marker beside it already had one; this
                    // channel, the one that decides whether a row is healthy,
                    // had none.
                    cell_row.child(
                        div()
                            .id(ElementId::NamedInteger(
                                "resource-row-health".into(),
                                cell_key,
                            ))
                            .debug_selector(move || format!("resource-row-health-{row_index}"))
                            .role(Role::Image)
                            .aria_label(design::health_label(severity))
                            .w(design::size::STATUS_MARKER)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(design::health_icon(severity))
                                    .size(IconSize::Custom(rems_from_px(f32::from(
                                        design::size::STATUS_MARKER,
                                    ))))
                                    .color(Color::Custom(severity.marker_on(cx, background))),
                            ),
                    )
                };
                cell_row
                    .child(gpui::Text::new(
                        cell_id,
                        SharedString::from(cell.text.clone()),
                    ))
                    .into_any_element()
            } else {
                // The cell's line is the row. A cell sized to its own line plus
                // padding is only `line` tall, and `ui::Table` puts such a child at
                // the top of the row, so the value hugged the top border with the
                // slack underneath it. Taking the whole row leaves the cell's own
                // vertical padding as the only slack, and the line fills what is
                // left, which puts the glyphs on the row's center.
                //
                // This holds because the cell is `whitespace_nowrap`: a second
                // line would be a second row-height line, not a taller cell.
                let mut element = div()
                    .id(cell_container_id)
                    .debug_selector(move || format!("resource-data-cell-{row_index}-{position}"))
                    .role(Role::Cell)
                    .aria_column_index(position + 1)
                    .font(typography.font.clone())
                    .font_features(typography.features.clone())
                    .text_size(data_size)
                    .line_height(cell_line_height)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .px_1()
                    .py_0p5()
                    .child(gpui::Text::new(
                        cell_id,
                        SharedString::from(cell.text.clone()),
                    ));
                // Clear the selection rail in the leading gutter.
                if position == 0 {
                    element = element.pl(first_cell_leading_pad());
                }
                if column.is_some_and(cell_is_right_aligned) {
                    element = element.w_full().text_right();
                }
                if column.is_some_and(|column| column.muted) {
                    element = element.text_color(muted_color);
                }
                // Put the pending badge in the first cell when Status is hidden.
                if let Some(pending) = pending
                    && position == 0
                    && !has_status_column
                {
                    h_flex()
                        .font_ui(cx)
                        .text_size(body_size)
                        .line_height(body_line_height)
                        .whitespace_nowrap()
                        .gap(design::space::XS)
                        .items_center()
                        .w_full()
                        .min_w_0()
                        .child(pending_badge(pending, row_index, cx))
                        .child(element)
                        .into_any_element()
                } else {
                    element.into_any_element()
                }
            };
            // The confidence marker rides the trailing edge of the last cell,
            // opposite the health glyph at the leading edge of the status cell.
            // It shares that cell rather than owning a column of its own, so
            // the table stays rectangular and the row keeps its full width for
            // the columns a reader actually came for.
            let element = match trailing
                .filter(|state| !state.is_definite() && position + 1 == visible.len())
            {
                Some(state) => h_flex()
                    .w_full()
                    .min_w(px(0.0))
                    .items_center()
                    .justify_end()
                    .child(element)
                    .child(
                        confidence_marker(row_index, state, cx)
                            .unwrap_or_else(|| div().into_any_element()),
                    )
                    .into_any_element(),
                None => element,
            };
            // Each cell is measured in the face it renders in, so a status long
            // enough to clip gets a tooltip that carries the whole value.
            let (cell_typography, cell_char_width) = if is_status {
                (&status_face, status_char_width)
            } else {
                (typography, char_width)
            };
            let width = widths
                .get(position)
                .copied()
                .or_else(|| column.map(|column| px(column_default_width(column, typography))))
                .unwrap_or(px(0.0));
            // The shaped width answers whether the value is clipped, and it
            // also sizes the tooltip that carries the full text.
            let measured =
                measure_cell_text(window, &cell.text, cell_typography, width, cell_char_width);
            if !cell_text_overflows(column, &cell.text, width, cell_char_width, measured) {
                return Some(element);
            }
            let key = ((row_index as u64) << 16) | index as u64;
            let mut element = div()
                .w_full()
                .min_w_0()
                .id(ElementId::NamedInteger("pod-cell".into(), key))
                .child(element);
            element
                .interactivity()
                .tooltip(cell_tooltip(&cell.text, measured, cell_typography));
            Some(element.into_any_element())
        })
        .collect()
}

/// Names the hover group of one table row. The trailing action button shows
/// while its row is hovered and the column keeps that width either way, so the
/// reveal never moves a row or another cell.
fn row_hover_group(row_index: usize) -> SharedString {
    SharedString::from(format!("resource-row-hover-{row_index}"))
}

/// Builds the observation-confidence marker for a row.
///
/// It sits on the trailing edge, opposite the health glyph that leads the status
/// cell, so a row that is healthy and a row the app could not read never merge
/// into a single mark. Its color comes from the confidence roles rather than a
/// status hue, and the shape is hollow where health is filled, so the two
/// channels stay separable when a row carries both
/// (`color.md > Inclusive color` on not relying on color alone). It shares the
/// row's trailing reveal channel rather than a second one, and a marker that
/// reports a missing answer is up at rest, because a mark nobody can see is not
/// a channel. A `Known` row still draws nothing at all.
fn confidence_marker(row_index: usize, state: design::Confidence, cx: &App) -> Option<AnyElement> {
    let icon = row_confidence_marker(state)?;
    let label = design::confidence_label(state);
    Some(
        h_flex()
            .id(ElementId::NamedInteger(
                "resource-row-confidence".into(),
                row_index as u64,
            ))
            .debug_selector(move || format!("resource-row-confidence-{row_index}"))
            .role(Role::Image)
            .aria_label(label)
            .aria_description(CONFIDENCE_MARKER_DESCRIPTION)
            .w(design::size::ICON)
            .flex_none()
            .items_center()
            .justify_center()
            .opacity(row_control_reveal(state))
            .group_hover(row_hover_group(row_index), |style| style.opacity(1.0))
            .tooltip(Tooltip::text(label))
            .child(
                Icon::new(icon)
                    .size(IconSize::Custom(rems_from_px(f32::from(
                        design::size::STATUS_MARKER,
                    ))))
                    .color(Color::Custom(design::confidence::foreground(state, cx))),
            )
            .into_any_element(),
    )
}

/// The typography the status cell renders in.
///
/// The status cell is the one cell set in the UI body face rather than the data
/// face, so measuring it with `DataTypography` judged a 13px string by a 12px
/// monospace one and the overflow check never fired.
fn status_typography(cx: &App) -> DataTypography {
    let font = theme::theme_settings(cx).ui_font(cx).clone();
    DataTypography {
        features: font.features.clone(),
        font,
        size: design::text::BODY,
        line_height: design::text::BODY_LINE_HEIGHT,
    }
}

fn column_span(widths: &[f32], index: usize) -> Option<(f32, f32)> {
    let width = *widths.get(index)?;
    if widths[..=index]
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    let start = widths[..index].iter().sum();
    Some((start, start + width))
}

fn reveal_scroll_position(
    current: f32,
    viewport: f32,
    content_width: f32,
    span: (f32, f32),
) -> f32 {
    if !current.is_finite()
        || !viewport.is_finite()
        || !content_width.is_finite()
        || !span.0.is_finite()
        || !span.1.is_finite()
        || span.1 < span.0
        || viewport <= 0.0
    {
        return 0.0;
    }
    let max_offset = (content_width - viewport).max(0.0);
    let mut position = current.clamp(0.0, max_offset);
    let (start, end) = span;
    if end - start >= viewport || start < position {
        position = start;
    } else if end > position + viewport {
        position = end - viewport;
    }
    position.clamp(0.0, max_offset)
}

/// Keeps a loading placeholder inside the cell padding of its column.
fn skeleton_bar_width(width: Pixels, fraction: f32) -> Pixels {
    let cell = f32::from(width);
    let inset = 2.0 * f32::from(design::space::SM);
    let available = (cell - inset).max(0.0);
    px((cell * fraction).clamp(0.0, available))
}

/// Reports whether a cell's text is right-aligned.
///
/// A number column reads down its right edge; a text column reads from its left.
fn cell_is_right_aligned(column: &ResourceColumn) -> bool {
    let id = column.column.id.to_ascii_lowercase();
    let title = column.title.to_ascii_lowercase();
    column.numeric || id.contains("byte") || title.contains("byte") || title.contains("size")
}

/// Returns the width a numeric column starts at.
///
/// A numeric column holds a count, a ratio, or a short age, and none of those
/// reaches past five glyphs. Reserving the designed 80 to 100px for two or three
/// characters takes the room away from the name column, which is the one that
/// actually truncates, so the default is the value's own width. A width the
/// reader chose still wins: this only moves the starting point.
fn column_default_width(column: &ResourceColumn, typography: &DataTypography) -> f32 {
    if column.numeric {
        column
            .width
            .min(narrow_numeric_width(column.title, typography))
    } else {
        column.width
    }
}

/// Returns the narrowest a numeric column may be, padding included.
///
/// The header decides, not the value. `Ready` and `Age` fit in five glyphs, but
/// sizing to the value clipped `Restarts` to `Re…`, and a header the reader has
/// to guess at is worse than a column with a little slack in it. Values vary and
/// already carry a tooltip; a header is a fixed label that has to stay readable.
///
/// The value floor measures the face the cell is drawn in, which the Data font
/// size setting decides. `DataTypography::columns` is that measurement, so the
/// floor and the digits it has to hold cannot come from two different notions of
/// how wide a character is.
fn narrow_numeric_width(title: &str, typography: &DataTypography) -> f32 {
    /// A ready ratio and a restart count are the longest values these columns
    /// show.
    const NARROW_VALUE_GLYPHS: f32 = 5.0;
    /// The header is set in the UI face, which runs wider per glyph than the
    /// data face the value estimate assumes. Sizing the header at the data
    /// ratio clipped `Ready` to `Re…`, and a header the reader has to guess at
    /// costs more than the slack it saves.
    const HEADER_WIDTH_RATIO: f32 = 0.8;
    let padding = 2.0 * f32::from(design::space::SM);
    let header = title.chars().count().max(1) as f32
        * f32::from(design::text::METADATA)
        * HEADER_WIDTH_RATIO;
    let value = f32::from(typography.columns(NARROW_VALUE_GLYPHS));
    header.max(value) + padding
}

fn horizontal_scroll_hit_padding() -> Pixels {
    design::border::HIT
}

/// Builds a tooltip that carries the full text. `shape` holds the measured
/// width and the font of a value, so the box matches the text it shows; UI copy
/// passes `None` and keeps the default measure.
fn table_tooltip(
    text: impl Into<SharedString>,
    shape: Option<CellShape>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    // A zero width means "no measurement", which keeps the default measure.
    let width = shape
        .as_ref()
        .map(|shape| shape.width)
        .filter(|width| f32::from(*width) > 0.0)
        .map(|width| {
            if width > TOOLTIP_MAX_WIDTH {
                TOOLTIP_MAX_WIDTH
            } else {
                width
            }
        });
    let font = shape.as_ref().map(|shape| shape.font.clone());
    let features = shape.as_ref().map(|shape| shape.features.clone());
    let size = shape.as_ref().map(|shape| shape.size);
    Tooltip::element(move |_, cx| {
        ui::tooltip_container(cx, |container, _| {
            // The full value stays readable: a long one scrolls instead of
            // losing the tail behind a line clamp. Scrollable overflow needs a
            // stateful element, and a tooltip must stay id-free, so the style
            // is set directly.
            let mut content = div()
                .when_some(font.clone(), |this, font| this.font(font))
                .when_some(features.clone(), |this, features| {
                    this.font_features(features)
                })
                .when_some(size, |this, size| {
                    this.text_size(rems_from_px(f32::from(size)))
                })
                .when_some(width, |this, width| this.w(width))
                .whitespace_normal()
                .max_h(TOOLTIP_MAX_HEIGHT)
                .child(text.clone());
            content.style().overflow.y = Some(gpui::Overflow::Scroll);
            container.child(content)
        })
        .into_any_element()
    })
}

/// Shows the whole cell value in a box that follows its measured width. The
/// value keeps the data font, so the box matches the text it carries.
fn cell_tooltip(
    text: &str,
    measured: Option<Pixels>,
    typography: &DataTypography,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let shape = CellShape {
        width: measured.unwrap_or(px(0.0)),
        font: typography.font.clone(),
        features: typography.features.clone(),
        size: typography.size,
    };
    table_tooltip(SharedString::from(text.to_owned()), Some(shape))
}

fn header_accessibility_label(title: &str, affordance: SortAffordance) -> String {
    format!("{title}, {}", sort_description(affordance))
}

/// Returns the column header height. The shared table header pads its cells by
/// 4px, so a 20px cell keeps a 20px hit target and a 32px band.
fn header_cell_height() -> Pixels {
    design::size::ROW - 2 * design::space::XS
}

/// Returns the color a column header's label draws with.
///
/// The sorted column and the column the keyboard selected are both emphasized,
/// and neither paints the cell. The accent highlight in a list is the focus
/// channel, so a sort must not borrow it: sorting is not focus, and one color
/// may only mean one thing (`color.md > Best practices`).
fn header_label_color(emphasized: bool, cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    if emphasized {
        colors.text
    } else {
        colors.text_muted
    }
}

/// Returns the color of the rail that marks the column the keyboard selected.
/// A rail keeps the selection readable without a filled slab across the header
/// band.
fn column_focus_rail_color(table_focused: bool, cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    if table_focused {
        colors.text_accent
    } else {
        colors.border_focused
    }
}

fn column_focus_rail(index: usize, table_focused: bool, cx: &App) -> AnyElement {
    div()
        .debug_selector(move || format!("resource-column-rail-{index}"))
        .absolute()
        // The shared table header pads its cells by 4px, so the rail inside a
        // header cell started 4px right of the rail on the body rows. One rail,
        // one column edge.
        .left(RAIL_GUTTER - design::space::XS)
        .top_0()
        .bottom_0()
        .w(design::border::TABLE_FOCUS_RAIL)
        .bg(column_focus_rail_color(table_focused, cx))
        .into_any_element()
}

fn header_accessibility_description(title: &str, affordance: SortAffordance) -> String {
    let next_action = match affordance {
        SortAffordance::Ascending => "Click or Shift+Enter to sort from high to low.",
        SortAffordance::Descending => "Click or Shift+Enter to return to the default order.",
        SortAffordance::Relevance => "Click or Shift+Enter to sort explicitly.",
        SortAffordance::Unsorted => "Click or Shift+Enter to sort from low to high.",
    };
    format!(
        "{title}. {} {next_action} Right-click opens the same menu. Press Enter or Space to sort, or press {ROW_ACTIONS_KEYS} to open Column Options.",
        sort_description(affordance)
    )
}

/// Returns visible column indexes in their original order.
fn visible_indices(columns: &[ResourceColumn], hidden: &HashSet<String>) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !hidden.contains(&column.column.id))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{Modifiers, TestAppContext};
    use k8s_core::controller::{StoreEvent, StoreOp};
    use serde_json::json;
    use theme::LoadThemes;
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::panels::InspectorPanel;
    use crate::table_view::columns::pod_columns;
    use crate::table_view::source::{ResourceSource, SourceEvent, Subscription};

    #[test]
    fn header_click_cycles_ascending_descending_then_default_order() {
        let default_column = fallback_sort_column(&[0, 1, 2, 3]);
        assert_eq!(next_sort(None, 3, default_column), Some(Sort::ascending(3)));
        assert_eq!(
            next_sort(Some(Sort::ascending(3)), 3, default_column),
            Some(Sort::descending(3))
        );
        // The third state is a real sort on the default column. An "unsorted"
        // table still runs by Name, so claiming no sort would be a lie.
        assert_eq!(
            next_sort(Some(Sort::descending(3)), 3, default_column),
            Some(Sort::ascending(default_column))
        );
        assert_eq!(
            next_sort(Some(Sort::ascending(default_column)), 3, default_column),
            Some(Sort::ascending(3)),
            "the cycle starts over on the clicked column"
        );
        // Sorting the default column itself has two states, not three: the
        // order it already shows is the order the third click returns to.
        assert_eq!(
            next_sort(
                Some(Sort::descending(default_column)),
                default_column,
                default_column
            ),
            Some(Sort::ascending(default_column))
        );
    }

    #[test]
    fn the_default_order_column_is_the_first_visible_one() {
        assert_eq!(fallback_sort_column(&[0, 1, 2]), 0);
        assert_eq!(
            fallback_sort_column(&[2, 3, 4]),
            2,
            "a hidden Name column hands the default order to the first column"
        );
        assert_eq!(fallback_sort_column(&[]), DEFAULT_SORT_COLUMN);
    }

    #[test]
    fn header_click_moves_sort_to_the_clicked_column() {
        assert_eq!(
            next_sort(Some(Sort::descending(1)), 4, DEFAULT_SORT_COLUMN),
            Some(Sort::ascending(4))
        );
    }

    #[test]
    fn sort_affordance_distinguishes_direction_and_unsorted_columns() {
        assert_eq!(
            sort_affordance_with_relevance(None, 0, false),
            SortAffordance::Unsorted
        );
        assert_eq!(
            sort_affordance_with_relevance(Some(Sort::ascending(2)), 2, false),
            SortAffordance::Ascending
        );
        assert_eq!(
            sort_affordance_with_relevance(Some(Sort::descending(2)), 2, false),
            SortAffordance::Descending
        );
        assert_eq!(
            sort_affordance_with_relevance(Some(Sort::ascending(1)), 2, false),
            SortAffordance::Unsorted
        );
        assert!(sort_description(SortAffordance::Ascending).contains("low to high"));
        assert!(sort_description(SortAffordance::Descending).contains("high to low"));
    }

    #[test]
    fn table_accessibility_description_uses_short_steps_and_ctrl_tab() {
        assert!(TABLE_ACCESSIBILITY_DESCRIPTION.contains("Ctrl+Tab to leave the table."));
        assert!(
            TABLE_ACCESSIBILITY_DESCRIPTION.contains(ROW_ACTIONS_KEYS),
            "the description names the context-menu keys"
        );
        assert_eq!(TABLE_ACCESSIBILITY_DESCRIPTION.matches('.').count(), 7);
    }

    #[test]
    fn column_header_description_names_how_to_open_options() {
        assert!(
            header_accessibility_description("Name", SortAffordance::Unsorted)
                .ends_with("to open Column Options.")
        );
        assert!(
            header_accessibility_description("Name", SortAffordance::Unsorted)
                .contains(ROW_ACTIONS_KEYS)
        );
    }

    /// The trailing Actions column is gone. The row itself advertises the
    /// row-menu keys, so removing the button did not remove the affordance.
    #[test]
    fn the_row_carries_the_row_menu_keys_without_an_actions_column() {
        assert!(ROW_ACTIONS_KEYSHORTCUTS.contains("F10"));
        assert!(ROW_ACTIONS_KEYSHORTCUTS.contains("Shift+F10"));
        assert!(ROW_ACTIONS_KEYSHORTCUTS.contains("Menu"));
        assert!(
            ROW_ACTIONS_KEYSHORTCUTS.contains("Enter"),
            "Enter opens the row's details from the focused row"
        );
    }

    // A permanent glyph on every header says "sortable" once per column and
    // reads as noise, so only the sorted column shows a direction and the rest
    // keep theirs for the pointer.
    #[test]
    fn only_the_sorted_column_shows_its_sort_glyph() {
        let ascending = sort_indicator(SortAffordance::Ascending);
        let descending = sort_indicator(SortAffordance::Descending);
        assert!(ascending.is_shown() && descending.is_shown());
        assert_eq!(ascending.icon(), IconName::ArrowUp);
        assert_eq!(descending.icon(), IconName::ArrowDown);

        let unsorted = sort_indicator(SortAffordance::Unsorted);
        assert!(!unsorted.is_shown(), "an unsorted header is quiet at rest");
        assert_eq!(
            unsorted.icon(),
            IconName::ChevronUpDown,
            "hover still has to offer the affordance"
        );
        // Relevance orders the whole table, so it belongs to no single column
        // and must not read as one column's direction.
        assert_eq!(sort_indicator(SortAffordance::Relevance), unsorted);
    }

    // A header marks sort with a glyph and a label, never with a filled cell:
    // the accent highlight in a list is the focus channel, and one color may
    // only mean one thing.
    #[gpui::test]
    fn a_column_header_never_paints_the_row_selection_fill(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            let colors = cx.theme().colors();
            let selection_fills = [colors.element_selected, colors.element_active];
            let header_colors = [
                header_label_color(false, cx),
                header_label_color(true, cx),
                column_focus_rail_color(true, cx),
                column_focus_rail_color(false, cx),
            ];
            for fill in selection_fills {
                assert!(
                    !header_colors.contains(&fill),
                    "a sorted header must not borrow the row selection fill"
                );
            }
        });
    }

    // One header cannot put the glyph on the far side of a right-aligned label
    // and the near side of a left-aligned one: a number used to render
    // `⇕ Ready` with the glyph a column-width away from its label.
    #[gpui::test]
    fn the_sort_glyph_follows_its_label_in_every_column(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1600.), px(1000.)));
        cx.run_until_parked();
        let (numeric, text) = view.read_with(cx, |view, _| {
            let index = |id: &str| {
                view.columns
                    .iter()
                    .position(|column| column.column.id == id)
                    .unwrap_or_else(|| panic!("{id} column"))
            };
            (index("restarts"), index("status"))
        });

        for (id, index) in [("restarts", numeric), ("status", text)] {
            view.update(cx, |view, cx| {
                view.apply_sort(Some(Sort::ascending(index)), cx)
            });
            cx.run_until_parked();
            let label = cx
                .debug_bounds("pod-header-sorted-label")
                .unwrap_or_else(|| panic!("{id} header label"));
            let glyph = cx
                .debug_bounds("pod-header-sorted-glyph")
                .unwrap_or_else(|| panic!("{id} header glyph"));
            assert!(
                glyph.left() >= label.right(),
                "{id}: the sort glyph follows its label"
            );
        }
    }

    // A numeric column that reserves 80 to 100px for two or three characters
    // takes the room away from the name column, which is the one that
    // truncates. The default follows the value instead, and follows the font
    // the reader actually set.
    #[test]
    fn a_numeric_column_is_narrow_but_its_header_stays_readable() {
        let typography = crate::settings::test_data_typography(
            f32::from(design::text::DATA),
            f32::from(design::text::DATA) * crate::settings::PRODUCT_DATA_LINE_HEIGHT,
        );
        let data_size = typography.size;
        let columns = pod_columns();
        let column = |id: &str| {
            columns
                .iter()
                .find(|column| column.column.id == id)
                .unwrap_or_else(|| panic!("{id} column"))
        };
        let char_width = f32::from(data_size) * CHAR_WIDTH_RATIO;
        // The widest value each of these columns shows.
        let widest = |id: &str| match id {
            "ready" => "12/12",
            "age" => "3650d",
            _ => "1234",
        };
        for id in ["ready", "restarts", "age"] {
            let value = column(id);
            let narrow = column_default_width(value, &typography);
            assert_eq!(
                narrow,
                narrow_numeric_width(value.title, &typography),
                "{id} default"
            );
            assert!(
                narrow < value.width,
                "{id} reserved {}px for a short value",
                value.width
            );
            // The header is a fixed label, so it decides the floor. Sizing to the
            // value instead clipped `Restarts` to `Re…`, and a header the reader
            // has to guess at costs more than a little slack in the column.
            let header_glyphs = value.title.chars().count() as f32;
            let header_fits = header_glyphs * f32::from(design::text::METADATA) * 0.8
                + 2.0 * f32::from(design::space::SM);
            assert!(
                narrow >= header_fits,
                "{id} clips its own header at the default width"
            );
            // The value still fits, so widening a numeric column is a choice
            // rather than a repair.
            let longest = widest(id);
            assert!(
                !needs_tooltip(Some(value), longest, px(narrow), char_width),
                "{id} clips {longest} at the narrow default"
            );
        }
        // Text columns keep the room they need, and the resize floor still fits
        // the narrow default.
        for id in ["name", "namespace", "image"] {
            assert_eq!(
                column_default_width(column(id), &typography),
                column(id).width
            );
        }
        let widest_header = columns
            .iter()
            .filter(|column| column.numeric)
            .map(|column| narrow_numeric_width(column.title, &typography))
            .fold(0.0, f32::max);
        assert!(widest_header >= COLUMN_MIN_WIDTH);
    }

    // The Data font size setting enlarged the text but not the columns, so
    // every numeric value started clipping the moment the reader raised it.
    #[test]
    fn a_numeric_column_widens_with_the_data_font_size() {
        let at = |size: f32| {
            narrow_numeric_width(
                "Restarts",
                &crate::settings::test_data_typography(
                    size,
                    size * crate::settings::PRODUCT_DATA_LINE_HEIGHT,
                ),
            )
        };
        assert_eq!(
            at(12.0),
            at(f32::from(design::text::DATA)),
            "the default data token is the default size"
        );
        assert!(
            at(40.0) > at(12.0) + 8.0,
            "a 40px data face must move the value floor, not only the header: {} -> {}",
            at(12.0),
            at(40.0)
        );
    }

    #[test]
    fn table_action_copy_names_complete_actions() {
        assert_eq!(RETRY_LIVE_UPDATES, "Retry Live Updates");
        assert_eq!(RETRY_LOADING_RESOURCES, "Retry Loading Resources");
        assert_eq!(START_PORT_FORWARD, "Start Port Forward");
        assert!(RETRY_LIVE_UPDATES_GUIDANCE.starts_with("Retry live updates."));
        assert!(RETRY_LOADING_RESOURCES_GUIDANCE.starts_with("Retry loading resources."));
    }

    #[test]
    fn status_severity_covers_workload_status_labels() {
        assert_eq!(status_severity("Running"), Severity::Success);
        assert_eq!(status_severity("Pending"), Severity::Warning);
        assert_eq!(status_severity("NotReady"), Severity::Warning);
        assert_eq!(status_severity("CrashLoopBackOff"), Severity::Error);
        assert_eq!(status_severity(""), Severity::Muted);
        // A pod the kubelet lost track of is not a pod that is waiting. The two
        // used to share a branch, so both rows got the same glyph and the same
        // amber, pixel for pixel apart from the word.
        assert_ne!(
            status_severity("Unknown"),
            status_severity("Pending"),
            "Unknown must not be painted as Pending"
        );
        assert_eq!(
            design::health_icon(status_severity("Unknown")),
            design::health_icon(Severity::Neutral),
            "no verdict is the dash, not the warning triangle"
        );
        assert_eq!(
            design::health_label(status_severity("Unknown")),
            "No verdict"
        );
    }

    // A resource the app could not read is not healthy, and a cluster that
    // stopped answering is not stale on one row only. The row carries no
    // freshness of its own, so the answer comes from the status text it already
    // shows and from the table it is drawn in.
    #[test]
    fn confidence_reads_the_answer_the_row_already_shows() {
        let columns = pod_columns();
        let status_index = columns
            .iter()
            .position(|column| column.column.id == "status")
            .expect("status column");
        let row = |phase: &str| Row {
            obj: test_pod(0),
            cells: {
                let mut cells = vec![CellValue::empty(); columns.len()];
                cells[status_index] = CellValue::text(phase);
                cells
            },
        };
        assert_eq!(
            row_confidence(false, &row("Running"), &columns),
            design::Confidence::Known
        );
        // A pod the kubelet lost track of states no verdict, which is not the
        // same as a pod that is unwell.
        assert_eq!(
            row_confidence(false, &row("Unknown"), &columns),
            design::Confidence::Unknown
        );
        // Freshness belongs to the table, so a stopped watch marks every row it
        // is still drawing.
        assert_eq!(
            row_confidence(true, &row("Running"), &columns),
            design::Confidence::Stale
        );
        // A table with no status column has no per-row verdict to doubt.
        assert_eq!(
            row_confidence(false, &row("Running"), &[]),
            design::Confidence::Known
        );
    }

    // A current row needs no marker: a mark on every row would invent a second
    // thing to read.
    #[test]
    fn only_an_unanswered_row_carries_a_confidence_glyph() {
        assert_eq!(row_confidence_marker(design::Confidence::Known), None);
        assert_eq!(
            row_confidence_marker(design::Confidence::Stale),
            Some(IconName::HistoryRerun)
        );
        assert_eq!(
            row_confidence_marker(design::Confidence::Unknown),
            Some(IconName::CircleHelp)
        );
        // A marker that reports a missing answer has to be up at rest. It took
        // `selected: bool`, so on every row that was neither selected nor
        // hovered it was fully transparent, which is every row.
        assert_eq!(row_control_reveal(design::Confidence::Known), 0.0);
        assert_eq!(row_control_reveal(design::Confidence::Stale), 1.0);
        assert_eq!(row_control_reveal(design::Confidence::Unknown), 1.0);
    }

    // Health is filled or heavy and confidence is hollow, so a row that carries
    // both never merges the two axes into one mark.
    #[test]
    fn the_two_channels_use_different_glyph_families() {
        for severity in [
            Severity::Success,
            Severity::Warning,
            Severity::Error,
            Severity::Info,
            Severity::Neutral,
            Severity::Muted,
        ] {
            for state in [design::Confidence::Stale, design::Confidence::Unknown] {
                assert_ne!(
                    design::health_icon(severity),
                    design::confidence::icon(state),
                    "health and confidence must not share a shape"
                );
            }
        }
    }

    #[test]
    fn table_status_severity_matches_status_shape() {
        assert_eq!(
            table_status_severity(&TableStatus::Streaming),
            Severity::Success
        );
        assert_eq!(table_status_severity(&TableStatus::Listing), Severity::Info);
        assert_eq!(
            table_status_severity(&TableStatus::Paused),
            Severity::Warning
        );
        assert_eq!(
            table_status_severity(&TableStatus::Stale("reason".to_owned())),
            Severity::Warning
        );
        assert_eq!(
            table_status_severity(&TableStatus::Failed("reason".to_owned())),
            Severity::Error
        );
        assert!(
            table_status_detail(&TableStatus::Stale("watch ended".to_owned()))
                .contains("watch ended")
        );
    }

    #[test]
    fn visible_indices_filters_hidden_columns_in_order() {
        let columns = pod_columns();
        let mut hidden = HashSet::new();
        assert_eq!(
            visible_indices(&columns, &hidden),
            (0..columns.len()).collect::<Vec<_>>()
        );
        hidden.insert("namespace".to_owned());
        hidden.insert("image".to_owned());
        let visible = visible_indices(&columns, &hidden);
        let image = columns
            .iter()
            .position(|column| column.column.id == "image")
            .expect("image column");
        assert!(!visible.contains(&1), "namespace is hidden");
        assert!(!visible.contains(&image), "image is hidden");
        assert_eq!(visible.len(), columns.len() - 2);
        assert!(
            visible.windows(2).all(|pair| pair[0] < pair[1]),
            "order is stable"
        );
    }

    #[test]
    fn target_column_moves_within_visible_columns() {
        assert_eq!(target_column(0, 1, 4), 1);
        assert_eq!(target_column(3, 1, 4), 3, "right edge does not wrap");
        assert_eq!(target_column(0, -1, 4), 0, "left edge does not wrap");
    }

    #[test]
    fn row_accessible_label_keeps_nonempty_cell_values() {
        let cells = [
            CellValue::text("web-0"),
            CellValue::empty(),
            CellValue::text("Running"),
        ];
        let columns = pod_columns();
        let visible: Vec<usize> = (0..columns.len()).collect();
        assert_eq!(
            row_accessible_label(&columns, &visible, &cells),
            "Name: web-0, Status: Running, Health: Healthy"
        );
        assert_eq!(
            row_accessible_label(&[], &[], &[CellValue::empty()]),
            "Resource row"
        );
        assert_eq!(
            row_accessible_label(&columns, &[0], &cells),
            "Name: web-0, Health: Healthy",
            "hidden columns stay out of accessible labels, the health verdict does not"
        );
    }

    // A screen reader used to hear "Status: Pending" and stop there. The verdict
    // only ever existed as a glyph, so nothing told the reader that Pending is a
    // warning, and nothing distinguished it from a pod that is merely waiting.
    #[test]
    fn the_row_label_announces_the_health_verdict_not_just_the_status() {
        let columns = pod_columns();
        let visible: Vec<usize> = (0..columns.len()).collect();
        let row = |status: &str| {
            let mut cells = vec![CellValue::empty(); columns.len()];
            cells[2] = CellValue::text(status);
            row_accessible_label(&columns, &visible, &cells)
        };
        let pending = row("Pending");
        assert!(
            pending.contains(design::health_label(Severity::Warning)),
            "{pending}"
        );
        let unknown = row("Unknown");
        assert!(
            unknown.contains("No verdict"),
            "an unanswered pod must not read as a warning: {unknown}"
        );
        assert_ne!(unknown, pending, "the two states must not read the same");
    }

    #[test]
    fn focused_row_falls_back_to_first_row_without_changing_selection() {
        assert_eq!(
            row_visual(false, false, true, false, 0),
            RowVisual::FocusRing,
            "the first row carries the ring before a selection exists"
        );
        // Only row 0 gets the ring, and the zebra stripe follows the row index
        // whether or not a selection exists.
        assert_eq!(row_visual(false, false, true, false, 1), RowVisual::Stripe);
        assert_eq!(row_visual(false, false, false, true, 0), RowVisual::Plain);
        assert_eq!(row_visual(false, false, true, true, 1), RowVisual::Stripe);
    }

    // Losing keyboard focus must not leave the selection looking active.
    #[test]
    fn selected_rows_differ_when_the_table_loses_focus() {
        let focused = row_visual(true, true, true, true, 2);
        let unfocused = row_visual(true, true, false, true, 2);
        assert_ne!(focused, unfocused);
        assert_eq!(focused, RowVisual::SelectedFocused);
        assert_eq!(unfocused, RowVisual::SelectedUnfocused);
        assert!(focused.is_selected(), "both keep the selection fill");
        assert!(unfocused.is_selected());
        assert!(focused.has_rail() && unfocused.has_rail());
        assert_ne!(
            design::border::TABLE_FOCUS_RAIL,
            design::border::FOCUS_RAIL,
            "the active row uses the thicker table rail"
        );
        // A selected row never renders the hover-only stripe.
        assert_ne!(unfocused, row_visual(false, false, false, true, 2));
    }

    // A multi-row selection must mark one command target, not every row.
    #[test]
    fn only_the_anchor_row_of_a_multi_selection_keeps_the_table_rail() {
        let anchor = row_visual(true, true, true, true, 0);
        let member = row_visual(true, false, true, true, 3);
        assert_eq!(anchor, RowVisual::SelectedFocused);
        assert_eq!(member, RowVisual::SelectedMember);
        assert_ne!(
            anchor, member,
            "the active row must look different from the rest of the selection"
        );
        assert!(
            anchor.is_selected() && member.is_selected(),
            "both keep the selection fill"
        );
        assert!(
            uses_table_focus_rail(anchor),
            "the anchor row carries the thick table rail"
        );
        assert!(
            !uses_table_focus_rail(member),
            "only the anchor of a multi-row selection carries the thick table rail"
        );
        // A single selection is always its own anchor, so the thick rail shows.
        assert_eq!(
            row_visual(true, true, true, true, 7),
            RowVisual::SelectedFocused
        );
        // Losing keyboard focus leaves every selected row on the thin rail.
        assert_eq!(
            row_visual(true, false, false, false, 3),
            RowVisual::SelectedUnfocused
        );
    }

    // The two selected states must not paint the same fill.
    #[gpui::test]
    fn unfocused_selected_fill_is_muted(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            assert_ne!(
                design::row_selected_bg(cx),
                row_selected_muted_bg(cx),
                "a selected row that lost focus must look different"
            );
        });
    }

    // A range member used to take the muted fill, which measured 1.018:1 in
    // dark and 1.022:1 in light against the row beside it. The wash was the only
    // signal a member had and neither appearance could show it.
    #[gpui::test]
    fn a_range_member_takes_the_same_fill_as_the_active_row(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            let selected = design::row_selected_bg(cx);
            let selected_muted = row_selected_muted_bg(cx);
            let focused = design::row_focus_bg(cx);
            let stripe = design::row_stripe_bg(cx);
            let plain = design::surface::input(cx);
            let fill = |visual| {
                row_visual_background(visual, selected, selected_muted, focused, stripe, plain)
            };
            assert_eq!(
                fill(RowVisual::SelectedMember),
                fill(RowVisual::SelectedFocused),
                "a range member has to read as selected"
            );
            for visual in [
                RowVisual::SelectedMember,
                RowVisual::SelectedFocused,
                RowVisual::SelectedUnfocused,
            ] {
                assert_ne!(
                    fill(visual),
                    plain,
                    "a selected row cannot match an unselected one"
                );
                assert_ne!(
                    fill(visual),
                    stripe,
                    "a selected row cannot match a striped one"
                );
            }
            // The rail still names one command target, so a member and the
            // active row stay told apart without a second colour.
            assert!(!uses_table_focus_rail(RowVisual::SelectedMember));
            assert!(uses_table_focus_rail(RowVisual::SelectedFocused));
        });
    }

    // The keyboard cursor used to share the hover token, so a row the keyboard
    // was on and a row the pointer was over were the same colour, and in the
    // light appearance the focus wash was quieter than the zebra stripe beside
    // it.
    #[gpui::test]
    fn the_keyboard_cursor_is_not_the_pointer_hover(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            assert_ne!(
                design::row_focus_bg(cx),
                design::row_hover_bg(cx),
                "the cursor and the hover are different states"
            );
            for state in [
                design::row_focus_bg(cx),
                design::row_hover_bg(cx),
                design::row_selected_bg(cx),
                design::row_stripe_bg(cx),
            ] {
                assert_ne!(
                    state,
                    design::surface::input(cx),
                    "a row state must not be the table's own surface"
                );
            }
        });
    }

    #[test]
    fn inspection_identity_uses_arc_ownership() {
        let first = test_pod(0);
        let same = Arc::clone(&first);
        let replacement = test_pod(0);
        assert!(!PodsView::inspection_changed(Some(&first), Some(&same)));
        assert!(PodsView::inspection_changed(
            Some(&first),
            Some(&replacement)
        ));
        assert!(PodsView::inspection_changed(None, Some(&first)));
        assert!(!PodsView::inspection_changed(None, None));
    }

    #[gpui::test]
    fn table_waits_for_user_navigation_before_taking_focus(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        let table = view.read_with(cx, |view, cx| view.table_focus_handle(cx));
        assert!(!cx.update(|window, _| table.is_focused(window)));
        focus_table(cx, &view);
        assert!(cx.update(|window, _| table.is_focused(window)));
    }

    #[gpui::test]
    fn resource_grid_fills_viewport_after_scrollbar_cleanup(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let grid = cx.debug_bounds("resource-grid").expect("resource grid");
        let status = cx.debug_bounds("table-status").expect("table status");
        let filter = cx
            .debug_bounds("shared-text-input")
            .expect("resource filter");
        assert!(f32::from(grid.size.height) > 400.);
        assert!(status.right() <= px(960.0));
        assert!(filter.right() <= px(960.0));
    }

    /// Installs the theme the app ships, refined the way the app refines it.
    ///
    /// The base theme every other test loads has a single surface level, so
    /// `surface::input` and `surface::canvas` are the same colour there and a
    /// guard that says the table is not floating on the canvas cannot fail on
    /// it. The product theme is the one `DESIGN.md` §3.4's ramp describes and
    /// the only one a reader ever sees.
    fn install_product_theme(cx: &mut gpui::App) {
        let family = theme_settings::refine_theme_family(
            theme_settings::deserialize_user_theme(
                include_str!("../../../k8s-app/assets/themes/k8s-studio.json").as_bytes(),
            )
            .expect("the product theme parses"),
        );
        let mut theme = family
            .themes
            .into_iter()
            .find(|theme| theme.name.as_ref() == "K8s Studio Dark")
            .expect("K8s Studio Dark");
        design::refine_theme(&mut theme);
        theme::GlobalTheme::update_theme(cx, Arc::new(theme));
    }

    // `DESIGN.md §3.4` files `surface` under tables and inputs. The table used
    // to paint its rows on `canvas`, and the loading skeleton on the canvas too,
    // so the one level of the ramp a reader stares at for eight hours was never
    // on screen.
    #[gpui::test]
    fn the_table_and_its_skeleton_sit_on_the_input_surface(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(install_product_theme);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                table_row_surface(cx),
                design::surface::input(cx),
                "a row composites onto the input surface, not the canvas"
            );
            assert_ne!(
                table_row_surface(cx),
                design::surface::canvas(cx),
                "the table must not float on the canvas"
            );
            assert_ne!(
                design::row_stripe_bg(cx),
                table_row_surface(cx),
                "the zebra must not be the table's own surface"
            );
        });
        assert!(cx.debug_bounds("resource-grid").is_some());
        assert!(
            cx.debug_bounds("resource-row-0").is_some(),
            "the live table paints the rows the skeleton stood in for"
        );
    }

    // The last column used to carry `TableResizeBehavior::None`, so the one
    // column that clips mid-glyph had no pointer way out of it.
    #[gpui::test]
    fn every_visible_column_keeps_its_resize_handle(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let (state, visible) = view.read_with(cx, |view, _| {
                (view.columns_state.clone(), view.visible_columns.len())
            });
            let behavior = state.read(cx).resize_behavior().clone();
            assert_eq!(behavior.cols(), visible);
            assert!(
                behavior
                    .as_slice()
                    .iter()
                    .all(|behavior| behavior.is_resizable()),
                "the last column must not opt out of resizing"
            );
        });
    }

    // A partial column at the right edge used to read as the end of the data:
    // no ellipsis, no fade, and no scrollbar thumb anywhere in the last 60px.
    #[test]
    fn the_horizontal_edge_follows_the_scroll_range() {
        assert!(
            !has_more_columns_to_the_right(px(0.), px(0.)),
            "a table that fits has no clipped edge to signal"
        );
        assert!(
            has_more_columns_to_the_right(px(0.), px(120.)),
            "content to the right is the only case that needs a signal"
        );
        assert!(
            !has_more_columns_to_the_right(px(-120.), px(120.)),
            "the end of the range has nothing left to show"
        );
        assert!(
            has_more_columns_to_the_right(px(-119.6), px(120.)),
            "a sub-pixel remainder is still content"
        );
    }

    // The contract's hit width for a divider is 20px. The shared component
    // mounts 8px off-centre, so the table draws its own band at the full width
    // and the pointer has something to find.
    #[gpui::test]
    fn every_column_edge_gets_a_contract_width_resize_band(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let band = cx
            .debug_bounds("column-resize-edge-0")
            .expect("the first column edge");
        assert_eq!(
            f32::from(band.size.width),
            f32::from(design::size::HIT_MIN),
            "the band is the divider hit width, not the divider"
        );
        let edge = cx
            .debug_bounds("table-horizontal-edge")
            .expect("the horizontal scroll edge");
        assert_eq!(f32::from(edge.size.width), f32::from(design::space::XL));
    }

    // The problems filter was a pointer-only control with a 20x22px hit box,
    // which is `design::size::HIT_MIN` with zero margin, so the keyboard route
    // was `Show Only Problems` buried in the column menu.
    #[gpui::test]
    fn the_problems_filter_reaches_the_keyboard_and_hides_healthy_rows(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("status-filter").is_some());

        let filter = view.read_with(cx, |view, _| view.problems_filter_focus.clone());
        cx.update(|window, cx| window.focus(&filter, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.problems_only),
            "Enter on the focused control toggles the filter"
        );
        // Every test pod is Running, so nothing is left to show.
        assert!(cx.debug_bounds("resource-empty").is_some());
        // The key must not reach the clickable column header underneath.
        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).sort()),
            Some(Sort::ascending(0)),
            "the control consumes the key instead of sorting the column"
        );
    }

    // The Overview counts a cluster's problem rows and then has to route to
    // them. Its button opened the Pods table with no way to ask for the filter
    // that page was reporting on, so the reader landed on an unfiltered list of
    // 10,101 rows and had to find the 9,900 themselves.
    #[gpui::test]
    fn an_external_problems_filter_hides_the_healthy_rows_and_comes_back(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(mixed_health_factory(), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("resource-row-0").is_some());
        assert!(cx.debug_bounds("resource-row-1").is_some());
        assert!(cx.debug_bounds("resource-row-2").is_some());

        view.update(cx, |view, cx| view.set_problems_only(true, cx));
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.problems_only),
            "the caller sets the filter the same way the header does"
        );
        assert!(
            cx.debug_bounds("resource-row-0").is_some(),
            "the one pod that needs attention is the row that is kept"
        );
        assert!(
            cx.debug_bounds("resource-row-1").is_none(),
            "a healthy row is hidden, not merely pushed down"
        );
        assert!(
            cx.debug_bounds("resource-empty").is_none(),
            "one matching row is a list, not an empty state"
        );
        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).sort()),
            Some(Sort::ascending(0)),
            "the status filter is not a name filter, so it reorders nothing"
        );

        // Asking for the state the view is already in is what a caller does when
        // it re-routes to a tab it may have filtered before.
        view.update(cx, |view, cx| view.set_problems_only(true, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("resource-row-1").is_none(),
            "setting the same value again changes nothing"
        );

        view.update(cx, |view, cx| view.set_problems_only(false, cx));
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, _| view.problems_only));
        assert!(
            cx.debug_bounds("resource-row-2").is_some(),
            "clearing the filter brings the healthy rows back"
        );

        // A row carries a status whether or not the Status column is on screen,
        // so hiding the column takes the control away and leaves the filter
        // working. The other way round, the filter silently stopped filtering.
        view.update(cx, |view, cx| {
            view.toggle_column_visibility(2, cx);
            view.set_problems_only(true, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("status-filter").is_none(),
            "the control belongs to the column that is no longer there"
        );
        assert!(
            cx.debug_bounds("resource-row-1").is_none(),
            "a hidden status column does not turn the filter into no filter"
        );
    }

    // The shared table header pads its cells by 4px, so the header's column rail
    // sat 4px right of the rail on the body rows: two marks for one column edge.
    #[gpui::test]
    fn the_header_rail_sits_on_the_same_edge_as_the_row_rail(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        // Down selects the first row, so the row's rail is on screen; the first
        // column is the selected one by default, so the header's rail is too.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let row = cx
            .debug_bounds("resource-row-rail-0")
            .expect("the row rail");
        let column = cx
            .debug_bounds("resource-column-rail-0")
            .expect("the selected column's rail");
        assert_eq!(
            column.left(),
            row.left(),
            "one rail per column edge: the header and the body must agree"
        );
    }

    // The control's box is a fixed square, so the focus border cannot move the
    // glyph, and the hit area has margin instead of sitting exactly on the
    // 20px minimum.
    #[gpui::test]
    fn the_problems_filter_has_a_square_hit_area_bigger_than_its_glyph(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let control = cx.debug_bounds("status-filter").expect("the filter");
        let hit = f32::from(design::size::HIT_MIN);
        let marker = f32::from(design::size::STATUS_MARKER);
        assert_eq!(control.size.width, control.size.height, "a square hit area");
        assert_eq!(f32::from(control.size.width), hit);
        assert!(
            f32::from(control.size.width) > marker,
            "a click one pixel off the glyph has to land on the control"
        );
    }

    // The live table numbered its first data row 1 while the loading skeleton
    // numbered it 2, so a screen reader reported the wrong row after an action.
    // Both states now read one function.
    #[test]
    fn both_table_states_number_their_rows_after_the_header() {
        assert_eq!(grid_row_index(0), 2, "the header is row one");
        assert_eq!(grid_row_index(4), 6);
        // The last data row is the count, so the two states agree on the size
        // as well as on the numbering.
        assert_eq!(grid_row_count(3), grid_row_index(2));
        assert_eq!(grid_row_count(0), 1, "a header on its own is one row");
    }

    #[test]
    fn toolbar_uses_compact_mode_at_supported_minimum_width() {
        assert!(is_compact_width(px(960.0)));
        assert!(!is_compact_width(px(1200.0)));
    }

    #[test]
    fn keyboard_navigation_clamps_to_row_bounds() {
        assert_eq!(target_index(Move::Down, None, 10, 5), 0);
        assert_eq!(target_index(Move::Down, Some(9), 10, 5), 9);
        assert_eq!(target_index(Move::Up, Some(0), 10, 5), 0);
        assert_eq!(target_index(Move::Up, None, 10, 5), 0);
        assert_eq!(target_index(Move::Home, Some(7), 10, 5), 0);
        assert_eq!(target_index(Move::End, Some(1), 10, 5), 9);
        assert_eq!(target_index(Move::PageDown, Some(2), 100, 20), 22);
        assert_eq!(target_index(Move::PageDown, Some(95), 100, 20), 99);
        assert_eq!(target_index(Move::PageUp, Some(3), 100, 20), 0);
    }

    #[test]
    fn column_navigation_clamps_to_bounds() {
        assert_eq!(target_column(0, 1, 10), 1);
        assert_eq!(target_column(0, -1, 10), 0);
        assert_eq!(target_column(9, 1, 10), 9);
        assert_eq!(target_column(3, -1, 10), 2);
        assert_eq!(target_column(0, 1, 0), 0);
    }

    #[test]
    fn counts_use_thousands_separators() {
        assert_eq!(design::format::count(0), "0");
        assert_eq!(design::format::count(999), "999");
        assert_eq!(design::format::count(1_000), "1,000");
        assert_eq!(design::format::count(1_031), "1,031");
        assert_eq!(design::format::count(10_000), "10,000");
        assert_eq!(design::format::count(1_234_567), "1,234,567");
        // The two halves of a filtered count each carry the separator.
        assert_eq!(toolbar_count(false, 1_031, 10_010), "1,031 / 10,010");
        // A count with a noun reads right in the singular too.
        assert_eq!(
            design::format::count_with_noun(1, "row selected", "rows selected"),
            "1 row selected"
        );
        assert_eq!(
            design::format::count_with_noun(3, "row selected", "rows selected"),
            "3 rows selected"
        );
    }

    #[test]
    fn operation_failures_report_unknown_results_for_every_mutating_action() {
        for label in ["Delete pod-0", "Scale web to 4", "Restart web"] {
            let message = PodsView::operation_result_unknown_message(label);
            assert!(message.contains("result is unknown"), "{message}");
            assert!(message.contains("Refresh"), "{message}");
            assert!(!message.contains("was not changed"), "{message}");
        }
    }

    #[test]
    fn toolbar_count_is_syncing_during_initial_list() {
        assert_eq!(toolbar_count(true, 0, 9_732), "9,732 / loading…");
        assert_eq!(toolbar_count(true, 100, 9_732), "9,732 / loading…");
        assert_eq!(toolbar_count(false, 9_732, 9_732), "9,732");
        assert_eq!(toolbar_count(false, 12, 9_732), "12 / 9,732");
    }

    // A failed request removes the pending state and shows an error.
    #[gpui::test]
    fn failed_delete_rolls_back_pending_and_reports(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::new(test_factory(Arc::clone(&subscribes)), None, cx)
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let target = view
            .read_with(cx, |view, cx| view.selection_ref(cx))
            .expect("selected target");

        view.update(cx, |view, cx| {
            view.set_ops(Some(Rc::new(FakeOps { delete_error: true })));
            view.request_delete_target(target, cx);
        });
        let uid = view
            .read_with(cx, |view, _| view.selected_uid.clone())
            .expect("Down selects the first row");
        assert!(
            view.read_with(cx, |view, cx| view
                .host
                .read(cx)
                .pending(uid.as_ref())
                .is_some()),
            "the delete request marks pending immediately"
        );

        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, cx| view
                .host
                .read(cx)
                .pending(uid.as_ref())
                .is_none()),
            "failure removes the pending overlay"
        );
        view.read_with(cx, |view, _| {
            let notice = view.notice.as_ref().expect("failure notice");
            assert_eq!(notice.severity, Severity::Error);
            assert!(
                notice.message.contains("result is unknown"),
                "{}",
                notice.message
            );
            assert!(notice.message.contains("Refresh"), "{}", notice.message);
            assert!(!notice.message.contains("server was not changed"));
            assert!(!notice.message.contains("forbidden"));
            assert_eq!(
                notice.detail.as_deref(),
                Some("forbidden: pods \"pod-0\" is protected")
            );
        });
        assert_eq!(subscribes.load(Ordering::Relaxed), 2);
    }

    // An accepted request keeps the pending state until the watch confirms it.
    #[gpui::test]
    fn accepted_delete_keeps_pending_until_watch_confirms(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let target = view
            .read_with(cx, |view, cx| view.selection_ref(cx))
            .expect("selected target");

        view.update(cx, |view, cx| {
            view.set_ops(Some(Rc::new(FakeOps {
                delete_error: false,
            })));
            view.request_delete_target(target, cx);
        });
        cx.run_until_parked();
        let uid = view
            .read_with(cx, |view, _| view.selected_uid.clone())
            .expect("Down selects the first row");
        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).pending(&uid).cloned()),
            Some(PendingOp::Delete),
            "pending stays visible until watch confirmation"
        );
        assert!(view.read_with(cx, |view, _| {
            view.notice.as_ref().is_some_and(|notice| {
                notice.message
                    == "Request sent. The table updates after the server confirms the change."
            })
        }));
    }

    #[gpui::test]
    fn same_uid_operations_are_rejected_while_pending(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let target = view
            .read_with(cx, |view, cx| view.selection_ref(cx))
            .expect("selected target");
        let uid = target.uid.clone();
        let calls = Rc::new(RefCell::new(Vec::new()));

        view.update(cx, |view, cx| {
            view.set_ops(Some(Rc::new(PendingOps {
                calls: Rc::clone(&calls),
            })));
            view.request_delete_target(target.clone(), cx);
            view.request_scale_target(target, 2, cx);
        });
        cx.run_until_parked();

        assert_eq!(calls.borrow().len(), 1);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.host.read(cx).pending_count(), 1);
            assert_eq!(view.host.read(cx).pending(&uid), Some(&PendingOp::Delete));
            assert_eq!(
                view.notice.as_ref().map(|notice| notice.severity),
                Some(Severity::Warning)
            );
        });
    }

    struct PendingOps {
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl ObjectOps for PendingOps {
        fn delete(&self, object: ObjectRef) -> OpsFuture<()> {
            self.calls.borrow_mut().push(object.uid);
            Box::pin(std::future::pending::<Result<(), String>>())
        }

        fn scale(&self, object: ObjectRef, _replicas: i32) -> OpsFuture<()> {
            self.calls
                .borrow_mut()
                .push(format!("scale:{}", object.uid));
            Box::pin(std::future::pending::<Result<(), String>>())
        }

        fn restart(&self, _object: ObjectRef) -> OpsFuture<()> {
            Box::pin(std::future::pending::<Result<(), String>>())
        }
    }

    struct DropFlag(Rc<Cell<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    struct DropOps {
        dropped: Rc<Cell<bool>>,
    }

    impl ObjectOps for DropOps {
        fn delete(&self, _object: ObjectRef) -> OpsFuture<()> {
            let dropped = Rc::clone(&self.dropped);
            Box::pin(async move {
                let _guard = DropFlag(dropped);
                std::future::pending::<Result<(), String>>().await
            })
        }

        fn scale(&self, _object: ObjectRef, _replicas: i32) -> OpsFuture<()> {
            Box::pin(async { Ok(()) })
        }

        fn restart(&self, _object: ObjectRef) -> OpsFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }

    #[gpui::test]
    fn operation_task_is_dropped_after_pending_is_resolved(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let target = view
            .read_with(cx, |view, cx| view.selection_ref(cx))
            .expect("selected target");
        let uid = target.uid.clone();
        let dropped = Rc::new(Cell::new(false));

        view.update(cx, |view, cx| {
            view.set_ops(Some(Rc::new(DropOps {
                dropped: Rc::clone(&dropped),
            })));
            view.request_delete_target(target, cx);
        });
        cx.run_until_parked();
        assert!(!dropped.get());

        view.update(cx, |view, cx| {
            view.host.update(cx, |host, _| host.resolve_pending(&uid));
            view.on_host_changed(cx);
        });
        cx.run_until_parked();
        assert!(dropped.get());
        assert!(view.read_with(cx, |view, _| view.operation_tasks.is_empty()));
    }

    #[gpui::test]
    fn filter_debounce_keeps_the_latest_input(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        view.update(cx, |view, cx| {
            view.on_filter_input("pod-0".to_owned(), cx);
            view.on_filter_input("pod-1".to_owned(), cx);
            assert!(view.filter_task.is_some());
        });
        cx.executor()
            .advance_clock(FILTER_DEBOUNCE + Duration::from_millis(1));
        cx.run_until_parked();

        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).row_count()),
            1
        );
        assert!(!view.read_with(cx, |view, _| view.filter_pending));
    }

    struct FakeOps {
        delete_error: bool,
    }

    impl ObjectOps for FakeOps {
        fn delete(&self, _object: ObjectRef) -> OpsFuture<()> {
            let error = self.delete_error;
            Box::pin(async move {
                if error {
                    Err("forbidden: pods \"pod-0\" is protected".to_owned())
                } else {
                    Ok(())
                }
            })
        }

        fn scale(&self, _object: ObjectRef, _replicas: i32) -> OpsFuture<()> {
            Box::pin(async { Ok(()) })
        }

        fn restart(&self, _object: ObjectRef) -> OpsFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }

    #[gpui::test]
    fn delete_without_confirmation_handler_fails_closed(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        view.update(cx, |view, _cx| {
            view.set_ops(Some(Rc::new(FakeOps {
                delete_error: false,
            })));
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_delete_confirmation(window, cx))
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.host.read(cx).pending_count(), 0);
            assert_eq!(
                view.notice.as_ref().map(|notice| notice.severity),
                Some(Severity::Error)
            );
        });
    }

    #[gpui::test]
    fn row_menu_targets_the_triggered_row(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_row_context_menu(Some(RowTarget::Position(1)), window, cx);
            });
        });

        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-1".into())
        );
    }

    // A row action follows its UID, so a rebuild cannot move it to a neighbour.
    #[gpui::test]
    fn row_actions_follow_the_uid_after_a_rebuild(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        // The last row sorts last by name, so uid-2 sits at index 2.
        let target = RowTarget::Uid("uid-2".into());
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_row_context_menu(Some(target), window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-2".into()),
            "the UID resolves to its own row"
        );

        // The same UID now sits at the top: the menu must still find it.
        view.update(cx, |view, cx| {
            view.host.update(cx, |host, cx| {
                host.dispatch(
                    TableEvent::SortChanged {
                        sort: Some(Sort::descending(0)),
                    },
                    cx,
                )
            });
        });
        cx.run_until_parked();
        let reordered = view.read_with(cx, |view, cx| {
            view.host
                .read(cx)
                .snapshot()
                .and_then(|snapshot| snapshot.by_uid.get("uid-2").copied())
        });
        assert_eq!(reordered, Some(0), "the row moved to the top");

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_row_context_menu(Some(RowTarget::Uid("uid-2".into())), window, cx);
            });
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-2".into()),
            "the menu follows the row, not the old position"
        );
    }

    // A vanished row must not hand its menu to the row that replaced it.
    #[gpui::test]
    fn row_actions_do_not_fall_back_when_the_row_is_gone(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into())
        );

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_row_context_menu(Some(RowTarget::Uid("uid-gone".into())), window, cx);
            });
        });
        assert!(view.read_with(cx, |view, _| view.context_menu.is_none()));
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into()),
            "a missing row leaves the selection alone"
        );
    }

    #[test]
    fn row_targets_resolve_positions_and_uids() {
        let empty = IndexSnapshot::default();
        assert_eq!(RowTarget::Position(0).index(&empty), None);
        assert_eq!(RowTarget::Uid("uid-1".into()).index(&empty), None);

        let snapshot = IndexSnapshot {
            rows: vec![Row {
                obj: test_pod(0),
                cells: Vec::new(),
            }],
            by_uid: HashMap::from([("uid-0".to_owned(), 0usize)]),
            generation: 1,
        };
        assert_eq!(RowTarget::Position(0).index(&snapshot), Some(0));
        assert_eq!(RowTarget::Position(1).index(&snapshot), None);
        assert_eq!(RowTarget::Uid("uid-0".into()).index(&snapshot), Some(0));
        assert_eq!(RowTarget::Uid("uid-9".into()).index(&snapshot), None);
        assert_eq!(
            RowTarget::for_row(&snapshot.rows[0]),
            Some(RowTarget::Uid("uid-0".into()))
        );
    }

    #[gpui::test]
    fn header_context_menu_targets_the_triggered_column(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.column_context_menu(4, window, cx));
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_column), 4);
        assert_eq!(
            view.read_with(cx, |view, _| view.focus_target),
            FocusTarget::Column
        );
    }

    // The menu said `Wider` and `Narrower` with nothing to say which column they
    // act on, and it stacked the width group, the status filter and the column
    // list in one flat run. The entries act on the selected column, so the menu
    // has to name it.
    #[gpui::test]
    fn the_column_menu_names_its_column_and_separates_its_groups(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_selected_column(4, window, cx);
                view.open_header_context_menu(window, cx);
            })
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("MENU_ITEM-Wider Restarts").is_some(),
            "the width entry names the column it resizes"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Narrower Restarts").is_some(),
            "the other width entry names the same column"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Wider").is_none(),
            "an unnamed width entry leaves the reader guessing"
        );
        assert!(cx.debug_bounds("MENU_ITEM-Reset Column Widths").is_some());
        // The Status header owns the filter, so the menu keeps it keyboard
        // reachable under a group of its own.
        assert!(cx.debug_bounds("MENU_ITEM-Show Only Problems").is_some());
    }

    #[gpui::test]
    fn open_details_uses_the_active_row_or_column_focus(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&opened);
        view.update(cx, |view, _| {
            view.on_activate_row(move |row, _, _| {
                sink.borrow_mut().push(row.obj.metadata.name.clone());
            });
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_details(window, cx)));
        assert_eq!(opened.borrow().as_slice(), [Some("pod-0".to_owned())]);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_selected_column(3, window, cx);
                view.open_details(window, cx);
            })
        });
        assert_eq!(opened.borrow().len(), 1);
        assert!(view.read_with(cx, |view, _| view.context_menu.is_some()));
    }

    #[gpui::test]
    fn toolbar_update_toggle_is_driven_by_its_action(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        // The toggle is a button, not a chord, so its tooltip must not invent
        // a binding and must not hardcode a platform modifier.
        let tooltip = cx.update(|_, cx| action_tooltip("Pause Live Updates", &ToggleUpdates, cx));
        assert_eq!(tooltip, "Pause Live Updates");
        assert!(!tooltip.contains("Ctrl"));
        cx.update(|window, cx| window.dispatch_action(Box::new(ToggleUpdates), cx));
        assert_eq!(
            view.read_with(cx, |view, cx| view.status(cx)),
            TableStatus::Paused
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(ToggleUpdates), cx));
        assert_eq!(
            view.read_with(cx, |view, cx| view.status(cx)),
            TableStatus::Streaming
        );
    }

    #[gpui::test]
    fn hiding_the_last_visible_column_points_to_open_columns(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            for index in 1..view.columns.len() {
                view.toggle_column_visibility(index, cx);
            }
            view.toggle_column_visibility(0, cx);
        });
        assert!(view.read_with(cx, |view, _| {
            view.notice
                .as_ref()
                .is_some_and(|notice| notice.message == "Open Columns to show a hidden column.")
        }));
    }

    #[gpui::test]
    fn hiding_the_sorted_column_moves_sort_to_a_visible_fallback(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.host.update(cx, |host, cx| {
                host.dispatch(
                    TableEvent::SortChanged {
                        sort: Some(Sort::descending(1)),
                    },
                    cx,
                )
            });
            view.selected_column = 1;
            view.toggle_column_visibility(1, cx);
        });
        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).sort()),
            Some(Sort::ascending(0))
        );
        assert_eq!(view.read_with(cx, |view, _| view.selected_column), 0);
    }

    // An explicit sort must not stay hidden behind relevance ranking.
    #[gpui::test]
    fn hiding_the_sorted_column_leaves_relevance_mode(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        let filter = view.read_with(cx, |view, _| view.filter.clone());
        filter.update(cx, |input, cx| input.set_text("pod", cx));
        cx.executor()
            .advance_clock(FILTER_DEBOUNCE + Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, cx| view.host.read(cx).relevance()),
            "an active filter ranks by relevance"
        );

        view.update(cx, |view, cx| {
            view.host.update(cx, |host, cx| {
                host.dispatch(
                    TableEvent::SortChanged {
                        sort: Some(Sort::descending(1)),
                    },
                    cx,
                )
            });
            view.relevance_sort = true;
            view.host.update(cx, |host, _| host.set_relevance(true));
            view.selected_column = 1;
            view.toggle_column_visibility(1, cx);
        });

        view.read_with(cx, |view, cx| {
            assert!(!view.host.read(cx).relevance());
            assert!(!view.relevance_sort);
            assert_eq!(
                view.host.read(cx).sort(),
                Some(Sort::ascending(0)),
                "the fallback sort applies to the visible rows"
            );
            assert_eq!(
                sort_mode_label(
                    view.host.read(cx).relevance(),
                    view.host.read(cx).sort(),
                    &view.columns,
                    "pod"
                ),
                "Name ↑",
                "the toolbar reports the sort instead of Relevance"
            );
        });
    }

    // Every recovery button disappears with the state it repairs, so focus
    // must not stay on it. The toolbar Retry had no focus handle before.
    #[gpui::test]
    fn every_recovery_button_hands_focus_back_to_the_table(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.host.update(cx, |host, cx| {
                host.report_watch_error("watch ended".to_owned(), cx)
            });
        });
        cx.run_until_parked();
        assert!(matches!(
            view.read_with(cx, |view, cx| view.status(cx)),
            TableStatus::Stale(_)
        ));

        let (stale_retry, error_retry, empty_action, table) = view.read_with(cx, |view, cx| {
            (
                view.stale_retry_focus.clone(),
                view.retry_focus.clone(),
                view.empty_action_focus.clone(),
                view.table_focus_handle(cx),
            )
        });

        for (index, handle) in [stale_retry, error_retry, empty_action]
            .into_iter()
            .enumerate()
        {
            cx.update(|window, cx| {
                window.focus(&handle, cx);
                assert!(
                    view.read_with(cx, |view, _| view.focus_restore_for_recovery(window)),
                    "recovery control {index} must hand focus back to the table"
                );
            });
        }

        // A refresh from the table or a toolbar keeps the existing focus.
        cx.update(|window, cx| {
            window.focus(&table, cx);
            assert!(
                !view.read_with(cx, |view, _| view.focus_restore_for_recovery(window)),
                "the table keeps the focus it already had"
            );
        });
    }

    // A live table with no rows offers one way forward.
    #[gpui::test]
    fn empty_live_table_recovers_with_a_refresh_action(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let factory: SourceFactory = Box::new({
            let subscribes = Arc::clone(&subscribes);
            move || {
                Box::new(CountingEmptySource {
                    subscribes: Arc::clone(&subscribes),
                }) as Box<dyn ResourceSource>
            }
        });
        let (view, cx) = cx.add_window_view(|_window, cx| PodsView::new(factory, None, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, cx| view.row_count(cx)), 0);
        assert_eq!(
            view.read_with(cx, |view, cx| view.status(cx)),
            TableStatus::Streaming
        );
        assert_eq!(subscribes.load(Ordering::Relaxed), 1);

        let action = view.read_with(cx, |view, _| view.empty_action_focus.clone());
        cx.update(|window, cx| window.focus(&action, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(
            subscribes.load(Ordering::Relaxed),
            2,
            "the recovery action opens a new watch"
        );
    }

    // The reconcile deadline names the rows it could not confirm.
    #[gpui::test]
    fn pending_deadline_names_the_unconfirmed_resource(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let target = view
            .read_with(cx, |view, cx| view.selection_ref(cx))
            .expect("selected target");

        view.update(cx, |view, cx| {
            view.set_ops(Some(Rc::new(PendingOps {
                calls: Rc::new(RefCell::new(Vec::new())),
            })));
            view.request_delete_target(target, cx);
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, cx| view.pending_count(cx)) > 0);

        cx.executor().advance_clock(
            crate::table_view::host::PENDING_OPERATION_TIMEOUT + Duration::from_secs(2),
        );
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            let notice = view.notice.as_ref().expect("deadline notice");
            assert_eq!(notice.severity, Severity::Error);
            assert!(
                notice.message.contains("Delete pod-0"),
                "the notice names the row: {}",
                notice.message
            );
            assert!(notice.message.contains("Refresh"), "{}", notice.message);
            assert_eq!(view.pending_count(cx), 0, "the badge is cleared");
        });
    }

    // Delete acts on the highlighted row and keeps the selection in step.
    #[gpui::test]
    fn delete_acts_on_the_focused_row_without_a_prior_selection(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            None
        );

        let deleted = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&deleted);
        view.update(cx, |view, _| {
            view.on_delete_requested(move |target, _, _| {
                sink.borrow_mut().push(target.object.uid.to_string());
            });
        });

        cx.simulate_keystrokes("delete");

        assert_eq!(deleted.borrow().as_slice(), ["uid-0"]);
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into()),
            "the row that Delete used becomes the selection"
        );
    }

    #[gpui::test]
    fn f10_opens_the_row_context_menu(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);

        cx.simulate_keystrokes("f10");

        assert!(view.read_with(cx, |view, _| view.context_menu.is_some()));
    }

    /// The trailing ellipsis button is gone, so the row menu is the only place a
    /// pointer reaches a row's details. It has to be there, and it has to be
    /// first, because it is what most readers want from a row they right-clicked.
    #[gpui::test]
    fn the_row_menu_offers_open_details(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&opened);
        view.update(cx, |view, _| {
            view.on_activate_row(move |row, _, _| {
                sink.borrow_mut().push(row.obj.metadata.name.clone());
            });
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();

        let item = cx
            .debug_bounds("MENU_ITEM-Open Details")
            .expect("the row menu opens details");
        assert!(item.size.width > px(0.0));
        assert!(
            cx.debug_bounds("resource-row-action-button").is_none(),
            "the trailing ellipsis button is gone"
        );
        cx.simulate_click(item.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(opened.borrow().as_slice(), [Some("pod-0".to_owned())]);
    }

    // `Open Details` and `Describe` carried byte-identical handlers: two labels,
    // two icons, one behaviour. The row menu is the only place a pointer reaches
    // a row's details now that the trailing button is gone, so a duplicate entry
    // there is a duplicate path, not a duplicate label.
    #[gpui::test]
    fn the_row_menu_has_one_entry_that_opens_details(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(cx.debug_bounds("MENU_ITEM-Open Details").is_some());
        assert!(
            cx.debug_bounds("MENU_ITEM-Describe").is_none(),
            "two labels for one action is a duplicate path"
        );
    }

    // The health glyph is the only place the verdict is drawn and it had no
    // accessible name, while the secondary confidence marker beside it had a
    // role, a label, a description and a tooltip. A screen reader read
    // "Status: Pending" and never learned that Pending is a warning.
    #[gpui::test]
    fn the_health_glyph_leads_the_status_cell_at_the_marker_size(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let status_index = pod_columns()
            .iter()
            .position(|column| column.column.id == "status")
            .expect("status column");
        assert_eq!(status_index, 2, "the health glyph leads the status cell");
        let glyph = cx
            .debug_bounds("resource-row-health-0")
            .expect("the health glyph is its own element");
        assert_eq!(
            f32::from(glyph.size.width),
            f32::from(design::size::STATUS_MARKER),
            "a 12px glyph beside 13px body text is not the marker size"
        );
        assert_eq!(
            design::size::STATUS_MARKER,
            design::size::ICON,
            "the marker size and the icon size are one token"
        );
        // A row that answered gets no confidence marker, so the two channels do
        // not both paint on every row.
        assert!(cx.debug_bounds("resource-row-confidence-0").is_none());
    }

    #[gpui::test]
    fn row_menu_opens_service_account_after_open_details(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        let targets = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&targets);
        view.update(cx, |view, _| {
            view.on_service_account_requested(move |target, _, _| {
                sink.borrow_mut().push(target);
            });
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();

        let open_details = cx
            .debug_bounds("MENU_ITEM-Open Details")
            .expect("Open Details menu item");
        let service_account = cx
            .debug_bounds("MENU_ITEM-Open Service Account")
            .expect("Open Service Account menu item");
        assert!(service_account.top() > open_details.top());
        cx.simulate_click(service_account.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            targets.borrow().as_slice(),
            [ServiceAccountTarget {
                namespace: "default".to_owned(),
                name: "default".to_owned(),
            }]
        );
    }

    #[gpui::test]
    fn row_menu_hides_service_account_without_a_pod_handler(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(cx.debug_bounds("MENU_ITEM-Open Details").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Open Service Account").is_none());
    }

    fn test_service(index: usize) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {
                    "name": format!("svc-{index}"),
                    "namespace": "default",
                    "uid": format!("svc-uid-{index}"),
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": {
                    "type": "ClusterIP",
                    "clusterIP": "10.96.0.10",
                    "ports": [{ "port": 80, "protocol": "TCP" }],
                },
            }))
            .expect("test service"),
        )
    }

    struct ServiceSource;

    impl ResourceSource for ServiceSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(SourceEvent::Init);
            for index in 0..2 {
                let _ = events.send(SourceEvent::Store(StoreEvent {
                    op: StoreOp::Apply,
                    obj: test_service(index),
                }));
            }
            let _ = events.send(SourceEvent::InitDone);
            Box::new(NoopSubscription { _events: events })
        }
    }

    #[gpui::test]
    fn row_menu_hides_service_account_for_other_resource_kinds(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory =
            Box::new(|| Box::new(ServiceSource) as Box<dyn ResourceSource>);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                factory,
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Service", "Services", true),
                cx,
            )
        });
        view.update(cx, |view, _| {
            view.on_service_account_requested(|_, _, _| {});
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(cx.debug_bounds("MENU_ITEM-Open Details").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Open Service Account").is_none());
    }

    // Shift+F10 and the Menu key are the keyboard context-menu keys.
    #[gpui::test]
    fn context_menu_keys_open_the_row_actions(cx: &mut TestAppContext) {
        for keys in ["shift-f10", "menu"] {
            init_app(cx);
            let subscribes = Arc::new(AtomicUsize::new(0));
            let (view, cx) =
                cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
            cx.run_until_parked();
            focus_table(cx, &view);

            cx.simulate_keystrokes(keys);

            assert!(
                view.read_with(cx, |view, _| view.context_menu.is_some()),
                "{keys} must open Row Actions"
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.selected_uid.clone()),
                Some("uid-0".into()),
                "{keys} acts on the focused row"
            );
        }
    }

    #[test]
    fn row_actions_key_matches_only_the_context_menu_keys() {
        let key = |spec: &str| gpui::Keystroke::parse(spec).expect("valid keystroke");
        assert!(is_row_actions_key(&key("f10")));
        assert!(is_row_actions_key(&key("shift-f10")));
        assert!(is_row_actions_key(&key("menu")));
        assert!(!is_row_actions_key(&key("shift-menu")));
        assert!(!is_row_actions_key(&key("ctrl-f10")));
        assert!(!is_row_actions_key(&key("alt-f10")));
        assert!(!is_row_actions_key(&key("secondary-f10")));
        assert!(!is_row_actions_key(&key("enter")));
        assert!(!is_row_actions_key(&key("f11")));
    }

    #[test]
    fn loading_bars_stay_inside_their_column() {
        let inset = 2.0 * f32::from(design::space::SM);
        // A normal column keeps the designed fraction.
        assert_eq!(skeleton_bar_width(px(320.0), 0.65), px(320.0 * 0.65));
        // A narrow column cannot hold the full fraction, so it is capped.
        let narrow = skeleton_bar_width(px(40.0), 0.65);
        assert!(
            f32::from(narrow) <= 40.0 - inset,
            "a narrow column must not draw into the next one"
        );
        assert!(f32::from(narrow) > 0.0);
        // A column narrower than its own padding draws nothing.
        assert_eq!(skeleton_bar_width(px(8.0), 0.9), px(0.0));
        assert_eq!(skeleton_bar_width(px(100.0), 0.0), px(0.0));
    }

    #[test]
    fn only_wide_cells_get_tooltips() {
        let columns = crate::table_view::pod_columns();
        let char_width = 12.0 * CHAR_WIDTH_RATIO;
        let name = columns
            .iter()
            .find(|column| column.column.id == "name")
            .expect("name column");
        assert!(!needs_tooltip(
            Some(name),
            "web-0a1b2c3d4e",
            px(name.width),
            char_width
        ));
        assert!(needs_tooltip(
            Some(name),
            &"a".repeat(name.width as usize),
            px(name.width),
            char_width
        ));
        let restarts = columns
            .iter()
            .find(|column| column.column.id == "restarts")
            .expect("restarts column");
        assert!(needs_tooltip(
            Some(restarts),
            &"9".repeat(40),
            px(restarts.width),
            char_width
        ));
        // The same value needs the tooltip at a larger font size. A long
        // generated Pod name is the value that actually reaches the cell edge,
        // and it is the one a reader has to read in full.
        let long_name = "web-0a1b2c3d4e-5f8c9d7b6a-queue-worker-7f9d";
        assert!(!needs_tooltip(
            Some(name),
            long_name,
            px(name.width),
            char_width
        ));
        assert!(needs_tooltip(
            Some(name),
            long_name,
            px(name.width),
            40.0 * CHAR_WIDTH_RATIO
        ));
    }

    // Rows follow the data font size, so Page Up and Page Down stay in view.
    #[test]
    fn row_height_and_page_size_follow_the_font_size() {
        let compact = design::row_height(px(18.0));
        let large = design::row_height(px(40.0));
        assert_eq!(compact, design::size::ROW, "18px lines keep the floor");
        assert_eq!(large, px(40.0), "large text grows the row");
        let viewport = 560.0;
        assert_eq!(rows_in_viewport(viewport, compact), 20);
        assert_eq!(rows_in_viewport(viewport, large), 14);
        assert_eq!(rows_in_viewport(0.0, large), 1, "one row always fits");
        assert_eq!(
            rows_in_viewport(viewport, px(14.0)),
            40,
            "a shorter row fits more rows"
        );
    }

    #[test]
    fn reveal_uses_the_nearest_edge_and_clamps_the_viewport() {
        let span = column_span(&[100.0, 100.0, 100.0, 100.0], 2).expect("column span");
        assert_eq!(reveal_scroll_position(0.0, 250.0, 400.0, span), 50.0);
        assert_eq!(reveal_scroll_position(50.0, 250.0, 400.0, span), 50.0);
        let first = column_span(&[100.0, 100.0, 100.0, 100.0], 0).expect("first span");
        assert_eq!(reveal_scroll_position(50.0, 250.0, 400.0, first), 0.0);
        assert_eq!(reveal_scroll_position(999.0, 250.0, 400.0, span), 150.0);
        assert_eq!(reveal_scroll_position(40.0, 250.0, 200.0, span), 0.0);
    }

    #[test]
    fn reveal_keeps_an_oversized_column_at_its_leading_edge() {
        let span = (25.0, 325.0);
        assert_eq!(reveal_scroll_position(100.0, 200.0, 400.0, span), 25.0);
    }

    #[test]
    fn header_accessibility_includes_sort_direction_and_actions() {
        let ascending = header_accessibility_label("Name", SortAffordance::Ascending);
        assert!(ascending.contains("Name"));
        assert!(ascending.contains("low to high"));
        let description = header_accessibility_description("Name", SortAffordance::Descending);
        assert!(description.contains("default order"));
        assert!(description.contains(ROW_ACTIONS_KEYS));
    }

    // A cell is judged by the width its text really takes, so a wide glyph run
    // gets a tooltip and a short value does not.
    #[gpui::test]
    fn cell_overflow_follows_the_shaped_width(cx: &mut TestAppContext) {
        init_app(cx);
        let typography = cx.update(|cx| DataTypography::from_theme_settings(cx));
        let columns = crate::table_view::pod_columns();
        let name = columns
            .iter()
            .find(|column| column.column.id == "name")
            .expect("name column");
        let width = px(name.width);
        let char_width = f32::from(typography.size) * CHAR_WIDTH_RATIO;
        let available = cell_text_width(Some(name), width);

        // No measurement: the character estimate answers the question.
        assert!(!cell_text_overflows(
            Some(name),
            "web-0",
            width,
            char_width,
            None
        ));
        // A measured value inside the cell never needs a tooltip, even when the
        // estimate was too pessimistic.
        assert!(!cell_text_overflows(
            Some(name),
            &"a".repeat(400),
            width,
            char_width,
            Some(px(available - 1.0))
        ));
        // A measured value wider than the cell does, even when the estimate
        // was too optimistic.
        assert!(cell_text_overflows(
            Some(name),
            "web-0",
            width,
            char_width,
            Some(px(available + 1.0))
        ));
    }

    // Shaping only runs where the estimate cannot decide, so the common case
    // stays cheap and the boundary case stays exact.
    #[gpui::test]
    fn shaping_runs_only_near_the_cell_edge(cx: &mut TestAppContext) {
        init_app(cx);
        // Shaping needs a window, so the app context steps aside for one.
        let cx = cx.add_empty_window();
        let typography = cx.update(|_, cx| DataTypography::from_theme_settings(cx));
        let char_width = f32::from(typography.size) * CHAR_WIDTH_RATIO;
        let width = px(320.0);
        let short = "web-0";
        let long = "w".repeat(200);
        cx.update(|window, _cx| {
            let short_width = measure_cell_text(window, short, &typography, width, char_width)
                .expect("short value");
            let long_width = measure_cell_text(window, &long, &typography, width, char_width)
                .expect("long value");
            assert_eq!(
                f32::from(short_width),
                short.chars().count() as f32 * char_width,
                "a value far inside the cell keeps the estimate"
            );
            assert!(
                f32::from(long_width) > f32::from(width),
                "a value far outside the cell still overflows"
            );
        });
    }

    // The pending badge shows the deadline, so a slow request is not silent.
    #[test]
    fn pending_rows_count_down_to_the_deadline() {
        let pending = PendingState {
            op: PendingOp::Delete,
            remaining: Duration::from_secs(18),
        };
        let detail = pending.detail();
        assert!(detail.contains("Delete requested."), "{detail}");
        assert!(detail.contains("stops waiting in 18 seconds"), "{detail}");

        let expired = PendingState {
            op: PendingOp::Restart,
            remaining: Duration::ZERO,
        };
        assert!(
            expired.detail().contains("deadline has passed"),
            "{}",
            expired.detail()
        );
    }

    /// Every column of a row has to read as one row, and the first thing that
    /// breaks that is text sitting at different heights across the columns.
    ///
    /// `ui::Table` wraps each cell in a block `div`, and a block container puts
    /// its child at the top and sizes it to content. A cell left to that default
    /// is only as tall as its own line plus padding, so the value hugged the top
    /// border with the slack dumped underneath it, and the data line and the
    /// status line — different roles, different heights — never met on one center
    /// line. Measured with the shipping 12px data face: the data cell drew 22px
    /// and the status cell 20px inside a 28px row, so the columns sat 3px and 4px
    /// above the row's own center and 1px apart from each other.
    ///
    /// A data cell's bounds are its line box, so a cell that centers the line is
    /// a cell whose own bounds sit on the row's center. The status cell packs a
    /// glyph beside a shorter line, so it centers with flex and the assertion
    /// reads the glyph, which is a child whose own bounds are observable.
    ///
    /// The product typography is installed here rather than left to the theme
    /// default, because a 15px test font fills a 28px row on its own and hides
    /// the very slack this is about.
    #[gpui::test]
    fn every_cell_shares_the_vertical_center_of_its_row(cx: &mut TestAppContext) {
        assert_cell_centers_match_the_row(cx, None);
    }

    /// The row grows with the configured data line, so the cells have to keep
    /// their center when the reader raises the size rather than only at 12px.
    #[gpui::test]
    fn a_raised_data_font_keeps_every_cell_on_the_row_center(cx: &mut TestAppContext) {
        assert_cell_centers_match_the_row(cx, Some(20));
    }

    fn assert_cell_centers_match_the_row(cx: &mut TestAppContext, data_font_size: Option<u8>) {
        init_app(cx);
        cx.update(crate::settings::install_product_typography_defaults);
        if let Some(size) = data_font_size {
            cx.update(|cx| {
                crate::settings::SettingsStore::update(cx, |store, cx| {
                    store
                        .set_user_settings(&format!(r#"{{ "buffer_font_size": {size} }}"#), cx)
                        .result()
                        .expect("the data size applies");
                });
            });
        }
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        let typography = cx.update(|_, cx| DataTypography::from_theme_settings(cx));
        let row_height = typography.row_height();
        if data_font_size.is_some() {
            assert!(
                row_height > design::size::ROW,
                "the fixture has to actually raise the row, or this test proves nothing"
            );
        } else {
            assert_eq!(row_height, design::size::ROW, "the shipping row is 28px");
            assert!(
                typography.line_height < row_height,
                "18px of line in a 28px row is the slack the defect showed in"
            );
        }
        assert!(
            row_height >= typography.line_height,
            "a row never crops the configured line"
        );

        // The status column is a column, not a fixed position, so the test asks
        // the view where it drew it rather than assuming.
        let status_position = view.read_with(cx, |view, _| {
            view.visible_columns
                .iter()
                .position(|&index| view.columns[index].column.id == "status")
        });
        let status_position = status_position.expect("the fixture lists a status column");
        let data_positions: Vec<usize> = (0..3)
            .filter(|position| *position != status_position)
            .collect();
        assert!(
            !data_positions.is_empty(),
            "the fixture has a data column to measure"
        );

        let row = cx
            .debug_bounds("resource-row-0")
            .expect("the first row is laid out");
        assert_eq!(
            row.size.height, row_height,
            "the row is on the shared rhythm"
        );
        let tolerance = px(0.5);
        for position in data_positions {
            // `debug_bounds` takes a `&'static str`; a test-only leak of a
            // selector is cheaper than a second, harder-to-read probe element.
            let selector: &'static str =
                Box::leak(format!("resource-data-cell-0-{position}").into_boxed_str());
            let cell = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is laid out"));
            assert!(
                (cell.center().y - row.center().y).abs() <= tolerance,
                "the data cell in column {position} drew its line at y={} in a row centered \
                 on y={}",
                f32::from(cell.center().y),
                f32::from(row.center().y)
            );
        }
        let status_selector: &'static str =
            Box::leak(format!("resource-status-cell-0-{status_position}").into_boxed_str());
        let status = cx
            .debug_bounds(status_selector)
            .unwrap_or_else(|| panic!("{status_selector} is laid out"));
        assert!(
            (status.center().y - row.center().y).abs() <= tolerance,
            "the status cell drew at y={} in a row centered on y={}",
            f32::from(status.center().y),
            f32::from(row.center().y)
        );
        // The status cell centers a glyph and a line side by side, so its own box
        // is not its line: the glyph is the observable half of that centering.
        let glyph = cx
            .debug_bounds("resource-row-health-0")
            .expect("the status glyph is laid out");
        assert!(
            (glyph.center().y - row.center().y).abs() <= tolerance,
            "the status glyph sits at y={} in a row centered on y={}",
            f32::from(glyph.center().y),
            f32::from(row.center().y)
        );
    }

    /// The selection rail stands off the table's leading edge, and the first
    /// cell carries enough padding to clear the widest rail. Without the gutter
    /// the rail butts into the sidebar divider and reads as a border of the
    /// window; without the padding it would sit under the first value.
    #[test]
    fn the_rail_stands_off_the_leading_edge_and_the_first_cell_clears_it() {
        assert!(RAIL_GUTTER >= design::space::XS);
        assert!(
            first_cell_leading_pad() > RAIL_GUTTER + design::border::TABLE_FOCUS_RAIL,
            "the first cell's text must clear the widest rail plus a gap"
        );
        // The pad is gutter + rail + one gap, so nothing else has crept in.
        assert_eq!(
            f32::from(first_cell_leading_pad()),
            f32::from(RAIL_GUTTER)
                + f32::from(design::border::TABLE_FOCUS_RAIL)
                + f32::from(design::space::XS)
        );
    }

    #[test]
    fn numeric_and_size_columns_use_right_alignment() {
        let mut columns = pod_columns();
        let name = columns.iter().position(|column| column.column.id == "name");
        let numeric = columns
            .iter()
            .position(|column| column.column.id == "restarts");
        assert!(!cell_is_right_aligned(&columns[name.expect("name")]));
        assert!(cell_is_right_aligned(&columns[numeric.expect("restarts")]));
        columns[0].column.id = "memory-bytes".to_owned();
        assert!(cell_is_right_aligned(&columns[0]));
    }

    #[test]
    fn horizontal_scroll_hit_padding_uses_the_shared_twenty_pixel_token() {
        assert_eq!(f32::from(horizontal_scroll_hit_padding()), 20.0);
    }

    fn init_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
    }

    fn focus_table(cx: &mut gpui::VisualTestContext, view: &gpui::Entity<PodsView>) {
        let handle = view.read_with(cx, |view, cx| view.table_focus_handle(cx));
        cx.update(|window, cx| window.focus(&handle, cx));
    }

    fn test_pod(index: usize) -> Arc<DynamicObject> {
        test_pod_in_phase(&format!("pod-{index}"), &format!("uid-{index}"), "Running")
    }

    /// `uid` is passed rather than derived from `name`: a uid is a row's identity
    /// and a name is what it is called, and deriving one from the other made a
    /// rename look like a different object.
    fn test_pod_in_phase(name: &str, uid: &str, phase: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": uid,
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": {
                    "nodeName": "node-01",
                    "containers": [{ "name": "app", "image": "app:1" }],
                },
                "status": {
                    "phase": phase,
                    "podIP": "10.244.0.1",
                    "containerStatuses": [{
                        "name": "app",
                        "ready": true,
                        "restartCount": 0,
                        "state": { "running": {} },
                    }],
                },
            }))
            .expect("test pod"),
        )
    }

    // Sends three rows synchronously for deterministic tests.
    struct TestSource {
        subscribes: Arc<AtomicUsize>,
    }

    struct NoopSubscription {
        _events: UnboundedSender<SourceEvent>,
    }

    impl Subscription for NoopSubscription {
        fn cancel(&mut self) {}
    }

    impl ResourceSource for TestSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            for event in self.events() {
                let _ = events.send(event);
            }
            Box::new(NoopSubscription { _events: events })
        }

        /// Delivers the rows on the bounded channel the view actually reads.
        /// The default `subscribe_bounded` bridges the legacy unbounded channel
        /// from a real thread, so a test could still read the loading state
        /// after the rows were sent.
        fn subscribe_bounded(
            &mut self,
            events: tokio::sync::mpsc::Sender<SourceEvent>,
        ) -> Box<dyn Subscription> {
            for event in self.events() {
                let _ = events.try_send(event);
            }
            Box::new(BoundedNoopSubscription { _events: events })
        }
    }

    impl TestSource {
        fn events(&mut self) -> Vec<SourceEvent> {
            self.subscribes.fetch_add(1, Ordering::Relaxed);
            let mut events = vec![SourceEvent::Init];
            events.extend((0..3).map(|index| {
                SourceEvent::Store(StoreEvent {
                    op: StoreOp::Apply,
                    obj: test_pod(index),
                })
            }));
            events.push(SourceEvent::InitDone);
            events
        }
    }

    /// Keeps the bounded channel open so the watch loop sees an idle source
    /// rather than a closed one.
    struct BoundedNoopSubscription {
        _events: tokio::sync::mpsc::Sender<SourceEvent>,
    }

    impl Subscription for BoundedNoopSubscription {
        fn cancel(&mut self) {}
    }

    fn test_factory(subscribes: Arc<AtomicUsize>) -> SourceFactory {
        Box::new(move || {
            Box::new(TestSource {
                subscribes: Arc::clone(&subscribes),
            }) as Box<dyn ResourceSource>
        })
    }

    /// Lists one pod that needs attention among two that do not.
    ///
    /// `pod-0-pending` sorts ahead of the other two, so the only row the
    /// problems filter keeps is also the row it puts first.
    struct MixedHealthSource;

    impl ResourceSource for MixedHealthSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            for event in self.events() {
                let _ = events.send(event);
            }
            Box::new(NoopSubscription { _events: events })
        }

        fn subscribe_bounded(
            &mut self,
            events: tokio::sync::mpsc::Sender<SourceEvent>,
        ) -> Box<dyn Subscription> {
            for event in self.events() {
                let _ = events.try_send(event);
            }
            Box::new(BoundedNoopSubscription { _events: events })
        }
    }

    impl MixedHealthSource {
        fn events(&mut self) -> Vec<SourceEvent> {
            let mut events = vec![SourceEvent::Init];
            for (name, phase) in [
                ("pod-0-pending", "Pending"),
                ("pod-1", "Running"),
                ("pod-2", "Running"),
            ] {
                events.push(SourceEvent::Store(StoreEvent {
                    op: StoreOp::Apply,
                    obj: test_pod_in_phase(name, &format!("uid-{name}"), phase),
                }));
            }
            events.push(SourceEvent::InitDone);
            events
        }
    }

    fn mixed_health_factory() -> SourceFactory {
        Box::new(|| Box::new(MixedHealthSource) as Box<dyn ResourceSource>)
    }

    struct EmptySource;

    impl ResourceSource for EmptySource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(SourceEvent::Init);
            let _ = events.send(SourceEvent::InitDone);
            Box::new(NoopSubscription { _events: events })
        }
    }

    /// Lists no rows but still reaches the live state.
    struct CountingEmptySource {
        subscribes: Arc<AtomicUsize>,
    }

    impl ResourceSource for CountingEmptySource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            self.subscribes.fetch_add(1, Ordering::Relaxed);
            let _ = events.send(SourceEvent::Init);
            let _ = events.send(SourceEvent::InitDone);
            Box::new(NoopSubscription { _events: events })
        }
    }

    #[gpui::test]
    fn empty_pause_resume_action_is_keyboard_operable(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| Box::new(EmptySource) as Box<dyn ResourceSource>);
        let (view, cx) = cx.add_window_view(|_, cx| PodsView::new(factory, None, cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.host
                .update(cx, |host, cx| host.dispatch(TableEvent::Pause, cx));
        });
        cx.run_until_parked();
        let focus = view.read_with(cx, |view, _| view.empty_action_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| view.status(cx)),
            TableStatus::Streaming
        );
    }

    #[gpui::test]
    fn empty_clear_filter_action_is_keyboard_operable(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        let filter = view.read_with(cx, |view, _| view.filter.clone());
        filter.update(cx, |input, cx| input.set_text("missing", cx));
        cx.executor()
            .advance_clock(FILTER_DEBOUNCE + Duration::from_millis(1));
        cx.run_until_parked();
        let focus = view.read_with(cx, |view, _| view.empty_action_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, cx| {
                view.filter
                    .read_with(cx, |input, _| input.text().to_owned())
            }),
            ""
        );
    }

    #[test]
    fn runtime_table_contexts_are_simple() {
        assert!(gpui::KeyContext::parse(TABLE_CONTEXT).is_ok());
        assert!(gpui::KeyContext::parse(TABLE_NAV_CONTEXT).is_ok());
    }

    // Exercises the real key binding and action path.
    #[gpui::test]
    fn table_actions_move_selection_columns_and_refresh(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let factory = test_factory(Arc::clone(&subscribes));
        let inspector = cx.update(|cx| cx.new(|cx| InspectorPanel::new(cx)));
        let notifications = Rc::new(Cell::new(0usize));
        let counter = notifications.clone();
        let _observation =
            cx.update(|cx| cx.observe(&inspector, move |_, _| counter.set(counter.get() + 1)));

        let (view, cx) = cx.add_window_view(|window, cx| {
            let view = PodsView::new_with_inspector(factory, None, Some(inspector.clone()), cx);
            let handle = view.table_focus_handle(cx);
            window.focus(&handle, cx);
            view
        });
        cx.run_until_parked();

        assert_eq!(subscribes.load(Ordering::Relaxed), 1);
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            None
        );

        cx.simulate_keystrokes("down");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into()),
            "Down selects the first row without a selection"
        );
        assert!(
            notifications.get() >= 1,
            "selection changes send YAML to the Inspector"
        );

        cx.simulate_keystrokes("down");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-1".into())
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into())
        );

        assert_eq!(view.read_with(cx, |view, _| view.selected_column), 0);
        cx.simulate_keystrokes("tab");
        assert_eq!(view.read_with(cx, |view, _| view.selected_column), 1);
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(view.read_with(cx, |view, _| view.selected_column), 0);
        cx.simulate_keystrokes("shift-enter");
        assert_eq!(
            view.read_with(cx, |view, cx| view.host.read(cx).sort()),
            Some(Sort::descending(0))
        );

        cx.simulate_keystrokes("f5");
        cx.run_until_parked();
        assert_eq!(
            subscribes.load(Ordering::Relaxed),
            2,
            "F5 Refresh reopens the watch and lists again"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_uid.clone()),
            Some("uid-0".into()),
            "relist preserves selection by UID"
        );

        let deleted = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&deleted);
        view.update(cx, |view, _| {
            view.on_delete_requested(move |target, _, _| {
                sink.borrow_mut().push(target.object.uid.to_string());
            });
        });
        cx.simulate_keystrokes("delete");
        assert_eq!(deleted.borrow().as_slice(), ["uid-0"]);
    }

    #[gpui::test]
    /// Enter and the row menu's `Open Details` both report the activated row.
    /// A click does not, because a click only selects.
    #[gpui::test]
    fn row_activation_reports_the_activated_row(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let factory = test_factory(subscribes);
        let (view, cx) = cx.add_window_view(|_, cx| PodsView::new(factory, None, cx));
        cx.run_until_parked();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);
        view.update(cx, |view, _| {
            view.on_activate_row(move |row, _, _| {
                sink.borrow_mut().push(row.obj.metadata.name.clone());
            });
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.activate_index(0, window, cx));
        });
        assert_eq!(seen.borrow().len(), 1);
        assert_eq!(seen.borrow()[0].as_deref(), Some("pod-0"));
    }
    #[test]
    fn failure_reason_becomes_one_actionable_line() {
        assert_eq!(
            user_reason("forbidden: cannot list pods", "pods"),
            "Forbidden: needs list pods"
        );
        assert_eq!(
            user_reason(
                "Error from server (Forbidden): pods is forbidden",
                "deployments"
            ),
            "Forbidden: needs list deployments"
        );
        assert_eq!(
            user_reason("Unauthorized", "pods"),
            "Not authorized: needs access to pods"
        );
        assert_eq!(
            user_reason("failed to connect to https://127.0.0.1:6443", "pods"),
            "Cannot reach context"
        );
        assert_eq!(
            user_reason("  ", "pods"),
            "The table did not report a reason."
        );
        // An unknown reason keeps one line and never leaks the whole reason.
        let line = user_reason(&"x".repeat(200), "pods");
        assert_eq!(line.chars().count(), USER_REASON_MAX);
        assert!(line.ends_with('\u{2026}'));
        assert_eq!(user_reason("boom\nsecond line", "pods"), "boom");
    }

    // An unrecognised kind has no columns, so an empty list is expected.
    #[gpui::test]
    fn unknown_kind_gets_its_own_empty_state(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| Box::new(EmptySource) as Box<dyn ResourceSource>);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                factory,
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Widget", "Widgets", true),
                cx,
            )
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("resource-empty").is_some(),
            "an unknown kind with no rows uses the empty state"
        );
        view.read_with(cx, |view, cx| {
            assert_eq!(
                empty_state_kind(
                    &view.status(cx),
                    "",
                    view.spec.kind.as_ref(),
                    None,
                    view.problems_only,
                ),
                EmptyState::UnknownKind,
                "an unknown kind is not reported as an empty scope"
            );
            assert_eq!(view.spec.label_lower(), "widgets");
        });
    }

    #[test]
    fn empty_state_kind_separates_every_condition() {
        assert_eq!(
            empty_state_kind(&TableStatus::Listing, "", "Pod", None, false),
            EmptyState::Loading
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Paused, "", "Pod", None, false),
            EmptyState::Paused
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Stale("x".to_owned()), "", "Pod", None, false),
            EmptyState::Stale
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Failed("x".to_owned()), "", "Pod", None, false),
            EmptyState::Failed
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "", "Widget", None, false),
            EmptyState::UnknownKind
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "web", "Pod", None, false),
            EmptyState::NoFilterMatch,
            "a known kind reports the filter, not the kind"
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "", "Pod", None, false),
            EmptyState::NoRows
        );
        assert_eq!(UNKNOWN_KIND_TITLE, "No columns for this kind");
        assert!(UNKNOWN_KIND_GUIDANCE.contains("CRD"));
    }

    // The status filter is the one the reader did not type, so it has to be
    // named before a name that happens not to match.
    #[test]
    fn the_status_filter_is_reported_before_a_name_that_does_not_match() {
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "web", "Pod", None, true),
            EmptyState::NoProblems,
            "a status filter plus an incidental name mismatch blames the status filter"
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "", "Pod", None, true),
            EmptyState::NoProblems
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "web", "Pod", None, false),
            EmptyState::NoFilterMatch,
            "with no status filter the name is the only explanation left"
        );
    }

    // The NoProblems state used to tell the reader to "Clear the filter" for a
    // filter they never typed, which points at the wrong control.
    #[gpui::test]
    fn the_no_problems_state_names_the_status_filter_not_a_typed_one(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        assert_eq!(NO_PROBLEMS_ACTION, "Show all rows");
        assert!(NO_PROBLEMS_GUIDANCE.contains("status filter"));
        assert!(
            !NO_PROBLEMS_GUIDANCE.contains("Clear the filter"),
            "the reader never typed a filter"
        );

        // Every test pod is Running, so the status filter hides all of them and
        // the table reports the state instead of an empty list.
        view.update(cx, |view, cx| view.toggle_problems_filter(cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("resource-empty").is_some());

        // The action behind the button. The button and the keyboard path both
        // dispatch `ClearFilter`, and that is the only thing that has to work.
        let action = view.read_with(cx, |view, _| view.empty_action_focus.clone());
        cx.update(|window, cx| window.focus(&action, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |view, _| view.problems_only),
            "the action turns the status filter off"
        );
        assert!(
            cx.debug_bounds("resource-row-0").is_some(),
            "the rows come back"
        );
    }

    // Shift extends the selection, Control or Command plus A selects every row.
    #[gpui::test]
    fn shift_extends_and_select_all_covers_every_row(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 0);

        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 1);

        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        let (count, uids) = view.read_with(cx, |view, _| {
            (
                view.selection_count(),
                view.selected_uids.iter().cloned().collect::<Vec<_>>(),
            )
        });
        assert_eq!(count, 2, "Shift+Down adds the next row");
        assert_eq!(uids.len(), 2);

        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.selection_count()),
            3,
            "the third row is the last one"
        );

        // A plain arrow resets the selection to one row.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 1);

        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.selection_count()),
            view.read_with(cx, |view, cx| view.row_count(cx)),
            "Control+A selects every loaded row"
        );
    }

    // Nothing on screen said a range was selected: the member fill measured
    // 1.018:1 in dark and 1.022:1 in light against the row beside it, and two
    // captures of the toolbar were pixel-identical. The count is a live region,
    // so the selection is announced as well as drawn.
    #[gpui::test]
    fn the_toolbar_counts_and_announces_the_selection(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        assert!(
            cx.debug_bounds("toolbar-chip-1 selected").is_none(),
            "an empty selection needs no chip"
        );
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 2);
        let chip = cx
            .debug_bounds("toolbar-chip-2 selected")
            .expect("the selection count");
        assert!(chip.size.width > px(0.0));
        // `toolbar_chip` gives the chip `Role::Status`, which is the live region
        // GPUI can express: there is no `aria_live` on the element API, and a
        // polite status region is what announces the count when it changes.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 1);
        assert!(
            cx.debug_bounds("toolbar-chip-1 selected").is_some(),
            "the chip follows the count"
        );
    }

    // A selection that outlives its rows keeps one active row.
    #[gpui::test]
    fn selection_survives_a_snapshot_rebuild(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 2);

        // A filter hides the other rows, so the selection shrinks with them.
        view.update(cx, |view, cx| view.commit_filter("pod-1", cx));
        cx.run_until_parked();
        let (count, active) = view.read_with(cx, |view, _| {
            (view.selection_count(), view.selected_uid.clone())
        });
        assert!(count <= 2);
        if count > 0 {
            assert!(active.is_some(), "an active row survives the rebuild");
        }
    }
    // A multi-row delete needs its own confirmation step.
    #[gpui::test]
    fn multi_row_delete_asks_before_it_acts(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 2);

        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_delete_confirmation(window, cx));
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let request = view.multi_delete.as_ref().expect("confirmation");
            assert_eq!(request.objects.len(), 2);
        });
        assert!(cx.debug_bounds("multi-delete-bar").is_some());
    }

    // The delete bar's marker was the last severity-coloured glyph in the table
    // still at Zed's 12px `XSmall`, directly beside 13px body text, while every
    // other status mark had moved to `design::size::STATUS_MARKER`. A bigger mark
    // must not grow the bar either: the 28px control and the 2px band on each
    // side still decide its height.
    #[gpui::test]
    fn the_delete_bars_marker_matches_the_status_markers(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        // Wide enough that the confirmation sentence stays on one line. The bar's
        // height is being measured to find out whether the mark grew it, and a
        // wrapped sentence grows it for an unrelated reason.
        cx.simulate_resize(gpui::size(px(1600.), px(1000.)));
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_delete_confirmation(window, cx));
        });
        cx.run_until_parked();

        let marker = cx
            .debug_bounds("multi-delete-marker")
            .expect("the delete marker");
        assert_eq!(
            f32::from(marker.size.width),
            f32::from(design::size::STATUS_MARKER),
            "a status mark beside 13px body text is the marker size"
        );
        // The 28px control and the 2px band on each side still decide the bar's
        // height, so a 16px mark costs it nothing. The 1px is the bottom rule.
        let bar = cx.debug_bounds("multi-delete-bar").expect("the bar");
        assert!(
            f32::from(bar.size.height)
                <= f32::from(design::size::CONTROL) + 2.0 * f32::from(design::space::XS) + 1.0,
            "the bar grew to {}px: the mark is now the tallest thing in it",
            bar.size.height
        );
    }

    // Confirming removes the bar, so the focus cannot stay on a button that is
    // no longer on screen.
    #[gpui::test]
    fn confirming_a_multi_row_delete_returns_focus_to_the_table(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.set_ops(Some(Rc::new(FakeOps {
                delete_error: false,
            })));
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_delete_confirmation(window, cx));
        });
        cx.run_until_parked();

        let (confirm, table) = view.read_with(cx, |view, cx| {
            (
                view.multi_delete_confirm_focus.clone(),
                view.table_focus_handle(cx),
            )
        });
        cx.update(|window, cx| window.focus(&confirm, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.confirm_multi_delete(window, cx));
        });
        cx.run_until_parked();

        assert!(view.read_with(cx, |view, _| view.multi_delete.is_none()));
        assert!(
            cx.update(|window, _| table.is_focused(window)),
            "the table takes the focus back"
        );
    }

    // The skeleton keeps the user's column widths and hidden columns, so the
    // first rows land where the live table will put them.
    #[gpui::test]
    fn the_loading_skeleton_follows_the_visible_columns_and_widths(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| {
            Box::new(StartupLoadingSource {
                reason: crate::shell::STARTUP_LOADING_REASON.to_owned(),
            }) as Box<dyn ResourceSource>
        });
        let (view, cx) = cx.add_window_view(|_window, cx| PodsView::new(factory, None, cx));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            // Hide a column the way the Columns menu does, then widen another.
            view.toggle_column_visibility(1, cx);
            view.selected_column = 0;
            view.resize_selected_column(96.0, cx);
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("table-loading-skeleton").is_some());
        assert!(
            cx.debug_bounds("table-skeleton-header-namespace").is_none(),
            "a hidden column gets no skeleton header"
        );
        let name = cx
            .debug_bounds("table-skeleton-header-name")
            .expect("name skeleton header");
        let expected = cx.update(|window, cx| {
            view.read_with(cx, |view, cx| {
                let position = view
                    .visible_columns
                    .iter()
                    .position(|&index| view.columns[index].column.id == "name")
                    .expect("name column stays visible");
                view.visible_widths(window, cx)
                    .get(position)
                    .copied()
                    .expect("visible name width")
            })
        });
        assert_eq!(
            f32::from(name.size.width),
            f32::from(expected),
            "the skeleton uses the width the user chose"
        );
    }

    /// Mirrors `UnavailableSource`: one error, no rows.
    struct StartupLoadingSource {
        reason: String,
    }

    impl ResourceSource for StartupLoadingSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(SourceEvent::Error {
                reason: self.reason.clone(),
            });
            Box::new(NoopSubscription { _events: events })
        }
    }
}

#[cfg(test)]
mod confirmation_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::TestAppContext;
    use k8s_core::controller::{StoreEvent, StoreOp};
    use serde_json::json;
    use theme::LoadThemes;
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::table_view::source::{ResourceSource, SourceEvent, Subscription};

    fn init_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
    }

    fn focus_table(cx: &mut gpui::VisualTestContext, view: &gpui::Entity<PodsView>) {
        let handle = view.read_with(cx, |view, cx| view.table_focus_handle(cx));
        cx.update(|window, cx| window.focus(&handle, cx));
    }

    fn deployment() -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {
                    "name": "web",
                    "namespace": "default",
                    "uid": "deploy-uid-1",
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": { "replicas": 3 },
                "status": { "readyReplicas": 3 },
            }))
            .expect("test deployment"),
        )
    }

    struct DeploymentSource;

    struct NoopSubscription {
        _events: UnboundedSender<SourceEvent>,
    }

    impl Subscription for NoopSubscription {
        fn cancel(&mut self) {}
    }

    impl ResourceSource for DeploymentSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(SourceEvent::Init);
            let _ = events.send(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj: deployment(),
            }));
            let _ = events.send(SourceEvent::InitDone);
            Box::new(NoopSubscription { _events: events })
        }
    }

    // Scale uses the current replica count for supported kinds.
    #[gpui::test]
    fn scale_target_reads_current_replicas_and_gates_kinds(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory =
            Box::new(|| Box::new(DeploymentSource) as Box<dyn ResourceSource>);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                factory,
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Deployment", "Deployments", true).with_resource(
                    kube_core::ApiResource::from_gvk_with_plural(
                        &kube_core::GroupVersionKind::gvk("apps", "v1", "Deployment"),
                        "deployments",
                    ),
                ),
                cx,
            )
        });
        cx.run_until_parked();

        view.read_with(cx, |view, cx| {
            assert!(view.supports_scale());
            assert!(
                view.scale_target(cx).is_none(),
                "Scale requires a selected row"
            );
        });

        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        let seen = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |view, _| {
            let seen = Rc::clone(&seen);
            view.on_scale_requested(move |target, _, _| seen.borrow_mut().push(target));
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_scale_dialog(window, cx));
        });
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1, "Scale must use the confirmation handler");
        assert_eq!(seen[0].object.name, "web");
        assert_eq!(seen[0].object.namespace.as_deref(), Some("default"));
        assert_eq!(seen[0].object.uid, "deploy-uid-1");
        assert_eq!(seen[0].object.resource.plural, "deployments");
        assert_eq!(
            seen[0].replicas, 3,
            "the default replica count comes from spec.replicas"
        );

        // Pods do not support Scale.
        let pod_factory: SourceFactory =
            Box::new(|| Box::new(DeploymentSource) as Box<dyn ResourceSource>);
        let (pods, _) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                pod_factory,
                None,
                None::<InspectorBinding>,
                ResourceSpec::pods(),
                cx,
            )
        });
        pods.read_with(cx, |view, _| {
            assert!(!view.supports_scale(), "Pods must not offer Scale");
        });
    }

    // Delete confirmation receives the full selected object reference.
    #[gpui::test]
    fn delete_confirmation_handler_receives_the_selected_object(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory =
            Box::new(|| Box::new(DeploymentSource) as Box<dyn ResourceSource>);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                factory,
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Deployment", "Deployments", true).with_resource(
                    kube_core::ApiResource::from_gvk_with_plural(
                        &kube_core::GroupVersionKind::gvk("apps", "v1", "Deployment"),
                        "deployments",
                    ),
                ),
                cx,
            )
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");

        let seen = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |view, _| {
            let seen = Rc::clone(&seen);
            view.on_delete_requested(move |target, _, _| seen.borrow_mut().push(target));
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_delete_confirmation(window, cx));
        });
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].object.resource.kind, "Deployment");
        assert_eq!(seen[0].object.resource.plural, "deployments");
        assert_eq!(seen[0].object.name, "web");
        assert_eq!(seen[0].object.namespace.as_deref(), Some("default"));
        assert_eq!(seen[0].object.uid, "deploy-uid-1");
    }
}
