//! Inspector panel for YAML, Describe, Events, and Metrics.
//! Describe and Events load on demand and cache results by UID.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyView, App, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, KeybindingKeystroke, Keystroke, ParentElement,
    Render, Role, ScrollHandle, SharedString, Styled, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window, div, point, px, uniform_list,
};
use k8s_core::metrics::{SampleDecision, SampleScheduler};
use kube_core::DynamicObject;
use serde_json::Value;
use ui::prelude::*;
use ui::{KeyBinding, KeyBinding as UiKeyBinding, KeyBindingStyle, TintColor, Tooltip};

use crate::charts::{ChartTable, LineChartView, Unit};
use crate::design::{self, Severity, space};
pub use crate::session::InspectorSelection;
use crate::session::{InspectorBinding, InspectorBindingInput, InspectorUpdate};
use crate::yaml_editor::{Apply as ApplyYaml, Diagnostic, YamlView};

use super::common;
use super::common::{
    TabSpec, buffer_font, empty_state, label_body, label_section, label_small, label_text, spinner,
    status_message,
};
pub use super::inspector_data::{
    ApplyRequest, ApplyTarget, InspectorApplyRequest, InspectorApplyTarget, InspectorSession,
    SessionIdentity,
};
use super::metrics::{
    DEFAULT_RANGE_MS, METRICS_UNAVAILABLE, MetricsHandle, MetricsProbeState, MetricsSamples,
    MetricsTarget, RANGE_OPTIONS, SamplePayload, retry_delay_text, sampling_interval_text,
};
use crate::session::{ApplyOutcome, DescribeData, InspectorSource, ObjectRef, OpsFuture};

const COPIED_FEEDBACK: Duration = Duration::from_millis(1500);
/// Event cache lifetime.
const EVENTS_TTL: Duration = Duration::from_secs(15);
/// Maximum number of cached UIDs.
const CACHE_CAPACITY: usize = 16;
/// Maximum visible scalar fields before expansion.
const MAX_FIELD_ROWS: usize = 14;
const LABEL_CHIP_LIMIT: usize = 8;
const INSPECTOR_RELOAD_TAB_INDEX: isize = 8;
const METRICS_RANGE_TAB_INDEX: isize = 20;
const METRICS_RETRY_TAB_INDEX: isize = 10;
const INSPECTOR_CONTENT_TAB_INDEX: isize = 11;
const LABELS_EXPAND_TAB_INDEX: isize = 12;
const STATUS_EXPAND_TAB_INDEX: isize = 13;
const SPEC_EXPAND_TAB_INDEX: isize = 14;
/// Focus order of the YAML toolbar actions.
const APPLY_TAB_INDEX: isize = 1;
const REVERT_TAB_INDEX: isize = 2;
const COPY_YAML_TAB_INDEX: isize = 3;
/// The problems list renders directly under the YAML toolbar, so it takes the tab stop right
/// after it. Tab order follows visual order: a list that draws above the review strip has to be
/// reached before the strip, not after the three scroll regions of the other tabs.
const INSPECTOR_PROBLEMS_TAB_INDEX: isize = 4;
/// The review strip is the only place a write can start, so its safe action comes first and
/// keeps the first tab stop. The destructive action never takes focus by itself.
const REVIEW_KEEP_EDITING_TAB_INDEX: isize = 5;
const REVIEW_CHECK_TAB_INDEX: isize = 6;
const REVIEW_APPLY_TAB_INDEX: isize = 7;
const INSPECTOR_DESCRIBE_RETRY_TAB_INDEX: isize = 9;
/// Focus order of the YAML error state's Retry. It replaces the whole tab body, so it cannot share
/// a handle with the toolbar's controls; it takes the stop after the three scroll regions so the
/// YAML tab's own order is unchanged.
const INSPECTOR_YAML_RETRY_TAB_INDEX: isize = 18;
/// Every scrollable region is its own tab stop, so the keyboard can reach the content.
const INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX: isize = 15;
const INSPECTOR_EVENTS_SCROLL_TAB_INDEX: isize = 16;
const INSPECTOR_METRICS_SCROLL_TAB_INDEX: isize = 17;
/// First tab index of the value-row focus pool. The pool is handed out in render order, so the
/// rows follow the toolbar controls in document order.
const VALUE_FOCUS_POOL_TAB_INDEX: isize = 30;
/// Focus handles reserved for the Describe value rows. One Describe body rarely holds more.
const VALUE_FOCUS_POOL_SIZE: usize = 96;
/// ARIA label of the Inspector tab strip.
const INSPECTOR_TAB_LIST_LABEL: &str = "Inspector tabs";
/// The tab strip holds the first tab stop, so focus enters the Inspector on the tabs.
const INSPECTOR_TAB_STRIP_TAB_INDEX: isize = 0;
/// The confirmation names the object a write would touch, so it is never read as a generic
/// "are you sure".
const APPLY_REVIEW_TITLE: &str = "Apply these changes to the cluster?";
const APPLY_UNAVAILABLE_REASON: &str =
    "No cluster is connected, so Apply is unavailable. Connect to a cluster, then retry.";
const APPLY_UNKNOWN_REASON: &str = "The result of the apply is unknown. Refresh the object to see whether the server applied the change.";
const APPLY_INVALID_REASON: &str =
    "The YAML has a problem. Fix every problem in the list, then apply again.";
const APPLY_STALE_REVIEW_REASON: &str =
    "The YAML changed while the review was open. Review the new text, then apply again.";
const RANGE_BUTTON_WIDTH: f32 = 52.0;
const INSPECTOR_ACTION_BUTTON_WIDTH: f32 = 72.0;
const RETRY_BUTTON_WIDTH: f32 = 72.0;
/// Severity marker slot. Always present, so a marked row keeps the same key and value
/// columns as an unmarked one.
const DESCRIBE_MARKER_SLOT: gpui::Pixels = design::size::ICON;
/// Key column of a two-column describe row.
///
/// The widest key a Describe row shows is a field path such as
/// `spec.containers[0].imagePullPolicy`, and a truncated key is unreadable in a way a truncated
/// value is not: the key is how the reader finds the field they are looking for.
///
/// This is a measure, not a rhythm step, so it is not a spacing token: `DESIGN.md` §3.2 covers
/// padding and gaps, and no value in the 4px scale is a readable key column. It is stated here
/// next to the reason instead, and [`DESCRIBE_VALUE_MIN_WIDTH`] is the other half of the pair so
/// the two cannot drift apart.
const DESCRIBE_KEY_WIDTH: f32 = 220.0;
/// Narrowest value column a two-column describe row may leave beside its key.
///
/// A value is YAML, a port range or a condition message. Below this it stops being skimmable,
/// which is the whole reason the two-column layout exists: the stacked layout is what a narrow
/// Inspector gets, and it is already the answer for "there is not enough room".
const DESCRIBE_VALUE_MIN_WIDTH: f32 = 164.0;
/// Narrowest content width that still leaves a readable value column beside the key.
///
/// Derived from the two columns rather than written out, so raising the key column cannot leave
/// the pair promising a two-column layout its own arithmetic says does not fit.
const DESCRIBE_TWO_COLUMN_MIN_WIDTH: f32 = DESCRIBE_KEY_WIDTH + DESCRIBE_VALUE_MIN_WIDTH;
/// Values longer than this move under their key and wrap instead of running off the row.
///
/// Eighty characters is roughly what fits beside a `DESCRIBE_KEY_WIDTH` key inside the widest
/// Inspector (480px - 220px key - the marker slot - gaps), so it is where the shared one-line
/// layout stops being readable. It counts characters, not bytes, because the limit is about the
/// width a reader sees. It is a character count rather than a width because the value is set in
/// the data role, whose size the reader can change: a pixel limit would silently mean something
/// different at every setting, and a character count is what the wrap decision is actually about.
const DESCRIBE_INLINE_VALUE_LIMIT: usize = 80;
const NOT_CONNECTED_REASON: &str = "Not connected to a cluster.";
/// Title of the YAML read failure, which is a different boundary from "no row selected".
const YAML_LOAD_FAILED_TITLE: &str = "Failed to load YAML";
/// Label of the control that asks for the document again.
const YAML_RETRY_LABEL: &str = "Retry loading YAML";
const OBJECT_REPLACED_REASON: &str =
    "The server returned a different object. Reload the resource details.";
/// Reason for a load that is still running after [`LOAD_DEADLINE`].
const LOAD_TIMEOUT_REASON: &str = "The request is taking longer than expected.";
/// How long a `Loading` entry may stay before the tab offers Retry again.
const LOAD_DEADLINE: Duration = Duration::from_secs(10);
/// Height of a metrics chart, and of the sample table under it.
const METRICS_CHART_HEIGHT: f32 = 132.0;
const METRICS_TABLE_HEIGHT: f32 = 168.0;
/// Diff lines rendered in the review strip. A longer change is summarised instead of scrolled,
/// so the review never becomes a scroll region the keyboard cannot reach.
const APPLY_REVIEW_DIFF_LINES: usize = 12;
/// Problems shown at once. A document with dozens of parse problems must not push the editor
/// out of the panel, so the list is capped and scrolls past the cap. Nothing is dropped: the
/// title keeps the real total and the arrow keys walk every problem, so a capped list never
/// hides that Apply is blocked.
const PROBLEMS_VISIBLE_ROWS: usize = 12;
// Toolbar commands. Each one is dispatchable so a keymap or the command palette can
// reach the same entry point as the toolbar buttons.
gpui::actions!(
    k8s_inspector,
    [
        // Reloads the data of the active Describe, Events, or Metrics tab.
        ReloadActiveTab,
        // Shows the last 5 minutes of metrics.
        MetricsRange5m,
        // Shows the last 15 minutes of metrics.
        MetricsRange15m,
        // Shows the last hour of metrics.
        MetricsRange1h,
        // Retries the metrics probe or takes the next sample now.
        RetryMetrics,
        // Confirms the reviewed change and writes it to the cluster.
        ConfirmApply,
        // Drops the review without writing anything.
        CancelApplyReview,
        // Restores the text the cluster last reported.
        RevertYaml,
        // Copies the YAML in the editor.
        CopyYaml,
        // Expands the focused Describe value to its full text, or collapses it again.
        ToggleValueExpansion,
        // Copies the focused Describe value.
        CopyValue,
        // Moves the keyboard cursor to the next YAML problem and reveals it.
        NextProblem,
    ]
);

const TABS: [TabSpec; 3] = [
    TabSpec {
        label: "YAML",
        icon: IconName::FileCode,
    },
    TabSpec {
        label: "Describe",
        icon: IconName::TextSnippet,
    },
    TabSpec {
        label: "Events",
        icon: IconName::Bell,
    },
];

const METRICS_TAB: TabSpec = TabSpec {
    label: "Metrics",
    icon: IconName::SignalHigh,
};

/// User-facing next step for a load failure.
fn load_failure_hint(reason: &str) -> &'static str {
    if reason == NOT_CONNECTED_REASON {
        "Connect to a cluster, then retry."
    } else if reason == OBJECT_REPLACED_REASON {
        "The object was replaced. Select the object again, then reload."
    } else if reason == LOAD_TIMEOUT_REASON {
        "The cluster is slow to answer. Retry, or check the cluster connection."
    } else {
        "Retry, or make sure the cluster connection works."
    }
}

/// Inspector tabs used by commands and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorTab {
    Yaml = 0,
    Describe = 1,
    Events = 2,
    Metrics = 3,
}

#[derive(Clone, Debug)]
enum LoadState<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, Debug)]
struct EventsEntry {
    fetched_at: Instant,
    state: LoadState<Arc<Vec<DynamicObject>>>,
}

/// A selection deferred while YAML has unsaved changes.
#[derive(Clone, Debug)]
enum PendingLoad {
    Yaml(Option<String>),
    Selection(Option<InspectorSelection>),
}

/// One lazily loaded tab, so a stuck request can be tracked separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadKind {
    Describe,
    Events,
}

type ApplyHandler = Box<dyn Fn(ApplyRequest, &mut App)>;
type ApplyCallback = Box<dyn Fn(ApplyRequest)>;
type CheckHandler = std::rc::Rc<dyn Fn(ApplyRequest, &mut App)>;

/// What the server concluded, reduced to what the review needs to say.
pub enum ApplyVerdict {
    Valid,
    Conflict { owners: Vec<String> },
}
type MetricsProbeRetry = Rc<dyn Fn(&mut App)>;
/// Asks whoever owns the document to fetch it again. See [`InspectorPanel::yaml_reload`].
type YamlReload = Rc<dyn Fn(&mut App)>;

#[derive(Clone, Copy, Debug, Default)]
struct ExpandedDetails {
    labels: bool,
    status: bool,
    spec: bool,
}

#[derive(Clone, Copy, Debug)]
enum DetailSection {
    Labels,
    Status,
    Spec,
}

impl DetailSection {
    /// Slot of this section in the toggle focus pool, which follows the tab order.
    fn index(&self) -> usize {
        match self {
            Self::Labels => 0,
            Self::Status => 1,
            Self::Spec => 2,
        }
    }
}

/// A change that passed every guard and waits for a review.
///
/// The panel never writes from the editor keystroke: it captures the text, the target, and the
/// identity that target names, and the review strip is the only way to turn this into a request.
#[derive(Clone, Debug)]
struct PendingApply {
    request_id: u64,
    target: ApplyTarget,
    yaml: String,
}

/// What the API server said when asked to validate the document under review, without storing it.
///
/// A local parse cannot see a schema violation, an immutable field, an unknown enum value, or a
/// missing required field. Those are the failures that surface after an apply has already
/// half-succeeded, which is the worst moment to learn about them, so the review can ask the
/// server first and state its answer before anything is written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ApplyCheckState {
    /// Nobody has asked yet. The review says so rather than implying it was checked.
    #[default]
    NotRun,
    Running,
    /// The server accepted the document and returned the object it would store.
    Valid,
    /// Another field manager owns a field this apply would change.
    Conflict {
        owners: Vec<String>,
    },
    /// The check could not complete. Applying is still allowed; the reason is shown instead.
    Failed {
        reason: String,
    },
}

/// One flattened field: its path, the text it shows, and whether the JSON value is a string.
type FieldRow = (String, String, bool);

/// How a field value is drawn.
///
/// The size follows the section, so one column cannot change size halfway down, and the font
/// follows the value's type, so an address or a digest reads as code while a count does not.
#[derive(Clone, Copy)]
struct ValueStyle {
    data: bool,
    mono: bool,
}

impl ValueStyle {
    /// A value in a data column, drawn in the buffer font when it is a string.
    fn data(is_string: bool) -> Self {
        Self {
            data: true,
            mono: is_string,
        }
    }

    /// A value in a prose column, drawn in the UI font.
    fn prose() -> Self {
        Self {
            data: false,
            mono: false,
        }
    }
}

/// Focus handles for the Describe value rows.
///
/// The handles are created once and handed out in render order, so Tab walks the rows in the
/// order they are read, and a row keeps its handle between renders. A body with more rows than
/// the pool holds gets no handle for the overflow, which costs those rows their keyboard
/// affordance instead of stealing another row's focus.
struct ValueFocus {
    pool: Vec<FocusHandle>,
    next: usize,
    by_selector: HashMap<String, FocusHandle>,
    texts: HashMap<String, String>,
}

impl ValueFocus {
    fn new(pool: Vec<FocusHandle>) -> Self {
        Self {
            pool,
            next: 0,
            by_selector: HashMap::new(),
            texts: HashMap::new(),
        }
    }

    /// Starts a new render pass, so the selector map describes the rows on screen now.
    fn begin_render(&mut self) {
        self.next = 0;
        self.by_selector.clear();
        self.texts.clear();
    }

    /// Reserves the next handle for a row, and remembers the text a copy would take.
    fn take(&mut self, selector: &str, value: &str) -> Option<FocusHandle> {
        // The text is recorded even when the pool is exhausted, so a copy still takes the full
        // value of a row that lost its tab stop.
        self.texts.insert(selector.to_owned(), value.to_owned());
        let handle = self.pool.get(self.next)?.clone();
        self.next += 1;
        self.by_selector.insert(selector.to_owned(), handle.clone());
        Some(handle)
    }

    #[allow(dead_code)]
    fn get(&self, selector: &str) -> Option<FocusHandle> {
        self.by_selector.get(selector).cloned()
    }

    fn text(&self, selector: &str) -> Option<String> {
        self.texts.get(selector).cloned()
    }
}

/// What every value row needs while the Describe body renders.
#[derive(Clone)]
struct ValueRows {
    focus: Rc<RefCell<ValueFocus>>,
    expanded: Rc<BTreeSet<String>>,
    /// Rows whose copy feedback is still on screen.
    copied: Rc<BTreeSet<String>>,
    panel: WeakEntity<InspectorPanel>,
}

impl ValueRows {
    fn take_focus(&self, selector: &str, value: &str) -> Option<FocusHandle> {
        self.focus.borrow_mut().take(selector, value)
    }

    fn is_expanded(&self, selector: &str) -> bool {
        self.expanded.contains(selector)
    }

    /// Whether the copy feedback of a row is still showing, so the row can say so.
    fn copied(&self, selector: &str) -> bool {
        self.copied.contains(selector)
    }
}

/// One line of a local diff, shown in the review strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffLine<'a> {
    Context(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

impl DiffLine<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Context(text) | Self::Added(text) | Self::Removed(text) => text,
        }
    }

    fn marker(&self) -> &'static str {
        match self {
            Self::Context(_) => " ",
            Self::Added(_) => "+",
            Self::Removed(_) => "-",
        }
    }

    fn severity(&self) -> Severity {
        match self {
            Self::Context(_) => Severity::Muted,
            Self::Added(_) => Severity::Success,
            Self::Removed(_) => Severity::Error,
        }
    }
}

pub struct InspectorPanel {
    yaml_view: Entity<YamlView>,
    yaml_available: bool,
    /// Why the document could not be read, when it could not be read.
    ///
    /// This is the "读不到 ≠ 健康" channel the YAML tab was missing. The document arrives from
    /// whoever selected the row, and a failure used to arrive as the same `None` as "nothing is
    /// selected", so an RBAC denial, a broken watch, or a timeout rendered as `Select a row to
    /// inspect its YAML.` - a sentence that sends the reader to click the table again instead of
    /// naming the state they are in.
    yaml_error: Option<String>,
    /// Asks the owner to fetch the document again.
    ///
    /// The Inspector does not read YAML itself, so Retry has to leave the panel. Without a
    /// handler the control can only clear the error, which is why the shell installs one.
    yaml_reload: Option<YamlReload>,
    original: Option<String>,
    pending: Option<PendingLoad>,
    editable: bool,
    validation_error: Option<String>,
    applied_at: Option<Instant>,
    on_apply: Option<ApplyCallback>,
    apply_handler: Option<ApplyHandler>,
    apply_request: Option<ApplyRequest>,
    next_apply_id: u64,
    applying: bool,
    apply_error: Option<String>,
    conflict_owners: Option<Vec<String>>,
    /// A change that passed every guard and waits for a review before it can be written.
    pending_apply: Option<PendingApply>,
    /// The text the cluster last accepted. `original` stays the text the cluster reported, so
    /// Revert can always go back to it.
    applied_text: Option<String>,
    expanded_details: ExpandedDetails,
    /// One handle per collapsible section, so "Show all" is a tab stop and not a mouse action.
    section_toggle_focus: [FocusHandle; 3],
    active_tab: usize,
    focused_tab: usize,
    tab_focus: FocusHandle,
    tabs_scroll: ScrollHandle,
    action_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    apply_focus: FocusHandle,
    revert_focus: FocusHandle,
    copy_yaml_focus: FocusHandle,
    /// Focus handle for the YAML error state's Retry, which replaces the whole tab body and so
    /// cannot borrow the toolbar's handles.
    yaml_retry_focus: FocusHandle,
    review_keep_editing_focus: FocusHandle,
    review_check_focus: FocusHandle,
    review_apply_focus: FocusHandle,
    /// The server's verdict on the document under review, if it was asked.
    apply_check: ApplyCheckState,
    /// Sends the document to the server for validation without storing it.
    check_handler: Option<CheckHandler>,
    problems_focus: FocusHandle,
    /// Scroll position of the capped problems list.
    problems_scroll: ScrollHandle,
    /// One handle for the reload control of whichever context toolbar is on screen.
    reload_focus: FocusHandle,
    /// One handle for the retry control of the shared load-failure state.
    load_retry_focus: FocusHandle,
    problem_cursor: usize,
    /// How many problems the editor reported in the last render.
    problem_count: usize,
    /// Watches the editor, so a parse that lands after the typing pause still reaches the panel.
    yaml_observation: Option<Subscription>,
    /// The problems the panel last rendered, so only a change asks for a repaint.
    rendered_problems: Vec<Diagnostic>,

    source: Option<Arc<dyn InspectorSource>>,
    session: InspectorSession,
    selection: Option<ObjectRef>,
    selection_target: Option<ApplyTarget>,
    load_epoch: u64,
    describe_states: HashMap<String, LoadState<DescribeData>>,
    events_states: HashMap<String, EventsEntry>,
    describe_task: Option<Task<()>>,
    events_task: Option<Task<()>>,
    /// Watchdogs that turn a load that never answers into a retryable failure.
    describe_deadline: Option<Task<()>>,
    events_deadline: Option<Task<()>>,
    /// Bumped per request, so a late watchdog cannot expire a newer load.
    describe_token: u64,
    events_token: u64,
    describe_scroll: ScrollHandle,
    describe_focus: FocusHandle,
    describe_width: Rc<Cell<f32>>,
    events_scroll: UniformListScrollHandle,
    events_focus: FocusHandle,
    /// Focus handles and expansion state for the Describe value rows.
    value_focus: Rc<RefCell<ValueFocus>>,
    value_cursor: Option<String>,
    expanded_values: BTreeSet<String>,
    value_copied_at: Option<Instant>,

    // Metrics sampling
    metrics_source: Option<MetricsHandle>,
    metrics_probe: MetricsProbeState,
    metrics_probe_retry: Option<MetricsProbeRetry>,
    metrics_probe_task: Option<Task<()>>,
    /// Whether the Inspector is visible in the window.
    metrics_visible: bool,
    metrics_target: Option<MetricsTarget>,
    metrics: MetricsSamples,
    metrics_range_ms: i64,
    metrics_scheduler: SampleScheduler,
    metrics_task: Option<Task<()>>,
    metrics_epoch: u64,
    /// When the last sample was taken, so a restarted loop keeps the sample rate.
    metrics_last_sample: Option<Instant>,
    /// Focus handles for the three range buttons, one per duration.
    range_focus: [FocusHandle; 3],
    metrics_retry_focus: FocusHandle,
    cpu_chart: Entity<LineChartView>,
    memory_chart: Entity<LineChartView>,
    metrics_scroll: ScrollHandle,
    metrics_focus: FocusHandle,
    /// Why the Metrics tab is missing, shown instead of a silent fallback to YAML.
    metrics_notice: Option<String>,

    copied_at: Option<Instant>,
}

impl InspectorBindingInput for Option<Entity<InspectorPanel>> {
    fn into_binding(self) -> Option<InspectorBinding> {
        self.map(|inspector| {
            InspectorBinding::new(move |update, cx| {
                inspector.update(cx, |panel, cx| match update {
                    InspectorUpdate::Selection(selection) => panel.set_selection(selection, cx),
                    InspectorUpdate::Yaml(yaml) => panel.set_yaml(yaml, cx),
                });
            })
        })
    }
}

impl InspectorPanel {
    pub fn new(cx: &mut App) -> Self {
        let value_pool = (0..VALUE_FOCUS_POOL_SIZE)
            .map(|index| {
                cx.focus_handle()
                    .tab_stop(true)
                    .tab_index(VALUE_FOCUS_POOL_TAB_INDEX + index as isize)
            })
            .collect();
        Self {
            yaml_view: cx.new(YamlView::new),
            yaml_available: false,
            yaml_error: None,
            yaml_reload: None,
            yaml_retry_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_YAML_RETRY_TAB_INDEX),
            original: None,
            pending: None,
            editable: false,
            validation_error: None,
            applied_at: None,
            on_apply: None,
            apply_handler: None,
            apply_request: None,
            next_apply_id: 1,
            applying: false,
            apply_error: None,
            conflict_owners: None,
            pending_apply: None,
            applied_text: None,
            expanded_details: ExpandedDetails::default(),
            section_toggle_focus: std::array::from_fn(|index| {
                cx.focus_handle()
                    .tab_stop(true)
                    .tab_index(LABELS_EXPAND_TAB_INDEX + index as isize)
            }),
            active_tab: 0,
            focused_tab: 0,
            tab_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_TAB_STRIP_TAB_INDEX),
            tabs_scroll: ScrollHandle::new(),
            action_scroll: ScrollHandle::new(),
            focus_handle: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_CONTENT_TAB_INDEX),
            apply_focus: cx.focus_handle().tab_stop(true).tab_index(APPLY_TAB_INDEX),
            revert_focus: cx.focus_handle().tab_stop(true).tab_index(REVERT_TAB_INDEX),
            copy_yaml_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(COPY_YAML_TAB_INDEX),
            review_check_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(REVIEW_CHECK_TAB_INDEX),
            apply_check: ApplyCheckState::default(),
            check_handler: None,
            review_keep_editing_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(REVIEW_KEEP_EDITING_TAB_INDEX),
            review_apply_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(REVIEW_APPLY_TAB_INDEX),
            problems_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_PROBLEMS_TAB_INDEX),
            problems_scroll: ScrollHandle::new(),
            reload_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_RELOAD_TAB_INDEX),
            load_retry_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_DESCRIBE_RETRY_TAB_INDEX),
            problem_cursor: 0,
            problem_count: 0,
            yaml_observation: None,
            rendered_problems: Vec::new(),
            source: None,
            session: InspectorSession::default(),
            selection: None,
            selection_target: None,
            load_epoch: 0,
            describe_states: HashMap::new(),
            events_states: HashMap::new(),
            describe_task: None,
            events_task: None,
            describe_deadline: None,
            events_deadline: None,
            describe_token: 0,
            events_token: 0,
            describe_scroll: ScrollHandle::new(),
            describe_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX),
            describe_width: Rc::new(Cell::new(0.0)),
            events_scroll: UniformListScrollHandle::new(),
            events_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_EVENTS_SCROLL_TAB_INDEX),
            value_focus: Rc::new(RefCell::new(ValueFocus::new(value_pool))),
            value_cursor: None,
            expanded_values: BTreeSet::new(),
            value_copied_at: None,
            metrics_source: None,
            metrics_probe: MetricsProbeState::default(),
            metrics_probe_retry: None,
            metrics_probe_task: None,
            metrics_visible: false,
            metrics_target: None,
            metrics: MetricsSamples::default(),
            metrics_range_ms: DEFAULT_RANGE_MS,
            metrics_scheduler: SampleScheduler::default(),
            metrics_task: None,
            metrics_epoch: 0,
            metrics_last_sample: None,
            range_focus: std::array::from_fn(|index| {
                cx.focus_handle()
                    .tab_stop(true)
                    .tab_index(METRICS_RANGE_TAB_INDEX + index as isize)
            }),
            metrics_retry_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(METRICS_RETRY_TAB_INDEX),
            cpu_chart: cx.new(|_| LineChartView::new()),
            memory_chart: cx.new(|_| LineChartView::new()),
            metrics_scroll: ScrollHandle::new(),
            metrics_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(INSPECTOR_METRICS_SCROLL_TAB_INDEX),
            metrics_notice: None,
            copied_at: None,
        }
    }

    fn tab_count(&self) -> usize {
        TABS.len() + usize::from(self.metrics_tab_visible())
    }

    fn activate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tab_count();
        if count == 0 || index >= count {
            return;
        }
        self.focused_tab = index;
        self.tabs_scroll.scroll_to_item(tab_scroll_index(index));
        if index != InspectorTab::Metrics as usize || self.metrics_tab_visible() {
            self.active_tab = index;
        }
        self.ensure_tab_data(cx);
        self.update_metrics_sampling(cx);
        window.focus(&self.tab_focus, cx);
        cx.notify();
    }

    /// Switches tabs and loads data for the selected tab.
    pub fn show_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        let requested = tab as usize;
        if requested == InspectorTab::Metrics as usize && !self.metrics_tab_visible() {
            // The tab cannot exist for this object, so the request falls back to YAML and says
            // why instead of appearing to do nothing.
            let reason = self.metrics_unavailable_reason();
            self.metrics_notice = Some(reason);
            self.active_tab = 0;
            self.focused_tab = 0;
            self.tabs_scroll.scroll_to_item(tab_scroll_index(0));
            cx.notify();
            return;
        }
        self.metrics_notice = None;
        let index = requested;
        self.active_tab = index.min(self.tab_count().saturating_sub(1));
        self.focused_tab = self.active_tab;
        self.tabs_scroll
            .scroll_to_item(tab_scroll_index(self.active_tab));
        self.ensure_tab_data(cx);
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    /// Why the Metrics tab is missing, so the fallback to YAML is never silent.
    fn metrics_unavailable_reason(&self) -> String {
        if self.metrics_source.is_none() {
            return "No cluster connection owns the metrics API, so this Inspector has no Metrics tab."
                .to_owned();
        }
        let Some(selection) = self.selection.as_ref() else {
            return "Select a Node or a Pod to see CPU and memory usage.".to_owned();
        };
        if self.metrics_target.is_none() {
            let kind = if selection.resource.kind.is_empty() {
                "This resource"
            } else {
                selection.resource.kind.as_str()
            };
            return format!("{kind} does not publish metrics. Only Nodes and Pods do.");
        }
        match &self.metrics_probe {
            MetricsProbeState::Missing => {
                "metrics-server is not installed on this cluster, so the Metrics tab is hidden."
                    .to_owned()
            }
            MetricsProbeState::Forbidden { reason } => format!(
                "The cluster denied the metrics request, so the Metrics tab is hidden: {reason}"
            ),
            MetricsProbeState::Error { reason } => {
                format!("The metrics check failed, so the Metrics tab is hidden: {reason}")
            }
            MetricsProbeState::Checking => {
                "The metrics check is still running, so the Metrics tab is hidden for now."
                    .to_owned()
            }
            MetricsProbeState::Available => {
                "The Metrics tab is hidden until the Inspector is visible.".to_owned()
            }
        }
    }

    pub fn set_source(&mut self, source: Arc<dyn InspectorSource>) {
        let changed = self
            .source
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &source));
        if changed {
            self.session.id = self.session.id.wrapping_add(1);
            self.invalidate_session_state();
        }
        self.source = Some(source);
    }

    pub fn set_session_identity(&mut self, session: InspectorSession) {
        if self.session != session {
            self.session = session;
            self.invalidate_session_state();
        }
    }

    pub fn set_cluster_id(&mut self, cluster_id: Option<k8s_core::cluster::ClusterId>) {
        self.set_session_identity(InspectorSession {
            id: self.session.id,
            cluster_id,
        });
    }

    pub fn set_session_epoch(&mut self, session_epoch: u64) {
        self.set_session_identity(InspectorSession {
            id: session_epoch,
            cluster_id: self.session.cluster_id,
        });
    }

    pub fn session_identity(&self) -> InspectorSession {
        self.session
    }

    // Metrics

    /// Replaces the Metrics source when the session changes.
    pub fn set_metrics_source(
        &mut self,
        source: Option<MetricsHandle>,
        state: MetricsProbeState,
        cx: &mut Context<Self>,
    ) {
        let tier = source
            .as_ref()
            .map_or(k8s_core::latency::LatencyTier::Local, MetricsHandle::tier);
        self.metrics_source = source;
        self.metrics = MetricsSamples::default();
        self.metrics_scheduler = SampleScheduler::new(tier);
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.metrics_probe_task = None;
        self.metrics_last_sample = None;
        self.set_metrics_probe_state(state, cx);
        self.update_chart_data(cx);
    }

    pub fn set_metrics_probe_retry_handler(&mut self, handler: impl Fn(&mut App) + 'static) {
        self.metrics_probe_retry = Some(Rc::new(handler));
    }

    pub fn set_metrics_probe_state(&mut self, state: MetricsProbeState, cx: &mut Context<Self>) {
        self.metrics_probe = state;
        self.metrics.last_error = match &self.metrics_probe {
            MetricsProbeState::Available | MetricsProbeState::Checking => None,
            MetricsProbeState::Missing => Some(METRICS_UNAVAILABLE.to_owned()),
            MetricsProbeState::Forbidden { reason } | MetricsProbeState::Error { reason } => {
                Some(reason.clone())
            }
        };
        if !self.metrics_tab_visible() && self.active_tab == InspectorTab::Metrics as usize {
            let reason = self.metrics_unavailable_reason();
            self.active_tab = 0;
            self.focused_tab = 0;
            self.metrics_notice = Some(reason);
        }
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    fn set_metrics_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if !available && matches!(self.metrics_probe, MetricsProbeState::Error { .. }) {
            self.update_metrics_sampling(cx);
            return;
        }
        let state = if available {
            MetricsProbeState::Available
        } else {
            MetricsProbeState::Missing
        };
        self.set_metrics_probe_state(state, cx);
    }

    fn retry_metrics_sample(&mut self, cx: &mut Context<Self>) {
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.reset_metrics_scheduler();
        self.metrics_last_sample = None;
        self.metrics.last_error = None;
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    fn retry_metrics(&mut self, cx: &mut Context<Self>) {
        if self.metrics_probe.is_available() {
            self.retry_metrics_sample(cx);
            return;
        }
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
        self.metrics_task = None;
        self.reset_metrics_scheduler();
        self.metrics_probe = MetricsProbeState::Checking;
        self.metrics.last_error = None;
        match self.metrics_probe_retry.clone() {
            Some(retry) => retry(cx),
            // No owner for the probe, so run it here. Otherwise the panel would wait in
            // "Checking metrics availability" with no way forward.
            None => self.probe_metrics(cx),
        }
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    /// Runs a one-shot metrics probe for a panel without a probe owner.
    fn probe_metrics(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.metrics_source.clone() else {
            self.set_metrics_probe_state(MetricsProbeState::Missing, cx);
            return;
        };
        let epoch = self.metrics_epoch;
        self.metrics_probe_task = Some(cx.spawn(async move |this, cx| {
            let result = source.probe_future().await;
            this.update(cx, |panel, cx| {
                if panel.metrics_epoch != epoch {
                    return;
                }
                panel.set_metrics_probe_state(MetricsProbeState::from_result(result), cx);
            })
            .ok();
        }));
    }

    /// Reloads the data behind the active tab.
    fn reload_active_tab(&mut self, cx: &mut Context<Self>) {
        match self.active_tab {
            0 => self.reload_yaml(cx),
            1 => self.ensure_describe(true, cx),
            2 => self.ensure_events(true, cx),
            3 => self.retry_metrics(cx),
            _ => {}
        }
    }

    fn reload_action(&mut self, _: &ReloadActiveTab, _window: &mut Window, cx: &mut Context<Self>) {
        self.reload_active_tab(cx);
    }

    fn metrics_retry_action(
        &mut self,
        _: &RetryMetrics,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.retry_metrics(cx);
    }

    fn set_metrics_range(&mut self, range_ms: i64, cx: &mut Context<Self>) {
        if self.metrics_range_ms == range_ms {
            return;
        }
        self.metrics_range_ms = range_ms;
        self.update_chart_data(cx);
        cx.notify();
    }

    fn range_5m(&mut self, _: &MetricsRange5m, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(5 * 60 * 1000, cx);
    }

    fn range_15m(&mut self, _: &MetricsRange15m, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(15 * 60 * 1000, cx);
    }

    fn range_1h(&mut self, _: &MetricsRange1h, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_metrics_range(60 * 60 * 1000, cx);
    }

    /// Updates sampling when Inspector visibility changes.
    pub fn set_metrics_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.metrics_visible == visible {
            return;
        }
        self.metrics_visible = visible;
        self.update_metrics_sampling(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn metrics_visible(&self) -> bool {
        self.metrics_visible
    }

    #[cfg(test)]
    pub(crate) fn metrics_sampling(&self) -> bool {
        self.metrics_task.is_some()
    }

    fn metrics_tab_visible(&self) -> bool {
        self.metrics_source.is_some() && self.metrics_target.is_some()
    }

    fn metrics_should_sample(&self) -> bool {
        self.metrics_tab_visible()
            && self.metrics_probe.is_available()
            && self.metrics_visible
            && self.active_tab == InspectorTab::Metrics as usize
    }

    fn reset_metrics_scheduler(&mut self) {
        let tier = self
            .metrics_source
            .as_ref()
            .map_or(k8s_core::latency::LatencyTier::Local, MetricsHandle::tier);
        self.metrics_scheduler = SampleScheduler::new(tier);
    }

    fn sync_metrics_scheduler_tier(
        &mut self,
        tier: k8s_core::latency::LatencyTier,
        cx: &mut Context<Self>,
    ) {
        if self.metrics_scheduler.interval() != SampleScheduler::interval_for(tier) {
            self.metrics_scheduler.set_tier(tier);
            self.update_chart_data(cx);
            cx.notify();
        }
    }

    fn update_metrics_sampling(&mut self, cx: &mut Context<Self>) {
        let should_sample = self.metrics_should_sample();
        self.metrics_scheduler.set_visible(should_sample);
        if !should_sample {
            self.metrics_task = None;
            return;
        }
        // Becoming visible forces one immediate sample. Drop that request when the last
        // sample is still fresh, so flipping tabs cannot flood the API server.
        if self
            .metrics_last_sample
            .is_some_and(|last| last.elapsed() < self.metrics_scheduler.interval())
        {
            let _ = self.metrics_scheduler.decide(Duration::ZERO);
        }
        if self.metrics_task.is_some() {
            return;
        }
        let (Some(source), Some(target)) =
            (self.metrics_source.clone(), self.metrics_target.clone())
        else {
            return;
        };
        let epoch = self.metrics_epoch;
        let initial_tier = source.tier();
        self.sync_metrics_scheduler_tier(initial_tier, cx);
        let mut latency = source.latency_receiver();
        self.metrics_task = Some(cx.spawn(async move |this, cx| {
            let mut last = this
                .update(cx, |panel, _| panel.metrics_last_sample)
                .ok()
                .flatten()
                .unwrap_or_else(Instant::now);
            loop {
                let current_tier = match latency.as_mut() {
                    Some(receiver) => receiver.borrow_and_update().tier(),
                    None => initial_tier,
                };
                let decision = this
                    .update(cx, |panel, cx| {
                        if panel.metrics_epoch != epoch || !panel.metrics_should_sample() {
                            return SampleDecision::Stopped;
                        }
                        panel.sync_metrics_scheduler_tier(current_tier, cx);
                        panel.metrics_scheduler.decide(last.elapsed())
                    })
                    .unwrap_or(SampleDecision::Stopped);
                match decision {
                    SampleDecision::Stopped => break,
                    SampleDecision::Wait(delay) => {
                        if let Some(receiver) = latency.as_mut() {
                            tokio::select! {
                                _ = cx.background_executor().timer(delay) => {}
                                result = receiver.changed() => {
                                    if result.is_err() {
                                        break;
                                    }
                                }
                            }
                        } else {
                            cx.background_executor().timer(delay).await;
                        }
                    }
                    SampleDecision::SampleNow => {
                        last = Instant::now();
                        let future = match &target {
                            MetricsTarget::Node { name, .. } => source.node_future(name),
                            MetricsTarget::Pod {
                                namespace, name, ..
                            } => source.pod_future(namespace, name),
                        };
                        let result = future.await;
                        let unavailable = result
                            .as_ref()
                            .err()
                            .is_some_and(|reason| reason == METRICS_UNAVAILABLE);
                        let updated = this
                            .update(cx, |panel, cx| {
                                if panel.metrics_epoch != epoch {
                                    return false;
                                }
                                panel.metrics_last_sample = Some(Instant::now());
                                panel.record_metrics_sample(result, cx);
                                if unavailable {
                                    panel.set_metrics_available(false, cx);
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !updated {
                            break;
                        }
                    }
                }
            }
        }));
    }

    fn record_metrics_sample(
        &mut self,
        result: Result<SamplePayload, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(payload) => {
                self.metrics.record(payload);
                self.metrics_scheduler.on_success();
            }
            Err(reason) => {
                self.metrics.last_error = Some(reason);
                self.metrics_scheduler.on_failure();
            }
        }
        self.update_chart_data(cx);
        cx.notify();
    }

    fn update_chart_data(&mut self, cx: &mut Context<Self>) {
        let interval_ms = MetricsSamples::interval_ms(&self.metrics_scheduler);
        let cpu = self
            .metrics
            .cpu_chart_data(self.metrics_range_ms, interval_ms);
        let memory = self
            .metrics
            .memory_chart_data(self.metrics_range_ms, interval_ms);
        let title = self.metrics_target.as_ref().map_or_else(
            || "Metrics".to_owned(),
            |target| target.kind_label().to_owned(),
        );
        self.cpu_chart.update(cx, |chart, cx| {
            chart.set_title(format!("{title} CPU"), cx);
            chart.set_data(Rc::new(cpu), cx);
        });
        self.memory_chart.update(cx, |chart, cx| {
            chart.set_title(format!("{title} memory"), cx);
            chart.set_data(Rc::new(memory), cx);
        });
    }

    pub fn set_selection(&mut self, selection: Option<InspectorSelection>, cx: &mut Context<Self>) {
        if self.yaml_view.read(cx).is_dirty() {
            self.pending = Some(PendingLoad::Selection(selection));
            self.validation_error = None;
            cx.notify();
            return;
        }
        self.load_selection(selection, cx);
    }

    pub fn set_yaml(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        if self.yaml_view.read(cx).is_dirty() {
            self.pending = Some(PendingLoad::Yaml(yaml));
            self.validation_error = None;
            cx.notify();
            return;
        }
        self.invalidate_apply();
        self.clear_selection_state();
        self.load_yaml(yaml, cx);
        self.sync_metrics_target(cx);
    }

    /// Records that the document could not be read, and why.
    ///
    /// This is the state `InspectorUpdate::Yaml(None)` used to swallow. A failed fetch and an
    /// empty selection are different facts: one is an anomaly the reader has to know about, the
    /// other is an instruction. Rendering them the same way is what made an RBAC denial during an
    /// incident read as "you have not clicked anything yet".
    ///
    /// `object` is the object whose document failed, when the caller knows it. The selection moves
    /// to it, because the object *is* still selected: Describe, Events, and Metrics can read it,
    /// and the identity bar has to keep naming it. Only the document is missing, so the editor is
    /// emptied rather than left holding the previous object's text next to the new object's name.
    pub fn set_yaml_error(
        &mut self,
        object: Option<ObjectRef>,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        if self.yaml_view.read(cx).is_dirty() {
            // The buffer holds edits that were never written, so a late failure must not replace
            // them. The next Apply or Revert clears the path back to a load.
            return;
        }
        let target = object
            .as_ref()
            .map(|object| ApplyTarget::from_object(object.clone(), Some(self.session)));
        let changed = self.selection != object || self.selection_target != target;
        if self.yaml_error.as_deref() == Some(reason.as_str()) && !changed {
            return;
        }
        if changed {
            self.load_yaml(None, cx);
            self.selection = object;
            self.selection_target = target;
            self.reset_selection_loads(cx);
            self.sync_metrics_target(cx);
        }
        self.invalidate_apply();
        // After the load, because `load_yaml` resolves every previous failure.
        self.yaml_error = Some(reason);
        cx.notify();
    }

    /// Installs the channel Retry uses to fetch the document again.
    ///
    /// The Inspector never reads YAML itself; the table hands it the text it already read, so the
    /// request has to go back out. The shell calls this with whatever re-reads the selected row.
    pub fn set_yaml_reload(&mut self, reload: impl Fn(&mut App) + 'static) {
        self.yaml_reload = Some(Rc::new(reload));
    }

    /// The reason the document could not be read, for tests and for the shell's own state.
    pub fn yaml_error(&self) -> Option<&str> {
        self.yaml_error.as_deref()
    }

    /// Asks for the document again and leaves the error state.
    ///
    /// The error clears either way: a Retry that leaves the reader in the same state is a control
    /// that does nothing. With no channel installed the panel simply returns to the empty
    /// selection, which is why the shell installs one.
    fn reload_yaml(&mut self, cx: &mut Context<Self>) {
        self.yaml_error = None;
        if let Some(reload) = self.yaml_reload.clone() {
            reload(cx);
        }
        cx.notify();
    }

    pub(crate) fn has_apply_handler(&self) -> bool {
        self.apply_handler.is_some() || self.on_apply.is_some()
    }

    fn apply_error_is_unknown(reason: &str) -> bool {
        let reason = reason.to_ascii_lowercase();
        reason.contains("timeout")
            || reason.contains("timed out")
            || reason.contains("gateway")
            || reason.contains("service unavailable")
            || reason.contains("hypererror")
            || reason.contains("serviceerror")
            || reason.contains("connection reset")
            || reason.contains("connection refused")
            || reason.contains("connection closed")
            || reason.contains("connection lost")
            || reason.contains("connection interrupted")
            || reason.contains("connection error")
            || reason.contains("network is unreachable")
            || reason.contains("broken pipe")
            || reason.contains("unexpected eof")
    }

    fn clear_unavailable_error(&mut self) {
        if self.apply_error.as_deref() == Some(APPLY_UNAVAILABLE_REASON) {
            self.apply_error = None;
        }
    }

    pub fn set_on_apply(&mut self, callback: impl Fn(ApplyRequest) + 'static) {
        self.on_apply = Some(Box::new(callback));
        self.clear_unavailable_error();
    }

    pub fn set_on_apply_request(&mut self, callback: impl Fn(ApplyRequest) + 'static) {
        self.set_on_apply(callback);
    }

    pub fn set_apply_handler(&mut self, handler: impl Fn(ApplyRequest, &mut App) + 'static) {
        self.apply_handler = Some(Box::new(handler));
        self.clear_unavailable_error();
    }

    pub fn set_targeted_apply_handler(
        &mut self,
        handler: impl Fn(ApplyRequest, &mut App) + 'static,
    ) {
        self.set_apply_handler(handler);
    }

    pub fn set_editable(&mut self, editable: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editable = editable && self.yaml_available && !self.applying;
        if self.editable != editable {
            self.editable = editable;
            self.validation_error = None;
            self.apply_error = None;
            self.conflict_owners = None;
            self.action_scroll.scroll_to_item(0);
        }
        let weak = cx.weak_entity();
        let edit_weak = weak.clone();
        self.yaml_view.update(cx, |view, cx| {
            view.set_editable(editable, cx);
            view.set_on_apply_requested(move |text, _window, cx| {
                if let Some(panel) = weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.apply_text(text, cx));
                    });
                }
            });
            view.set_on_edit(move |cx| {
                if let Some(panel) = edit_weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.clear_edit_feedback(cx));
                    });
                }
            });
            if editable {
                view.focus(window, cx);
            }
        });
        cx.notify();
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.yaml_view.read(cx).is_dirty()
    }

    pub fn is_applying(&self) -> bool {
        self.applying
    }

    pub fn has_yaml(&self) -> bool {
        self.yaml_available
    }

    /// Current tab for commands and tests.
    pub fn active_tab(&self) -> InspectorTab {
        match self.active_tab {
            1 => InspectorTab::Describe,
            2 => InspectorTab::Events,
            3 => InspectorTab::Metrics,
            _ => InspectorTab::Yaml,
        }
    }

    /// YAML edit state for commands and tests.
    pub fn is_editing(&self, cx: &App) -> bool {
        self.yaml_view.read(cx).is_editable()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn yaml_focus_handle(&self, cx: &App) -> FocusHandle {
        self.yaml_view.read(cx).focus_handle().clone()
    }

    pub fn current_selection(&self, cx: &App) -> Option<InspectorSelection> {
        Some(InspectorSelection {
            object: self.selection.clone()?,
            yaml: self.yaml_view.read(cx).text()?,
        })
    }

    pub fn selection(&self) -> Option<&ObjectRef> {
        self.selection.as_ref()
    }

    pub fn apply_target(&self) -> Option<ApplyTarget> {
        self.current_apply_target().cloned()
    }

    pub fn has_exact_target(&self) -> bool {
        self.current_apply_target()
            .is_some_and(ApplyTarget::is_complete)
    }

    pub fn current_apply_request(&self) -> Option<ApplyRequest> {
        self.apply_request.clone()
    }

    pub fn pending_selection(&self) -> Option<InspectorSelection> {
        match &self.pending {
            Some(PendingLoad::Selection(selection)) => selection.clone(),
            _ => None,
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn has_pending_selection(&self) -> bool {
        matches!(&self.pending, Some(PendingLoad::Selection(_)))
    }

    /// Explains why Apply cannot run while a selection is deferred.
    ///
    /// The visible YAML still belongs to the previous object, so an Apply would write to
    /// an object the user already navigated away from.
    fn apply_blocked_by_pending(&self) -> Option<(&'static str, String)> {
        self.pending_selection()?;
        let current = self
            .selection
            .as_ref()
            .map_or_else(|| "the previous object".to_owned(), object_display_identity);
        Some((
            "Apply paused",
            format!(
                "The YAML still shows {current}. Cancel the changes to load the selected object."
            ),
        ))
    }

    fn current_apply_target(&self) -> Option<&ApplyTarget> {
        self.selection_target
            .as_ref()
            .filter(|target| target.session == Some(self.session))
    }

    fn clear_selection_state(&mut self) {
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.abandon_stale_loads();
        self.describe_task = None;
        self.events_task = None;
        self.describe_deadline = None;
        self.events_deadline = None;
        self.selection = None;
        self.selection_target = None;
    }

    /// Drops cached `Loading` entries whose request can no longer complete.
    ///
    /// A load started for another selection never returns, so leaving the entry behind
    /// would block every later visit to that object.
    fn abandon_stale_loads(&mut self) {
        self.describe_states
            .retain(|_, state| !matches!(state, LoadState::Loading));
        self.events_states
            .retain(|_, entry| !matches!(entry.state, LoadState::Loading));
    }

    fn invalidate_apply(&mut self) {
        self.applying = false;
        self.apply_request = None;
        self.pending_apply = None;
    }

    fn clear_edit_feedback(&mut self, cx: &mut Context<Self>) {
        self.validation_error = None;
        self.applied_at = None;
        self.apply_error = None;
        self.conflict_owners = None;
        // Undoing back to the saved text leaves nothing to apply, so the deferred
        // selection can load right away.
        if self.has_pending() && !self.yaml_view.read(cx).is_dirty() && self.load_pending(cx) {
            return;
        }
        cx.notify();
    }

    fn invalidate_session_state(&mut self) {
        self.invalidate_apply();
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.describe_task = None;
        self.events_task = None;
        self.describe_deadline = None;
        self.events_deadline = None;
        self.describe_states.clear();
        self.events_states.clear();
        self.pending = None;
        self.selection = None;
        self.selection_target = None;
        self.metrics_target = None;
        self.metrics = MetricsSamples::default();
        self.reset_metrics_scheduler();
        self.metrics_task = None;
        self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
    }

    /// Drops the loads and the disclosure state that belonged to the previous object.
    ///
    /// Shared by the two ways the selection moves: a document arriving, and a document failing to
    /// arrive. A load started for another object never returns, so leaving it behind would block
    /// every later visit to that object.
    fn reset_selection_loads(&mut self, cx: &mut Context<Self>) {
        self.load_epoch = self.load_epoch.wrapping_add(1);
        self.abandon_stale_loads();
        self.describe_task = None;
        self.events_task = None;
        self.expanded_details = ExpandedDetails::default();
        self.events_scroll
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.ensure_tab_data(cx);
    }

    fn load_selection(&mut self, selection: Option<InspectorSelection>, cx: &mut Context<Self>) {
        let (yaml, object) = match selection {
            Some(selection) => (Some(selection.yaml), Some(selection.object)),
            None => (None, None),
        };
        let target = object
            .as_ref()
            .map(|object| ApplyTarget::from_object(object.clone(), Some(self.session)));
        let changed = self.selection != object || self.selection_target != target;
        if changed {
            self.load_yaml(yaml, cx);
        } else if self.yaml_view.read(cx).text().as_deref() != yaml.as_deref() {
            // Same object, new content: keep the caret, selection, scroll, and IME state
            // so a live update does not interrupt typing or a composition.
            self.load_yaml_content(yaml, cx);
        }
        self.selection = object;
        self.selection_target = target;
        if changed {
            self.reset_selection_loads(cx);
        }
        self.sync_metrics_target(cx);
        cx.notify();
    }

    /// Updates the Metrics target when the selection changes.
    fn sync_metrics_target(&mut self, cx: &mut Context<Self>) {
        let next = self.selection.as_ref().and_then(|object| {
            MetricsTarget::from_object(
                object.resource.kind.as_str(),
                object.namespace.as_deref(),
                &object.name,
                &object.uid,
            )
        });
        if self.metrics_target != next {
            self.metrics_target = next;
            self.metrics = MetricsSamples::default();
            self.reset_metrics_scheduler();
            self.metrics_epoch = self.metrics_epoch.wrapping_add(1);
            self.metrics_task = None;
            self.metrics_last_sample = None;
            self.update_chart_data(cx);
        }
        if !self.metrics_tab_visible() && self.active_tab == InspectorTab::Metrics as usize {
            let reason = self.metrics_unavailable_reason();
            self.active_tab = 0;
            self.focused_tab = 0;
            self.metrics_notice = Some(reason);
        }
        self.update_metrics_sampling(cx);
    }

    fn load_yaml(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        self.load_yaml_content(yaml, cx);
        self.yaml_view
            .update(cx, |view, cx| view.reset_view_state(cx));
    }

    /// Replaces the document text and the panel state around it.
    fn load_yaml_content(&mut self, yaml: Option<String>, cx: &mut Context<Self>) {
        self.invalidate_apply();
        self.yaml_available = yaml.is_some();
        // A document that arrived resolves the last failure: the two states are exclusive, and
        // keeping a stale reason would put an anomaly banner over a document that is on screen.
        self.yaml_error = None;
        self.original = yaml.clone();
        self.editable = self.yaml_available;
        self.action_scroll.scroll_to_item(0);
        self.validation_error = None;
        self.applied_at = None;
        self.applied_text = None;
        self.apply_error = None;
        self.conflict_owners = None;
        self.expanded_values.clear();
        self.value_cursor = None;
        let editable = self.yaml_available;
        let weak = cx.weak_entity();
        let edit_weak = weak.clone();
        self.yaml_view.update(cx, |view, cx| {
            view.set_text(yaml, cx);
            view.set_editable(editable, cx);
            view.set_on_apply_requested(move |text, _window, cx| {
                if let Some(panel) = weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.apply_text(text, cx));
                    });
                }
            });
            view.set_on_edit(move |cx| {
                if let Some(panel) = edit_weak.upgrade() {
                    cx.defer(move |cx| {
                        panel.update(cx, |panel, cx| panel.clear_edit_feedback(cx));
                    });
                }
            });
        });
        cx.notify();
    }

    fn load_pending(&mut self, cx: &mut Context<Self>) -> bool {
        match self.pending.take() {
            Some(PendingLoad::Yaml(yaml)) => {
                self.clear_selection_state();
                self.load_yaml(yaml, cx);
                self.sync_metrics_target(cx);
                true
            }
            Some(PendingLoad::Selection(selection)) => {
                self.load_selection(selection, cx);
                true
            }
            None => false,
        }
    }

    pub fn discard_changes(&mut self, cx: &mut Context<Self>) {
        self.revert(cx);
    }

    pub fn discard_dirty(&mut self, cx: &mut Context<Self>) {
        self.revert(cx);
    }

    pub fn discard(&mut self, cx: &mut Context<Self>) {
        self.revert(cx);
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.invalidate_apply();
        self.clear_selection_state();
        self.load_yaml(None, cx);
        self.sync_metrics_target(cx);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
    }

    // YAML apply

    pub fn apply(&mut self, cx: &mut Context<Self>) {
        if self.applying || !self.yaml_view.read(cx).is_dirty() {
            return;
        }
        let Some(text) = self.yaml_view.read(cx).text() else {
            return;
        };
        self.apply_text(text, cx);
    }

    /// Requests a review of the current text. Nothing is written from this call: the request it
    /// captures waits for [`Self::confirm_pending_apply`].
    fn apply_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.applying || !self.yaml_view.read(cx).is_dirty() {
            return;
        }
        if self.apply_blocked_by_pending().is_some() {
            // The status strip already explains the paused target.
            cx.notify();
            return;
        }
        match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) {
            Ok(_) => {
                self.validation_error = None;
                self.apply_error = None;
                self.conflict_owners = None;
                self.yaml_view
                    .update(cx, |view, cx| view.clear_diagnostics(cx));
                if !self.has_apply_handler() {
                    self.apply_error = Some(APPLY_UNAVAILABLE_REASON.to_owned());
                    cx.notify();
                    return;
                }
                let Some(target) = self.current_apply_target().cloned() else {
                    self.apply_error = Some("Select a row to apply YAML.".to_owned());
                    cx.notify();
                    return;
                };
                if !target.is_complete() {
                    self.apply_error = Some(
                        "The selected object is incomplete. Select a complete object, then apply YAML."
                            .to_owned(),
                    );
                    cx.notify();
                    return;
                }
                if !yaml_matches_target(&target, &text) {
                    self.apply_error = Some(
                        "The YAML name, namespace, UID, or resource type does not match the selected object. Update the YAML or select the matching object."
                            .to_owned(),
                    );
                    cx.notify();
                    return;
                }
                let request_id = self.next_apply_id;
                self.next_apply_id = self.next_apply_id.wrapping_add(1);
                self.pending_apply = Some(PendingApply {
                    request_id,
                    target,
                    yaml: text,
                });
                self.problem_cursor = 0;
                cx.notify();
            }
            Err(error) => {
                let diagnostic = Diagnostic::from_yaml_error(&error);
                self.validation_error = Some(diagnostic.message.clone());
                self.applied_at = None;
                self.yaml_view
                    .update(cx, |view, cx| view.set_diagnostics(vec![diagnostic], cx));
                cx.notify();
            }
        }
    }

    /// A reviewed change waiting for the user.
    #[allow(dead_code)]
    fn reviewed_change(&self) -> Option<&PendingApply> {
        self.pending_apply.as_ref()
    }

    /// The text the cluster last accepted, which stays readable after the editor is marked saved.
    pub fn applied_text(&self) -> Option<&str> {
        self.applied_text.as_deref()
    }

    /// Writes the reviewed change. Every guard runs again here, because the text, the target, or
    /// the session can all have moved while the review was open.
    pub fn confirm_pending_apply(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_apply.take() else {
            return;
        };
        if self.applying {
            return;
        }
        if self.yaml_view.read(cx).text().as_deref() != Some(pending.yaml.as_str()) {
            self.apply_error = Some(APPLY_STALE_REVIEW_REASON.to_owned());
            cx.notify();
            return;
        }
        let Some(target) = self.current_apply_target().cloned() else {
            self.apply_error = Some("Select a row to apply YAML.".to_owned());
            cx.notify();
            return;
        };
        if target != pending.target {
            self.apply_error = Some(
                "The selected object changed. Select the object again, then apply the YAML."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        if !yaml_matches_target(&target, &pending.yaml) {
            self.apply_error = Some(
                "The YAML name, namespace, UID, or resource type does not match the selected object. Update the YAML or select the matching object."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let request = ApplyRequest::new(pending.request_id, target, pending.yaml);
        self.apply_request = Some(request.clone());
        self.applying = true;
        self.editable = false;
        self.yaml_view
            .update(cx, |view, cx| view.set_editable(false, cx));
        if let Some(handler) = &self.apply_handler {
            handler(request, cx);
            cx.notify();
            return;
        }
        if let Some(on_apply) = &self.on_apply {
            on_apply(request.clone());
            self.mark_applied(request, cx);
        }
    }

    /// Drops a review without writing anything.
    pub fn cancel_pending_apply(&mut self, cx: &mut Context<Self>) {
        if self.pending_apply.take().is_none() {
            return;
        }
        // A verdict belongs to the document it checked, so it does not outlive the review.
        self.apply_check = ApplyCheckState::default();
        cx.notify();
    }

    /// Asks the API server to validate the document under review without storing it.
    ///
    /// The check never blocks the apply. It answers a question the local diff cannot: would the
    /// server accept this document at all. A person about to write to a cluster should be able
    /// to ask that first, and a person who would rather not wait can still apply straight away.
    ///
    /// The shell owns the request, exactly as it owns the apply, and reports the outcome back
    /// through [`Self::apply_check_finished`].
    pub fn check_pending_apply(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_apply.clone() else {
            return;
        };
        let Some(handler) = self.check_handler.clone() else {
            // No session to ask. Saying so beats a button that silently does nothing.
            self.apply_check = ApplyCheckState::Failed {
                reason: "Not connected to a context. Select a context, then check the change."
                    .to_owned(),
            };
            cx.notify();
            return;
        };
        let request = ApplyRequest::new(pending.request_id, pending.target, pending.yaml);
        self.apply_check = ApplyCheckState::Running;
        cx.notify();
        handler(request, cx);
    }

    /// Installs the transport that sends a document to the server for validation.
    pub fn set_targeted_check_handler(
        &mut self,
        handler: impl Fn(ApplyRequest, &mut App) + 'static,
    ) {
        self.check_handler = Some(std::rc::Rc::new(handler));
    }

    /// Records the server's verdict.
    pub fn apply_check_finished(
        &mut self,
        result: Result<ApplyVerdict, String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_check = match result {
            Ok(ApplyVerdict::Valid) => ApplyCheckState::Valid,
            Ok(ApplyVerdict::Conflict { owners }) => ApplyCheckState::Conflict { owners },
            Err(reason) => ApplyCheckState::Failed { reason },
        };
        cx.notify();
    }

    fn confirm_action(&mut self, _: &ConfirmApply, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_pending_apply(cx);
    }

    fn cancel_review_action(
        &mut self,
        _: &CancelApplyReview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_apply(cx);
    }

    fn revert_action(&mut self, _: &RevertYaml, _window: &mut Window, cx: &mut Context<Self>) {
        self.revert(cx);
    }

    fn copy_yaml_action(&mut self, _: &CopyYaml, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy_yaml(cx);
    }

    fn toggle_value_action(
        &mut self,
        _: &ToggleValueExpansion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selector) = self.value_cursor.clone() else {
            return;
        };
        self.toggle_value_expansion(&selector, cx);
    }

    fn copy_value_action(&mut self, _: &CopyValue, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(selector) = self.value_cursor.clone() else {
            return;
        };
        self.copy_value(&selector, cx);
    }

    fn next_problem_action(
        &mut self,
        _: &NextProblem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_problem_cursor(1);
        // The problems list is capped, so the next problem follows the cursor into view.
        self.problems_scroll.scroll_to_item(self.problem_cursor);
        self.jump_to_problem(window, cx);
    }

    fn release_apply_lock(&mut self, cx: &mut Context<Self>) {
        self.applying = false;
        let editable = self.yaml_available;
        self.editable = editable;
        self.yaml_view
            .update(cx, |view, cx| view.set_editable(editable, cx));
    }

    fn mark_applied(&mut self, request: ApplyRequest, cx: &mut Context<Self>) {
        if self.current_apply_target() != Some(&request.target) {
            self.apply_request = None;
            self.release_apply_lock(cx);
            self.apply_error = Some(
                "The selected object changed. Select the object again, then apply the YAML."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        if self.yaml_view.read(cx).text().as_deref() != Some(request.yaml.as_str()) {
            self.apply_request = None;
            self.release_apply_lock(cx);
            self.apply_error = Some(
                "The YAML changed during the apply operation. Review the YAML, then apply again."
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        self.apply_request = None;
        self.yaml_view
            .update(cx, |view, cx| view.clear_diagnostics(cx));
        self.yaml_view.update(cx, |view, cx| view.mark_saved(cx));
        // `original` stays the text the cluster reported, so Revert can still go back to it. The
        // text that was applied is kept beside it instead of overwriting it.
        self.applied_text = Some(request.yaml);
        self.validation_error = None;
        self.apply_error = None;
        self.conflict_owners = None;
        self.applied_at = Some(Instant::now());
        self.release_apply_lock(cx);
        self.action_scroll.scroll_to_item(0);
        if !self.load_pending(cx) {
            cx.notify();
        }
    }

    pub fn apply_finished_for(
        &mut self,
        request: ApplyRequest,
        result: Result<ApplyOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.apply_request.as_ref() else {
            return;
        };
        if active != &request || self.current_apply_target() != Some(&request.target) {
            return;
        }
        match result {
            Ok(ApplyOutcome::Applied(object))
                if !applied_object_matches(&request.target, object.as_ref()) =>
            {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(
                    "The apply result does not match the selected object. Refresh the object, then apply again."
                        .to_owned(),
                );
                cx.notify();
            }
            Ok(ApplyOutcome::Applied(_)) => self.mark_applied(request, cx),
            Ok(ApplyOutcome::Conflict { owners }) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.conflict_owners = Some(owners);
                self.apply_error = None;
                cx.notify();
            }
            Ok(ApplyOutcome::Unknown { .. }) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(APPLY_UNKNOWN_REASON.to_owned());
                self.conflict_owners = None;
                cx.notify();
            }
            Err(reason) => {
                self.apply_request = None;
                self.release_apply_lock(cx);
                self.apply_error = Some(if Self::apply_error_is_unknown(&reason) {
                    APPLY_UNKNOWN_REASON.to_owned()
                } else {
                    reason
                });
                self.conflict_owners = None;
                cx.notify();
            }
        }
    }

    pub fn apply_finished(
        &mut self,
        request: ApplyRequest,
        result: Result<ApplyOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_finished_for(request, result, cx);
    }

    /// Restores the text the cluster last reported.
    ///
    /// This is the only way back after an apply, so it stays available while the editor holds
    /// applied text rather than the text the server sent.
    pub fn revert(&mut self, cx: &mut Context<Self>) {
        self.invalidate_apply();
        if self.load_pending(cx) {
            return;
        }
        let original = self.original.clone();
        self.load_yaml(original, cx);
    }

    // Describe and Events loading

    fn ensure_tab_data(&mut self, cx: &mut Context<Self>) {
        match self.active_tab {
            1 => self.ensure_describe(false, cx),
            2 => self.ensure_events(false, cx),
            3 => self.update_metrics_sampling(cx),
            _ => {}
        }
    }

    fn selection_uid(&self) -> Option<String> {
        self.selection.as_ref().map(|object| object.uid.clone())
    }

    fn ensure_describe(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let uid = selection.uid.clone();
        if !force
            && matches!(
                self.describe_states.get(&uid),
                Some(LoadState::Loading | LoadState::Ready(_))
            )
        {
            return;
        }
        // Describe shows the newest events, so it reads the same cache the Events tab reads. One
        // request serves both, and the "+N more" count can no longer disagree with that tab.
        self.ensure_events(false, cx);
        let Some(source) = self.source.clone() else {
            self.describe_states
                .insert(uid, LoadState::Failed(NOT_CONNECTED_REASON.to_owned()));
            cx.notify();
            return;
        };
        let task: OpsFuture<DescribeData> = source.describe(&selection);
        self.describe_states.insert(uid.clone(), LoadState::Loading);
        self.arm_load_deadline(LoadKind::Describe, uid.clone(), cx);
        let epoch = self.load_epoch;
        self.describe_task = Some(cx.spawn(async move |this, cx| {
            let result = task
                .await
                .and_then(|data| prepare_describe_data(data, &uid));
            this.update(cx, |panel, cx| {
                panel.on_loaded(&epoch, result, &mut |panel, result| {
                    panel.describe_states.insert(uid.clone(), result);
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn ensure_events(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let uid = selection.uid.clone();
        if !force {
            match self.events_states.get(&uid) {
                Some(entry) if matches!(entry.state, LoadState::Loading) => return,
                Some(entry)
                    if matches!(entry.state, LoadState::Ready(_))
                        && entry.fetched_at.elapsed() < EVENTS_TTL =>
                {
                    return;
                }
                _ => {}
            }
        }
        let Some(source) = self.source.clone() else {
            self.events_states.insert(
                uid,
                EventsEntry {
                    fetched_at: Instant::now(),
                    state: LoadState::Failed(NOT_CONNECTED_REASON.to_owned()),
                },
            );
            cx.notify();
            return;
        };
        let task: OpsFuture<Vec<DynamicObject>> = source.events(&selection);
        self.events_states.insert(
            uid.clone(),
            EventsEntry {
                fetched_at: Instant::now(),
                state: LoadState::Loading,
            },
        );
        self.arm_load_deadline(LoadKind::Events, uid.clone(), cx);
        let epoch = self.load_epoch;
        self.events_task = Some(cx.spawn(async move |this, cx| {
            let result = task
                .await
                .map(|events| Arc::new(events_newest_first(events)));
            this.update(cx, |panel, cx| {
                panel.on_loaded(&epoch, result, &mut |panel, result| {
                    panel.events_states.insert(
                        uid.clone(),
                        EventsEntry {
                            fetched_at: Instant::now(),
                            state: result,
                        },
                    );
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fails a load that never answers, so the tab offers Retry again.
    ///
    /// A `Loading` entry is cached so a second visit does not start a duplicate request,
    /// which means a request that never resolves would pin the tab in a spinner for the
    /// whole session. The token check keeps a late watchdog from expiring a newer request.
    fn arm_load_deadline(&mut self, kind: LoadKind, uid: String, cx: &mut Context<Self>) {
        let epoch = self.load_epoch;
        let token = match kind {
            LoadKind::Describe => {
                self.describe_token = self.describe_token.wrapping_add(1);
                self.describe_token
            }
            LoadKind::Events => {
                self.events_token = self.events_token.wrapping_add(1);
                self.events_token
            }
        };
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOAD_DEADLINE).await;
            this.update(cx, |panel, cx| {
                if panel.load_epoch != epoch || panel.load_token(kind) != token {
                    return;
                }
                if !panel.still_loading(kind, &uid) {
                    return;
                }
                // The request is dropped, so Retry starts a new one.
                match kind {
                    LoadKind::Describe => {
                        panel.describe_task = None;
                        panel.describe_states.insert(
                            uid.clone(),
                            LoadState::Failed(LOAD_TIMEOUT_REASON.to_owned()),
                        );
                    }
                    LoadKind::Events => {
                        panel.events_task = None;
                        panel.events_states.insert(
                            uid.clone(),
                            EventsEntry {
                                fetched_at: Instant::now(),
                                state: LoadState::Failed(LOAD_TIMEOUT_REASON.to_owned()),
                            },
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        });
        match kind {
            LoadKind::Describe => self.describe_deadline = Some(task),
            LoadKind::Events => self.events_deadline = Some(task),
        }
    }

    fn load_token(&self, kind: LoadKind) -> u64 {
        match kind {
            LoadKind::Describe => self.describe_token,
            LoadKind::Events => self.events_token,
        }
    }

    fn still_loading(&self, kind: LoadKind, uid: &str) -> bool {
        match kind {
            LoadKind::Describe => matches!(self.describe_states.get(uid), Some(LoadState::Loading)),
            LoadKind::Events => self
                .events_states
                .get(uid)
                .is_some_and(|entry| matches!(entry.state, LoadState::Loading)),
        }
    }

    /// Discards stale results and stores current data.
    fn on_loaded<T>(
        &mut self,
        epoch: &u64,
        result: Result<T, String>,
        store: &mut dyn FnMut(&mut Self, LoadState<T>),
    ) {
        if *epoch != self.load_epoch {
            return;
        }
        let state = match result {
            Ok(value) => LoadState::Ready(value),
            Err(reason) => LoadState::Failed(reason),
        };
        store(self, state);
        self.prune_caches();
    }

    fn prune_caches(&mut self) {
        let current = self.selection_uid();
        if self.describe_states.len() > CACHE_CAPACITY {
            self.describe_states
                .retain(|uid, _| current.as_deref() == Some(uid.as_str()));
        }
        if self.events_states.len() > CACHE_CAPACITY {
            self.events_states
                .retain(|uid, _| current.as_deref() == Some(uid.as_str()));
        }
    }

    fn copied(&self) -> bool {
        self.copied_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
    }

    // Keyboard scrolling

    /// Scrolls a scroll container by a distance, and reports whether it moved.
    ///
    /// The offset is clamped to the content, so a key at either end changes nothing and stays
    /// available to the rest of the app instead of being swallowed.
    ///
    /// A GPUI scroll offset is the distance from the top of the content to the top of the
    /// viewport, so it grows more negative as the view moves down: a scroll towards the end of
    /// the content passes a negative distance.
    fn scroll_by(&self, handle: &ScrollHandle, distance: f32) -> bool {
        let offset = handle.offset();
        let limit = f32::from(handle.max_offset().y);
        if limit <= 0.0 {
            return false;
        }
        let next = px((f32::from(offset.y) + distance).clamp(-limit, 0.0));
        if (next - offset.y).abs() < px(0.5) {
            return false;
        }
        handle.set_offset(point(offset.x, next));
        true
    }

    /// Scrolls a uniform list by a distance, with the same clamping and sign as
    /// [`Self::scroll_by`].
    fn scroll_list_by(&self, handle: &UniformListScrollHandle, distance: f32) -> bool {
        let mut state = handle.0.borrow_mut();
        // A pending scroll_to_item would win over the offset on the next prepaint.
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        let limit = f32::from(state.base_handle.max_offset().y);
        if limit <= 0.0 {
            return false;
        }
        let next = px((f32::from(offset.y) + distance).clamp(-limit, 0.0));
        if (next - offset.y).abs() < px(0.5) {
            return false;
        }
        state.base_handle.set_offset(point(offset.x, next));
        true
    }

    /// Height of a scroll viewport, so a page key moves a page instead of a guess.
    fn viewport_height(handle: &ScrollHandle) -> f32 {
        let height = f32::from(handle.bounds().size.height);
        if height.is_finite() && height > 0.0 {
            height
        } else {
            f32::from(design::size::ROW) * 8.
        }
    }

    /// Height of the events viewport, taken from the handle the list renders through.
    fn viewport_height_events(handle: &UniformListScrollHandle) -> f32 {
        let height = f32::from(handle.0.borrow().base_handle.bounds().size.height);
        if height.is_finite() && height > 0.0 {
            height
        } else {
            f32::from(design::size::ROW) * 8.
        }
    }

    /// Distance one arrow key moves: one row, so a line of text follows the key.
    fn scroll_step() -> f32 {
        f32::from(design::size::ROW)
    }

    fn scroll_handle_to_top(handle: &ScrollHandle) -> bool {
        let offset = handle.offset();
        if offset.y == px(0.) {
            return false;
        }
        handle.set_offset(point(offset.x, px(0.)));
        true
    }

    fn scroll_handle_to_bottom(handle: &ScrollHandle) -> bool {
        if handle.max_offset().y <= px(0.) {
            return false;
        }
        handle.scroll_to_bottom();
        true
    }

    fn scroll_list_to_top(handle: &UniformListScrollHandle) -> bool {
        let mut state = handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        if offset.y == px(0.) {
            return false;
        }
        state.base_handle.set_offset(point(offset.x, px(0.)));
        true
    }

    fn scroll_list_to_bottom(handle: &UniformListScrollHandle) -> bool {
        let mut state = handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        if state.base_handle.max_offset().y <= px(0.) {
            return false;
        }
        state.base_handle.scroll_to_bottom();
        true
    }

    fn on_describe_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height(&self.describe_scroll);
        let handle = self.describe_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_by(&handle, step),
            "down" => self.scroll_by(&handle, -step),
            "pageup" => self.scroll_by(&handle, page),
            "pagedown" => self.scroll_by(&handle, -page),
            "home" => Self::scroll_handle_to_top(&handle),
            "end" => Self::scroll_handle_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_events_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height_events(&self.events_scroll);
        let handle = self.events_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_list_by(&handle, step),
            "down" => self.scroll_list_by(&handle, -step),
            "pageup" => self.scroll_list_by(&handle, page),
            "pagedown" => self.scroll_list_by(&handle, -page),
            "home" => Self::scroll_list_to_top(&handle),
            "end" => Self::scroll_list_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_metrics_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let step = Self::scroll_step();
        let page = Self::viewport_height(&self.metrics_scroll);
        let handle = self.metrics_scroll.clone();
        let moved = match event.keystroke.key.as_str() {
            "up" => self.scroll_by(&handle, step),
            "down" => self.scroll_by(&handle, -step),
            "pageup" => self.scroll_by(&handle, page),
            "pagedown" => self.scroll_by(&handle, -page),
            "home" => Self::scroll_handle_to_top(&handle),
            "end" => Self::scroll_handle_to_bottom(&handle),
            _ => return,
        };
        if moved {
            cx.stop_propagation();
            cx.notify();
        }
    }

    // Value rows

    /// A snapshot of what the value rows need, taken once per Describe render.
    fn value_rows(&mut self, cx: &mut Context<Self>) -> ValueRows {
        self.value_focus.borrow_mut().begin_render();
        let copied = self
            .value_copied_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
            .then(|| self.value_cursor.clone())
            .into_iter()
            .flatten()
            .collect();
        ValueRows {
            focus: self.value_focus.clone(),
            expanded: Rc::new(self.expanded_values.clone()),
            copied: Rc::new(copied),
            panel: cx.weak_entity(),
        }
    }

    /// Whether a value row is expanded, for commands and tests.
    #[allow(dead_code)]
    fn value_is_expanded(&self, selector: &str) -> bool {
        self.expanded_values.contains(selector)
    }

    /// The focus handle a value row was given in the last render.
    #[allow(dead_code)]
    fn value_focus_handle(&self, selector: &str) -> Option<FocusHandle> {
        self.value_focus.borrow().get(selector)
    }

    fn toggle_value_expansion(&mut self, selector: &str, cx: &mut Context<Self>) {
        self.value_cursor = Some(selector.to_owned());
        if !self.expanded_values.remove(selector) {
            self.expanded_values.insert(selector.to_owned());
        }
        cx.notify();
    }

    /// Copies a value row in full, which is what a truncated row hides.
    fn copy_value(&mut self, selector: &str, cx: &mut Context<Self>) {
        self.value_cursor = Some(selector.to_owned());
        let Some(value) = self.value_text(selector) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        self.value_copied_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            this.update(cx, |panel, cx| {
                panel.value_copied_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The full text of a value row, recorded while the body rendered it.
    fn value_text(&self, selector: &str) -> Option<String> {
        self.value_focus.borrow().text(selector)
    }

    /// Whether the copy feedback of a value row is still showing.
    #[allow(dead_code)]
    fn value_copied(&self, selector: &str) -> bool {
        self.value_cursor.as_deref() == Some(selector)
            && self
                .value_copied_at
                .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
    }

    // YAML problems

    /// The parse problems the editor reports, which gate Apply.
    fn diagnostics(&self, cx: &App) -> Vec<Diagnostic> {
        self.yaml_view.read(cx).diagnostics().to_vec()
    }

    /// Whether the editor's problems differ from the ones the panel last rendered.
    ///
    /// The editor validates after a typing pause, so the problems arrive without an edit. The
    /// panel owns the problems list and the disabled Apply button, so it has to hear about that
    /// parse; the comparison keeps a caret blink or a scroll from repainting the whole panel.
    fn problems_changed(&mut self, current: &[Diagnostic]) -> bool {
        if self.rendered_problems == current {
            return false;
        }
        self.rendered_problems = current.to_vec();
        true
    }

    fn move_problem_cursor(&mut self, delta: isize) {
        let count = self.problem_count;
        if count == 0 {
            return;
        }
        let last = count - 1;
        let next = (self.problem_cursor as isize + delta).clamp(0, last as isize);
        self.problem_cursor = next as usize;
    }

    /// Reveals the problem under the cursor and puts the caret in the editor.
    fn jump_to_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self
            .problem_cursor
            .min(self.problem_count.saturating_sub(1));
        let Some(diagnostic) = self.yaml_view.read(cx).diagnostics().get(index).cloned() else {
            return;
        };
        self.yaml_view
            .update(cx, |view, cx| view.scroll_to_line(diagnostic.line, cx));
        self.yaml_view.update(cx, |view, cx| view.focus(window, cx));
    }

    fn on_problems_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let handled = match key {
            "up" => {
                self.move_problem_cursor(-1);
                true
            }
            "down" => {
                self.move_problem_cursor(1);
                true
            }
            "pageup" => {
                self.move_problem_cursor(-self.problem_page());
                true
            }
            "pagedown" => {
                self.move_problem_cursor(self.problem_page());
                true
            }
            "home" => {
                self.problem_cursor = 0;
                true
            }
            "end" => {
                self.problem_cursor = self.problem_count.saturating_sub(1);
                true
            }
            "enter" | "return" | "space" => {
                self.jump_to_problem(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            // The list is capped, so the cursor row has to come into view: a highlighted problem
            // the reader cannot see is the same as no highlight.
            self.problems_scroll.scroll_to_item(self.problem_cursor);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Rows one page key moves in the problems list: the rows the capped viewport shows.
    fn problem_page(&self) -> isize {
        let page = Self::viewport_height(&self.problems_scroll) / f32::from(design::size::ROW);
        page.floor().max(1.) as isize
    }

    /// Copies the YAML in the editor.
    fn copy_yaml(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.yaml_view.read(cx).text() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            this.update(cx, |panel, cx| {
                panel.copied_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // Common rendering

    /// Label of the active tab, for the tab panel and the review copy.
    fn tab_label(&self, index: usize) -> &'static str {
        match index {
            1 => "Describe",
            2 => "Events",
            3 => "Metrics",
            _ => "YAML",
        }
    }

    /// The object this Inspector is showing, on every tab.
    ///
    /// The toolbar titles are 14px muted text that a 240px Inspector clips, and the Metrics tab
    /// used to show a bare name. A persistent header names the kind, the name, and the age, so no
    /// tab can be read without knowing which object it belongs to. It is also the *only* place
    /// object identity is written down: the YAML band kept a second copy of it that survived on
    /// about 27 of its 53 characters, so the header is the single source and the bands name their
    /// document instead.
    fn render_identity(&self, cx: &Context<Self>) -> AnyElement {
        let Some(selection) = self.selection.as_ref() else {
            return h_flex()
                .id("inspector-identity")
                .debug_selector(|| "inspector-identity".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .px(space::SM)
                .gap(space::SM)
                .items_center()
                .bg(cx.theme().colors().toolbar_background.alpha(1.0))
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .role(Role::Region)
                .aria_label("Inspector. No resource selected.")
                .child(common::label_panel_title("No resource selected").color(Color::Muted))
                .into_any_element();
        };
        let kind = if selection.resource.kind.is_empty() {
            "Resource"
        } else {
            selection.resource.kind.as_str()
        };
        // The age comes from the fetched object, so it appears once Describe or Metrics has read
        // it rather than guessing from a selection that carries no timestamp.
        let age = self
            .selection
            .as_ref()
            .and_then(|object| self.describe_states.get(&object.uid))
            .and_then(|state| match state {
                LoadState::Ready(data) => data.object.metadata.creation_timestamp.as_ref(),
                _ => None,
            })
            .map(|created| format!("{} old", format_age(created.0.as_second())));
        // The name takes the reclaimed width. It used to share the row with a `{kind} ·
        // {namespace}` line that said what the icon beside it already said and repeated two
        // columns of the active table; at the 336px default that left the name - the one thing
        // this bar exists to say - with the least room on the row.
        //
        // The namespace is not lost: it is a column of the table the selection came from, and the
        // full identity is this bar's accessible name and the name's tooltip, so a screen reader
        // and a hover both still get it.
        let mut name = div()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(common::label_panel_title(selection.name.clone()).color(Color::Default));
        name.interactivity()
            .tooltip(Tooltip::text(object_accessible_identity(selection)));
        h_flex()
            .id("inspector-identity")
            .debug_selector(|| "inspector-identity".to_owned())
            .flex_none()
            .w_full()
            .min_w(px(0.))
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(cx.theme().colors().toolbar_background.alpha(1.0))
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .role(Role::Region)
            .aria_label(object_accessible_identity(selection))
            .child(
                Icon::new(design::kind_icon(kind))
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(name)
            .when_some(age, |this, age| {
                this.child(label_small(age).color(Color::Muted).flex_none())
            })
            .into_any_element()
    }

    fn render_tabs(&self, cx: &Context<Self>) -> AnyElement {
        let tab_count = self.tab_count();
        let active_tab = self.active_tab.min(tab_count.saturating_sub(1));
        let focused_tab = self.focused_tab.min(tab_count.saturating_sub(1));
        let colors = cx.theme().colors();
        let mut tabs = TABS.to_vec();
        if self.metrics_tab_visible() {
            tabs.push(METRICS_TAB);
        }
        let visible_tab_count = tabs.len();
        let focus = self.tab_focus.clone();
        let items = tabs.iter().copied().enumerate().map(|(index, tab)| {
            let selected = index == active_tab;
            let focused = index == focused_tab;
            h_flex()
                .id(("inspector-tab", index))
                .debug_selector(move || format!("inspector-tab-{index}"))
                .relative()
                .h_full()
                .flex_none()
                .px(space::SM)
                .gap(space::XS)
                .items_center()
                .border_1()
                .border_color(colors.border_transparent)
                .cursor_pointer()
                .when(focused, |this| this.track_focus(&focus))
                .role(Role::Tab)
                .aria_label(tab.label)
                .aria_selected(selected)
                .aria_position_in_set(index + 1)
                .aria_size_of_set(visible_tab_count)
                .aria_keyshortcuts("Enter Space ArrowLeft ArrowRight Home End")
                .accessibility_id(format!("inspector-tab-{index}"))
                // One treatment for the whole strip: the active tab carries the raised surface
                // `DESIGN.md` §3.4 assigns it and the accent rail, the focused tab keeps a focus
                // border, and an inactive tab stays muted until it is hovered. The wash used to be
                // `element_selected`, a full accent slab on a navigation state; accent is the
                // focus channel inside lists, and the previous round took the same slab off the
                // table headers for exactly that reason.
                .when(selected, |this| this.bg(design::surface::tab_active(cx)))
                .when(!selected, |this| {
                    this.hover(|this| this.bg(colors.element_hover))
                })
                .active(|this| this.bg(colors.element_active))
                .focus_visible(|style| style.border_color(colors.border_focused))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_tab(index, window, cx);
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.control
                        || event.keystroke.modifiers.alt
                        || event.keystroke.modifiers.platform
                    {
                        return;
                    }
                    if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                        this.activate_tab(index, window, cx);
                    } else if let Some(target) =
                        tab_focus_target(index, visible_tab_count, event.keystroke.key.as_str())
                    {
                        this.focused_tab = target;
                        this.tabs_scroll.scroll_to_item(tab_scroll_index(target));
                        cx.notify();
                    } else {
                        return;
                    }
                    cx.stop_propagation();
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
                .child(label_text(tab.label).color(if selected {
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
                })
                .into_any_element()
        });
        h_flex()
            .id("inspector-tabs")
            .debug_selector(|| "inspector-tabs".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TAB_BAR)
            .bg(colors.tab_bar_background.alpha(1.0))
            .border_b_1()
            .border_color(colors.border)
            .tab_group()
            .child(
                h_flex()
                    .id("inspector-tabs-scroll")
                    .role(Role::TabList)
                    .aria_label(INSPECTOR_TAB_LIST_LABEL)
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.tabs_scroll)
                    .children(items),
            )
            .into_any_element()
    }

    fn status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if let Some(error) = &self.validation_error {
            return Some(status_message(
                Severity::Error,
                "YAML is invalid. Fix the syntax, then apply.",
                Some(error.clone()),
                cx,
            ));
        }
        if let Some(owners) = &self.conflict_owners {
            return Some(status_message(
                Severity::Warning,
                "Apply conflict. Review your changes, then apply again.",
                Some(format!("Managed by: {}", owners.join(", "))),
                cx,
            ));
        }
        if let Some(error) = &self.apply_error {
            if error == APPLY_UNAVAILABLE_REASON {
                return Some(status_message(
                    Severity::Warning,
                    "Apply unavailable",
                    Some(error.clone()),
                    cx,
                ));
            }
            if error == APPLY_UNKNOWN_REASON {
                return Some(status_message(
                    Severity::Warning,
                    "Apply result is unknown",
                    Some(error.clone()),
                    cx,
                ));
            }
            return Some(status_message(
                Severity::Error,
                "Apply failed. Cancel the changes or try again.",
                Some(error.clone()),
                cx,
            ));
        }
        if self.applying {
            return Some(
                h_flex()
                    .id("yaml-applying-status")
                    .flex_none()
                    .gap(space::XS)
                    .items_center()
                    .role(Role::Status)
                    .aria_label("Applying changes")
                    .child(spinner(
                        IconName::LoadCircle,
                        Color::Accent,
                        IconSize::XSmall,
                        cx,
                    ))
                    .child(label_small("Applying changes…").color(Color::Default))
                    .into_any_element(),
            );
        }
        if let Some(reason) = &self.metrics_notice {
            return Some(status_message(
                Severity::Warning,
                "Metrics unavailable",
                Some(reason.clone()),
                cx,
            ));
        }
        if self.has_pending() {
            let (title, reason) = self.apply_blocked_by_pending().unwrap_or_else(|| {
                (
                    "Unsaved changes",
                    "Apply or cancel before the next selection loads.".to_owned(),
                )
            });
            return Some(status_message(Severity::Warning, title, Some(reason), cx));
        }
        if self.yaml_available && !self.has_apply_handler() {
            let title = if self.yaml_view.read(cx).is_dirty() {
                "Unsaved changes. Apply is unavailable."
            } else {
                "Apply unavailable"
            };
            return Some(status_message(
                Severity::Warning,
                title,
                Some(APPLY_UNAVAILABLE_REASON.to_owned()),
                cx,
            ));
        }
        if self.yaml_view.read(cx).is_dirty() {
            return Some(status_message(
                Severity::Warning,
                "Unsaved changes",
                None,
                cx,
            ));
        }
        if self
            .applied_at
            .is_some_and(|at| at.elapsed() < COPIED_FEEDBACK)
        {
            return Some(status_message(Severity::Success, "Applied", None, cx));
        }
        None
    }

    fn render_yaml_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let copied = self.copied();
        let applying = self.applying;
        let dirty = self.yaml_view.read(cx).is_dirty();
        let apply_available = self.has_apply_handler();
        let pending_reason = self.apply_blocked_by_pending();
        // A parse problem is a hard stop: the editor reports every problem it finds, and a
        // request built from broken YAML would only be rejected by the API server.
        let problems = self.diagnostics(cx);
        let problem_count = problems.len();
        let apply_tooltip = match (&pending_reason, problem_count > 0, apply_available) {
            (Some((_, reason)), _, _) => reason.clone(),
            (None, true, _) => APPLY_INVALID_REASON.to_owned(),
            (None, false, true) => "Review the change, then apply it to the cluster".to_owned(),
            (None, false, false) => APPLY_UNAVAILABLE_REASON.to_owned(),
        };
        let apply_binding =
            UiKeyBinding::for_action_in(&ApplyYaml, self.yaml_view.read(cx).focus_handle(), cx);
        let mut actions = vec![
            div()
                .debug_selector(|| "yaml-action-apply".to_owned())
                .child(
                    Button::new("yaml-apply", "Apply")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .size(ButtonSize::Medium)
                        .width(px(INSPECTOR_ACTION_BUTTON_WIDTH))
                        .track_focus(&self.apply_focus)
                        .tab_index(APPLY_TAB_INDEX)
                        .disabled(
                            applying
                                || !dirty
                                || !apply_available
                                || pending_reason.is_some()
                                || problem_count > 0,
                        )
                        .key_binding(apply_binding)
                        .tooltip(Tooltip::text(apply_tooltip))
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.action_scroll.scroll_to_item(0);
                            this.apply(cx);
                        }))
                        .into_any_element(),
                )
                .into_any_element(),
        ];

        // The control stays while the editor holds text the cluster did not send, so the way back
        // survives an apply. Before an apply it reads Cancel, afterwards Revert.
        let diverged = matches!(
            (self.original.as_deref(), self.yaml_view.read(cx).text()),
            (Some(saved), Some(text)) if saved != text
        );
        if diverged {
            let label = if dirty { "Cancel" } else { "Revert" };
            let tooltip = if dirty {
                "Discard the local changes and restore the text the cluster reported"
            } else {
                "Restore the text the cluster reported before this change"
            };
            let revert_binding = UiKeyBinding::for_action_in(&RevertYaml, &self.focus_handle, cx);
            actions.push(
                div()
                    .debug_selector(|| "yaml-action-cancel".to_owned())
                    .child(
                        Button::new("yaml-cancel", label)
                            .style(ButtonStyle::OutlinedGhost)
                            .size(ButtonSize::Medium)
                            .width(px(INSPECTOR_ACTION_BUTTON_WIDTH))
                            .track_focus(&self.revert_focus)
                            .tab_index(REVERT_TAB_INDEX)
                            .disabled(applying)
                            .key_binding(revert_binding)
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.action_scroll.scroll_to_item(0);
                                this.revert(cx);
                            }))
                            .into_any_element(),
                    )
                    .into_any_element(),
            );
        }
        let copy_binding = control_chord("k8s_inspector::CopyYaml", cx);
        actions.push(
            div()
                .debug_selector(|| "yaml-action-copy".to_owned())
                .child(
                    IconButton::new(
                        "copy-yaml",
                        if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        },
                    )
                    .size(ButtonSize::Medium)
                    .width(design::size::CONTROL)
                    .icon_size(IconSize::XSmall)
                    .icon_color(if copied {
                        Color::Custom(Severity::Success.marker(cx))
                    } else {
                        Color::Muted
                    })
                    .tooltip(inspector_control_tooltip(
                        if copied { "Copied" } else { "Copy YAML" },
                        copy_binding,
                    ))
                    .aria_label(if copied { "Copied" } else { "Copy YAML" })
                    .track_focus(&self.copy_yaml_focus)
                    .tab_index(COPY_YAML_TAB_INDEX)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.action_scroll.scroll_to_item(0);
                        this.copy_yaml(cx);
                    }))
                    .into_any_element(),
                )
                .into_any_element(),
        );
        // No title here. The identity bar forty pixels above already names the object at
        // `label_panel_title` and full contrast, and this band had room for about 27 of the 53
        // characters `YAML · Pod nginx-7d8f9c-x2k9p in namespace default` needs at the 336px
        // default width - so it kept the kind and the start of the name and dropped the
        // namespace. Once the buffer is dirty the `Cancel` control takes its place and the title
        // falls to about 16 characters, which is the moment the reader is about to apply.
        // `render_identity` is the single source of object identity; the band's own icon says
        // which document this is.
        let metadata = self.status(cx).unwrap_or_else(|| {
            h_flex()
                .id("yaml-clean-metadata")
                .debug_selector(|| "yaml-clean-metadata".to_owned())
                .min_w(px(0.))
                .gap(space::XS)
                .items_center()
                .child(
                    Icon::new(IconName::FileCode)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        });
        inspector_toolbar(
            "yaml-action-toolbar",
            div()
                .flex_1()
                .min_w(px(0.))
                .h(design::size::ROW)
                .items_center()
                .overflow_hidden()
                .child(metadata)
                .into_any_element(),
            h_flex()
                .id("yaml-action-scroll")
                .flex_none()
                .min_w(px(0.))
                .h(design::size::ROW)
                .gap(space::XS)
                .items_center()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.action_scroll)
                .children(actions)
                .into_any_element(),
            cx,
        )
    }

    fn render_yaml(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // A fetch that failed is an anomaly, so it gets the same treatment Describe and Events
        // already give theirs: a severity, `Role::Alert`, the reason, and a Retry. The empty
        // selection below stays reserved for an actual empty selection, where `Select a row` is
        // the correct instruction rather than a guess.
        if let Some(reason) = self.yaml_error.clone() {
            return load_error(
                LoadErrorParts {
                    title: YAML_LOAD_FAILED_TITLE,
                    hint: load_failure_hint(&reason),
                    retry_label: YAML_RETRY_LABEL,
                    reason,
                    retry_focus: self.yaml_retry_focus.clone(),
                    retry_tab_index: INSPECTOR_YAML_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.reload_yaml(cx)),
            );
        }
        if !self.yaml_available {
            // The same sentence the editor shows for the same state, from one definition. The two
            // used to be written out separately and disagreed on punctuation.
            return empty_state(
                IconName::FileCode,
                crate::yaml_editor::EMPTY_TITLE,
                crate::yaml_editor::EMPTY_HINT,
            );
        }
        let source_label = self
            .selection
            .as_ref()
            .map(|selection| format!("YAML for {}", object_accessible_identity(selection)))
            .unwrap_or_else(|| "YAML editor. No resource selected.".to_owned());
        v_flex()
            .size_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .overflow_hidden()
            .child(self.render_yaml_toolbar(cx))
            .when_some(self.render_problems(cx), |this, problems| {
                this.child(problems)
            })
            .when_some(self.render_apply_review(cx), |this, review| {
                this.child(review)
            })
            .child(
                div()
                    .id("yaml-source-panel")
                    .role(Role::Region)
                    .aria_label(source_label)
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(self.yaml_view.clone()),
            )
            .into_any_element()
    }

    /// The parse problems that block Apply, as a list the keyboard can walk.
    fn render_problems(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let problems = self.diagnostics(cx);
        self.problem_count = problems.len();
        if problems.is_empty() {
            return None;
        }
        // The key handler reads the count from the panel, because it has no context to read the
        // editor with.
        let count = problems.len();
        let cursor = self.problem_cursor.min(count - 1);
        let colors = cx.theme().colors();
        let selected_bg = colors.element_selected;
        let focus_border = colors.border_focused;
        let marker = Color::Custom(Severity::Error.marker(cx));
        let weak = cx.weak_entity();
        // A problem position is data, and the reader can set the data font size. A row drawn at
        // the default size next to a row drawn at the configured one puts two sizes in one list.
        let data = crate::settings::data_typography(cx);
        let rows = problems.iter().enumerate().map(|(index, diagnostic)| {
            let focused = index == cursor;
            let position = format!(
                "Line {}, column {}",
                diagnostic.line + 1,
                diagnostic.column + 1
            );
            let message = diagnostic.short_message().to_owned();
            let mut detail = div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label_small(message.clone()).color(Color::Muted));
            detail
                .interactivity()
                .tooltip(Tooltip::text(diagnostic.message.clone()));
            let aria = format!("{position}. {message}");
            let panel = weak.clone();
            h_flex()
                .id(("yaml-problem", index))
                .debug_selector(move || format!("yaml-problem-{index}"))
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .gap(space::SM)
                .items_center()
                .role(Role::ListItem)
                .aria_label(aria)
                .aria_selected(focused)
                .when(focused, |this| this.bg(selected_bg))
                .child(
                    Icon::new(design::severity_icon(Severity::Error))
                        .size(IconSize::XSmall)
                        .color(marker),
                )
                .child(
                    div().flex_none().child(
                        Label::new(position)
                            .size(LabelSize::Custom(rems_from_px(f32::from(data.size)))),
                    ),
                )
                .child(detail)
                .on_click(move |_: &ClickEvent, window, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |panel, cx| {
                            panel.problem_cursor = index;
                            panel.jump_to_problem(window, cx);
                        });
                    }
                })
                .into_any_element()
        });
        let title = format!(
            "{count} problem{} block Apply",
            if count == 1 { "" } else { "s" }
        );
        Some(
            v_flex()
                .id("yaml-problems")
                .debug_selector(|| "yaml-problems".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .gap(space::XS)
                .py(space::XS)
                .px(space::SM)
                .bg(colors.toolbar_background.alpha(1.0))
                .border_b_1()
                .border_color(colors.border)
                .role(Role::List)
                .aria_label(format!("YAML problems, {count} found"))
                .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End Enter")
                .track_focus(&self.problems_focus)
                .tab_index(INSPECTOR_PROBLEMS_TAB_INDEX)
                // The rail is always reserved, so taking and leaving focus cannot slide the
                // list sideways.
                .border_l_2()
                .border_color(colors.border_transparent)
                .focus_visible(move |style| style.border_color(focus_border))
                .on_key_down(cx.listener(Self::on_problems_key_down))
                .child(
                    h_flex()
                        .w_full()
                        .gap(space::XS)
                        .items_center()
                        .child(
                            Icon::new(design::severity_icon(Severity::Error))
                                .size(IconSize::XSmall)
                                .color(marker),
                        )
                        .child(common::label_panel_title(title).color(Color::Muted)),
                )
                .child(
                    div()
                        .id("yaml-problem-list")
                        .debug_selector(|| "yaml-problem-list".to_owned())
                        .w_full()
                        .min_w(px(0.))
                        // The cap is what keeps a document with dozens of problems from pushing
                        // the editor out of the panel. Every problem stays in the list; the
                        // focus handle below scrolls the rest into reach.
                        .max_h(problems_list_max_height())
                        .overflow_y_scroll()
                        .track_scroll(&self.problems_scroll)
                        .children(rows),
                )
                // A capped list says so, because this is the list that blocks the write. The
                // title keeps the real total, so the reader never mistakes the cap for the whole
                // list.
                .when(count > PROBLEMS_VISIBLE_ROWS, |this| {
                    let count_text = format!("Showing {PROBLEMS_VISIBLE_ROWS} of {count}.");
                    this.child(
                        label_small(format!("{count_text} The arrow keys reach the rest."))
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
        )
    }

    /// The review that stands between a parsed change and a write.
    ///
    /// It names the object a request would touch, shows the local change against the text the
    /// cluster last reported, keeps the safe action first in the tab order, and can ask the
    /// server to validate the document without storing it. Nothing here writes: only "Apply to
    /// cluster" does, and it is never the default focus.
    ///
    /// The review's own line about what has and has not been verified.
    ///
    /// A local diff answers "what text changed". It cannot answer "would the server accept this",
    /// so the copy states which of the two the reader is looking at rather than implying a
    /// guarantee the app has not made.
    fn apply_check_line(&self) -> AnyElement {
        let (text, color) = match &self.apply_check {
            ApplyCheckState::NotRun => (
                "Not checked against the cluster. The diff below is a local comparison; the \
                 server has not seen this document."
                    .to_owned(),
                Color::Muted,
            ),
            ApplyCheckState::Running => (
                "Asking the server to validate this document without storing it.".to_owned(),
                Color::Muted,
            ),
            ApplyCheckState::Valid => (
                "The server accepted this document and returned the object it would store. \
                 Nothing has been written yet."
                    .to_owned(),
                Color::Success,
            ),
            ApplyCheckState::Conflict { owners } => {
                let owners = if owners.is_empty() {
                    "another field manager".to_owned()
                } else {
                    owners.join(", ")
                };
                (
                    format!("A field this change would take is owned by {owners}."),
                    Color::Warning,
                )
            }
            ApplyCheckState::Failed { reason } => (reason.clone(), Color::Muted),
        };
        let busy = matches!(self.apply_check, ApplyCheckState::Running);
        h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .items_start()
            .debug_selector(|| "yaml-review-check-status".to_owned())
            .child(
                Icon::new(if busy {
                    IconName::LoadCircle
                } else {
                    IconName::Info
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(label_small(text).color(color))
            .into_any_element()
    }

    fn render_apply_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending_apply.as_ref()?;
        let identity = object_display_identity(&pending.target.object_ref());
        let colors = cx.theme().colors();
        let baseline = self.original.clone().unwrap_or_default();
        let diff = yaml_diff(&baseline, &pending.yaml);
        let hidden = diff.len().saturating_sub(APPLY_REVIEW_DIFF_LINES);
        let shown = diff
            .iter()
            .take(APPLY_REVIEW_DIFF_LINES)
            .copied()
            .collect::<Vec<_>>();
        let keep_editing_focus = self.review_keep_editing_focus.clone();
        let check_focus = self.review_check_focus.clone();
        let apply_focus = self.review_apply_focus.clone();
        let checking = matches!(self.apply_check, ApplyCheckState::Running);
        // A diff line is data, and the row is as tall as the configured data line: a taller glyph
        // in a shorter box crops, which is the case the "Data font size" help text says cannot
        // happen. The review strip keeps the dense line rather than `design::size::ROW` — it caps
        // itself at a dozen lines to protect the editor — so the default appearance is unchanged
        // and a raised font still grows the row.
        let data = crate::settings::data_typography(cx);
        let row_height = data.line_height;
        let rows: Vec<AnyElement> = shown
            .into_iter()
            .map(|line| {
                let text = format!("{} {}", line.marker(), line.text());
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(row_height)
                    .gap(space::XS)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(data.font.clone())
                            .font_features(data.features.clone())
                            .text_size(rems_from_px(f32::from(data.size)))
                            .line_height(rems_from_px(f32::from(data.line_height)))
                            .text_color(line.severity().marker(cx))
                            .child(SharedString::from(text)),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .id("yaml-apply-review")
                .debug_selector(|| "yaml-apply-review".to_owned())
                .flex_none()
                .w_full()
                .min_w(px(0.))
                .gap(space::SM)
                .py(space::SM)
                .px(space::SM)
                .bg(design::surface::raised(cx).alpha(1.0))
                .border_t_1()
                .border_b_1()
                .border_color(colors.border)
                .role(Role::AlertDialog)
                .aria_label(format!("{APPLY_REVIEW_TITLE} {identity}"))
                .on_key_down({
                    let panel = cx.weak_entity();
                    move |event: &KeyDownEvent, _window: &mut Window, cx: &mut App| {
                        if event.keystroke.key.as_str() != "escape" {
                            return;
                        }
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
                        }
                        cx.stop_propagation();
                    }
                })
                .child(
                    v_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::XS)
                        .child(common::label_panel_title(APPLY_REVIEW_TITLE))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w(px(0.))
                                .gap(space::XS)
                                .items_center()
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(label_body(identity.clone()).color(Color::Muted)),
                                )
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(
                                            Label::new(format!("UID {}", pending.target.uid)).size(
                                                LabelSize::Custom(rems_from_px(f32::from(
                                                    design::text::METADATA,
                                                ))),
                                            ),
                                        ),
                                ),
                        )
                        .child(self.apply_check_line())
                        .child(
                            label_small("This writes to the cluster when you apply.")
                                .color(Color::Muted),
                        ),
                )
                .child(
                    v_flex()
                        .w_full()
                        .min_w(px(0.))
                        .gap(space::XS)
                        .px(space::XS)
                        .py(space::XS)
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(cx.theme().colors().editor_background.alpha(1.0))
                        .id("yaml-apply-review-diff")
                        .role(Role::Region)
                        .aria_label("Local change against the last text the cluster reported")
                        .children(rows)
                        .when(hidden > 0, |this| {
                            this.child(label_small(format!(
                                "and {hidden} more line{}",
                                if hidden == 1 { "" } else { "s" }
                            )).color(Color::Muted))
                        }),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap(space::SM)
                        .justify_end()
                        .child(
                            div()
                                .debug_selector(|| "yaml-review-keep-editing".to_owned())
                                .child(
                                    Button::new("yaml-review-keep-editing", "Keep editing")
                                        .style(ButtonStyle::OutlinedGhost)
                                        .size(ButtonSize::Medium)
                                        .width(px(INSPECTOR_ACTION_BUTTON_WIDTH * 1.5))
                                        .track_focus(&keep_editing_focus)
                                        .tab_index(REVIEW_KEEP_EDITING_TAB_INDEX)
                                        .tooltip(Tooltip::text(
                                            "Close the review without writing anything",
                                        ))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.cancel_pending_apply(cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "yaml-review-check".to_owned())
                                .child(
                                    Button::new(
                                        "yaml-review-check",
                                        if checking { "Checking…" } else { "Check against the cluster" },
                                    )
                                        .style(ButtonStyle::Outlined)
                                        .size(ButtonSize::Medium)
                                        .disabled(checking)
                                        .track_focus(&check_focus)
                                        .tab_index(REVIEW_CHECK_TAB_INDEX)
                                        .tooltip(Tooltip::text(
                                            "Ask the API server to validate this document without storing it",
                                        ))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.check_pending_apply(cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "yaml-review-apply".to_owned())
                                .child(
                                    Button::new("yaml-review-apply", "Apply to cluster")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .size(ButtonSize::Medium)
                                        .width(px(INSPECTOR_ACTION_BUTTON_WIDTH * 1.75))
                                        .track_focus(&apply_focus)
                                        .tab_index(REVIEW_APPLY_TAB_INDEX)
                                        .tooltip(Tooltip::text(format!(
                                            "Send this change to {identity}"
                                        )))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.confirm_pending_apply(cx)
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    // Describe rendering

    fn render_describe(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(selection) = self.selection.clone() else {
            return empty_state(
                IconName::TextSnippet,
                "No resource details to show",
                "Select a row to see its fields, owners, and conditions.",
            );
        };
        let state = self.describe_states.get(&selection.uid).cloned();
        let body: AnyElement = match state {
            Some(LoadState::Ready(data)) => self.describe_body(&data, cx),
            Some(LoadState::Failed(reason)) => load_error(
                LoadErrorParts {
                    title: "Failed to load resource details",
                    hint: load_failure_hint(&reason),
                    retry_label: "Retry loading resource details",
                    reason,
                    retry_focus: self.load_retry_focus.clone(),
                    retry_tab_index: INSPECTOR_DESCRIBE_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.ensure_describe(true, cx)),
            ),
            _ => loading_state(
                cx,
                "Loading resource details",
                "Fetching fields and conditions…",
            ),
        };
        let colors = cx.theme().colors();
        let focus_border = colors.border_focused;
        let rail_ghost = colors.border_transparent;
        v_flex()
            .size_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .child(self.render_inspector_toolbar(cx, "Describe"))
            .child(
                div()
                    .id("describe-scroll")
                    .debug_selector(|| "describe-scroll".to_owned())
                    .role(Role::Region)
                    .aria_label("Resource details. Use the arrow keys to scroll.")
                    .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
                    .track_focus(&self.describe_focus)
                    .tab_index(INSPECTOR_DESCRIBE_SCROLL_TAB_INDEX)
                    // The rail is always reserved, so taking and leaving focus cannot slide the
                    // details sideways.
                    .border_l_2()
                    .border_color(rail_ghost)
                    .focus_visible(move |style| style.border_color(focus_border))
                    .on_key_down(cx.listener(Self::on_describe_key_down))
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.describe_scroll)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_inspector_toolbar(&self, cx: &mut Context<Self>, label: &'static str) -> AnyElement {
        let title = self.selection.as_ref().map_or_else(
            || label.to_owned(),
            |selection| format!("{label} · {}", object_display_identity(selection)),
        );
        let reload_label = format!("Reload {title}");
        let leading = h_flex()
            .id("inspector-context-leading")
            .flex_1()
            .min_w(px(0.))
            .h(design::size::ROW)
            .items_center()
            .overflow_hidden()
            .child(
                div()
                    .min_w(px(0.))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(common::label_panel_title(title).color(Color::Muted)),
            )
            .into_any_element();
        let actions = h_flex()
            .id("inspector-context-actions")
            .flex_none()
            .h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .child(
                div()
                    .debug_selector(|| "inspector-action-reload".to_owned())
                    .child(
                        IconButton::new("inspector-reload", IconName::RotateCcw)
                            .size(ButtonSize::Medium)
                            .width(design::size::CONTROL)
                            .icon_size(IconSize::XSmall)
                            .tooltip(inspector_control_tooltip(
                                reload_label.clone(),
                                control_chord("k8s_inspector::ReloadActiveTab", cx),
                            ))
                            .aria_label(reload_label)
                            .track_focus(&self.reload_focus)
                            .tab_index(INSPECTOR_RELOAD_TAB_INDEX)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.reload_active_tab(cx)),
                            ),
                    )
                    .into_any_element(),
            )
            .into_any_element();
        inspector_toolbar("inspector-context-toolbar", leading, actions, cx)
    }

    fn describe_stacked(&self) -> bool {
        let width = self.describe_width.get();
        !width.is_finite() || width < DESCRIBE_TWO_COLUMN_MIN_WIDTH
    }

    fn describe_body(&mut self, data: &DescribeData, cx: &mut Context<Self>) -> AnyElement {
        let object = &data.object;
        let kind = object
            .types
            .as_ref()
            .map(|types| types.kind.clone())
            .unwrap_or_default();
        let name = object.metadata.name.clone().unwrap_or_default();
        let namespace = object.metadata.namespace.clone().unwrap_or_default();
        let stacked = self.describe_stacked();
        let values = self.value_rows(cx);
        let events_state = self
            .selection
            .as_ref()
            .and_then(|selection| self.events_states.get(&selection.uid))
            .map(|entry| entry.state.clone());

        let mut sections: Vec<AnyElement> = Vec::new();
        sections.push(self.identity_section(&kind, &name, &namespace, data, stacked, &values, cx));

        let labels = label_rows(object);
        if !labels.is_empty() {
            sections.push(self.detail_section(
                "Labels",
                labels,
                LABEL_CHIP_LIMIT,
                DetailSection::Labels,
                false,
                stacked,
                LABELS_EXPAND_TAB_INDEX,
                &values,
                cx,
            ));
        }

        let conditions = condition_rows(object, stacked, &values, cx);
        if !conditions.is_empty() {
            sections.push(section("Conditions", conditions, None, cx));
        }

        if kind == "Pod" {
            let containers = container_rows(object, stacked, &values, cx);
            if !containers.is_empty() {
                sections.push(section("Containers", containers, None, cx));
            }
        }

        let status_rows = describe_status_rows(object, &kind);
        if !status_rows.is_empty() {
            sections.push(self.detail_section(
                "Status",
                status_rows,
                MAX_FIELD_ROWS,
                DetailSection::Status,
                true,
                stacked,
                STATUS_EXPAND_TAB_INDEX,
                &values,
                cx,
            ));
        }

        // The spec stays complete: the Containers block shows what each container is and
        // how it runs, while resources, ports and mounts only exist here.
        let mut spec_rows = Vec::new();
        if let Some(spec) = object.data.get("spec") {
            flatten_scalars(spec, "", &mut spec_rows);
        }
        if !spec_rows.is_empty() {
            sections.push(self.detail_section(
                "Spec",
                spec_rows,
                MAX_FIELD_ROWS,
                DetailSection::Spec,
                true,
                stacked,
                SPEC_EXPAND_TAB_INDEX,
                &values,
                cx,
            ));
        }

        sections.push(self.owners_section(data, stacked, &values, cx));
        sections.push(self.events_section(events_state.as_ref(), cx));

        let measured_width = self.describe_width.clone();
        let panel = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .on_children_prepainted(move |children, window, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let width = f32::from(bounds.size.width);
                if width.is_finite() && width > 0.0 && (measured_width.get() - width).abs() > 0.5 {
                    measured_width.set(width);
                    let panel = panel.clone();
                    window.defer(cx, move |_, cx| {
                        if let Some(panel) = panel.upgrade() {
                            panel.update(cx, |_, cx| cx.notify());
                        }
                    });
                }
            })
            .id("inspector-describe-body")
            .debug_selector(|| "inspector-describe-body".to_owned())
            .w_full()
            .min_w(px(0.))
            .px(space::SM)
            .py(space::SM)
            .gap(space::MD)
            .children(sections)
            .into_any_element()
    }

    #[expect(clippy::too_many_arguments)]
    fn detail_section(
        &mut self,
        title: &'static str,
        rows: Vec<FieldRow>,
        limit: usize,
        detail: DetailSection,
        data_column: bool,
        stacked: bool,
        tab_index: isize,
        values: &ValueRows,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = match detail {
            DetailSection::Labels => self.expanded_details.labels,
            DetailSection::Status => self.expanded_details.status,
            DetailSection::Spec => self.expanded_details.spec,
        };
        let total = rows.len();
        let (visible, hidden) = visible_detail_rows(rows, limit, expanded);
        let rendered = visible
            .into_iter()
            .map(|(key, value, is_string)| {
                let style = if data_column {
                    ValueStyle::data(is_string)
                } else {
                    ValueStyle::prose()
                };
                field_row(&key, &value, style, stacked, values, cx)
            })
            .collect::<Vec<_>>();
        let footer = (hidden > 0).then(|| {
            let label = if expanded {
                "Show Fewer".to_owned()
            } else {
                format!("Show {hidden} More")
            };
            let aria_label = if expanded {
                format!("Collapse {title}, {total} fields")
            } else {
                format!("Show all {title}, {total} fields")
            };
            let toggle_focus = self.section_toggle_focus[detail.index()].clone();
            h_flex()
                .w_full()
                .justify_end()
                .child(
                    Button::new(format!("inspector-detail-toggle-{title}"), label)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .track_focus(&toggle_focus)
                        .tab_index(tab_index)
                        .start_icon(Icon::new(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        }))
                        .toggle_state(expanded)
                        .selected_style(ButtonStyle::Subtle)
                        .aria_expanded(expanded)
                        .aria_label(aria_label)
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            match detail {
                                DetailSection::Labels => {
                                    panel.expanded_details.labels = !panel.expanded_details.labels
                                }
                                DetailSection::Status => {
                                    panel.expanded_details.status = !panel.expanded_details.status
                                }
                                DetailSection::Spec => {
                                    panel.expanded_details.spec = !panel.expanded_details.spec
                                }
                            }
                            cx.notify();
                        })),
                )
                .into_any_element()
        });
        section(title, rendered, footer, cx)
    }

    #[expect(clippy::too_many_arguments)]
    fn identity_section(
        &self,
        kind: &str,
        name: &str,
        namespace: &str,
        data: &DescribeData,
        stacked: bool,
        values: &ValueRows,
        cx: &App,
    ) -> AnyElement {
        let object = &data.object;
        let uid = object.metadata.uid.clone().unwrap_or_default();
        let created = object
            .metadata
            .creation_timestamp
            .as_ref()
            .map_or_else(|| "—".to_owned(), |time| format_age(time.0.as_second()));
        let mut name_view = div()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(design::text::BODY)
            .line_height(design::text::BODY_LINE_HEIGHT)
            .text_color(cx.theme().colors().text)
            .child(SharedString::from(name.to_owned()));
        name_view
            .interactivity()
            .tooltip(Tooltip::text(name.to_owned()));
        let context = if namespace.is_empty() {
            kind.to_owned()
        } else {
            format!("{kind} · {namespace}")
        };
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_center()
                    .child(
                        Icon::new(design::kind_icon(kind))
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(name_view),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_center()
                    .child(
                        div()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(label_small(context).color(Color::Muted)),
                    )
                    .child(
                        label_small(format!("Created {created} ago"))
                            .color(Color::Muted)
                            .flex_none(),
                    ),
            )
            .when(!uid.is_empty(), |this| {
                this.child(field_row(
                    "UID",
                    &uid,
                    ValueStyle::data(true),
                    stacked,
                    values,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn owners_section(
        &self,
        data: &DescribeData,
        stacked: bool,
        values: &ValueRows,
        cx: &App,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = data
            .owners
            .iter()
            .map(|(kind, name)| {
                field_row(
                    kind,
                    &format!("{kind}/{name}"),
                    ValueStyle::prose(),
                    stacked,
                    values,
                    cx,
                )
            })
            .collect();
        if rows.is_empty() {
            rows.push(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .h(design::size::ROW)
                    .items_center()
                    .child(
                        label_small("This resource has no owner references.").color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        section("Owners", rows, None, cx)
    }

    /// The newest events, read from the same cache the Events tab reads.
    ///
    /// Describe used to render the events the describe call happened to return, which is a
    /// different list with no lifetime, so "+N more in the Events tab" could disagree with that
    /// tab. One cache, one list, one count.
    fn events_section(
        &self,
        state: Option<&LoadState<Arc<Vec<DynamicObject>>>>,
        cx: &App,
    ) -> AnyElement {
        let note = |text: &'static str| -> AnyElement {
            h_flex()
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .items_center()
                .child(label_small(text).color(Color::Muted))
                .into_any_element()
        };
        let Some(state) = state else {
            return section(
                "Events",
                vec![note("Waiting for the event list…")],
                None,
                cx,
            );
        };
        let events = match state {
            LoadState::Loading => {
                return section("Events", vec![note("Loading the event list…")], None, cx);
            }
            LoadState::Failed(reason) => {
                return section("Events", vec![note(load_failure_hint(reason))], None, cx);
            }
            LoadState::Ready(events) => events,
        };
        let total = events.len();
        let mut rows: Vec<AnyElement> = events
            .iter()
            .take(5)
            .map(|event| event_summary_row(event, cx))
            .collect();
        if rows.is_empty() {
            rows.push(note(
                "No recent events. Reload the Events tab to check again.",
            ));
        }
        let footer = (total > 5).then(|| {
            let text = format!("+{} more in the Events tab", total - 5);
            let mut view = div()
                .w_full()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(SharedString::from(text.clone()));
            view.interactivity().tooltip(Tooltip::text(text));
            view.into_any_element()
        });
        section("Events", rows, footer, cx)
    }

    // Events rendering

    fn render_events(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(selection) = self.selection.clone() else {
            return empty_state(
                IconName::Bell,
                "No events to show",
                "Select a row to see its events, newest first.",
            );
        };
        let state = self
            .events_states
            .get(&selection.uid)
            .map(|e| e.state.clone());
        let body: AnyElement = match state {
            Some(LoadState::Ready(events)) if events.is_empty() => empty_state(
                IconName::Bell,
                "No events",
                "Reload to check for new events.",
            ),
            Some(LoadState::Ready(events)) => self.events_list(events),
            Some(LoadState::Failed(reason)) => load_error(
                LoadErrorParts {
                    title: "Failed to load events",
                    hint: load_failure_hint(&reason),
                    retry_label: "Retry loading events",
                    reason,
                    retry_focus: self.load_retry_focus.clone(),
                    retry_tab_index: INSPECTOR_DESCRIBE_RETRY_TAB_INDEX,
                },
                cx,
                cx.listener(|this, _, _, cx| this.ensure_events(true, cx)),
            ),
            _ => loading_state(cx, "Loading events", "Waiting for Kubernetes events…"),
        };
        let colors = cx.theme().colors();
        let focus_border = colors.border_focused;
        let rail_ghost = colors.border_transparent;
        v_flex()
            .size_full()
            .min_h(px(0.))
            .child(self.render_inspector_toolbar(cx, "Events"))
            .child(
                div()
                    .id("events-scroll")
                    .debug_selector(|| "events-scroll".to_owned())
                    .role(Role::Region)
                    .aria_label("Events, newest first. Use the arrow keys to scroll.")
                    .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
                    .track_focus(&self.events_focus)
                    .tab_index(INSPECTOR_EVENTS_SCROLL_TAB_INDEX)
                    // The rail is always reserved, so taking and leaving focus cannot slide the
                    // list sideways.
                    .border_l_2()
                    .border_color(rail_ghost)
                    .focus_visible(move |style| style.border_color(focus_border))
                    .on_key_down(cx.listener(Self::on_events_key_down))
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .child(body),
            )
            .into_any_element()
    }

    fn events_list(&self, events: Arc<Vec<DynamicObject>>) -> AnyElement {
        let count = events.len();
        let aria_label = self.selection.as_ref().map_or_else(
            || "Events, newest first".to_owned(),
            |selection| {
                format!(
                    "Events for {}, newest first",
                    object_accessible_identity(selection)
                )
            },
        );
        let list = uniform_list("inspector-events-list", count, move |range, _, cx| {
            range
                .filter_map(|index| events.get(index).map(|event| event_row(event, cx)))
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.events_scroll)
        .debug_selector(|| "inspector-events-list".to_owned())
        .size_full();
        div()
            .id("inspector-events-list")
            .size_full()
            .role(Role::List)
            .aria_label(aria_label)
            .child(list)
            .into_any_element()
    }

    // Metrics rendering

    fn render_metrics_toolbar(&self, target: &MetricsTarget, cx: &Context<Self>) -> AnyElement {
        let title = target.title();
        let full_title = metrics_identity(target);
        // The name, the kind, and the namespace stay in the bar, because a metrics chart without
        // them cannot be traced back to an object.
        let mut title_view = h_flex()
            .flex_1()
            .min_w(px(0.))
            .gap(space::XS)
            .items_center()
            .overflow_hidden()
            .child(
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(common::label_panel_title(title)),
            )
            .child(
                label_small(format!(
                    "{} · {}",
                    target.kind_label(),
                    metrics_namespace(target)
                ))
                .color(Color::Muted)
                .flex_none(),
            );
        title_view
            .interactivity()
            .tooltip(Tooltip::text(full_title.clone()));
        let range = RANGE_OPTIONS
            .iter()
            .enumerate()
            .map(|(index, (millis, label))| {
                let selected = self.metrics_range_ms == *millis;
                let selector = format!("metrics-range-action-{label}");
                let binding = match index {
                    0 => UiKeyBinding::for_action_in(&MetricsRange5m, &self.focus_handle, cx),
                    1 => UiKeyBinding::for_action_in(&MetricsRange15m, &self.focus_handle, cx),
                    _ => UiKeyBinding::for_action_in(&MetricsRange1h, &self.focus_handle, cx),
                };
                let handle = self.range_focus[index].clone();
                div().debug_selector(move || selector.clone()).child(
                    Button::new(("metrics-range", *millis as usize), *label)
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        .width(px(RANGE_BUTTON_WIDTH))
                        .track_focus(&handle)
                        .tab_index(METRICS_RANGE_TAB_INDEX + index as isize)
                        .key_binding(binding)
                        .aria_label(format!("Show the last {label}"))
                        .toggle_state(selected)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                            panel.set_metrics_range(*millis, cx);
                        })),
                )
            })
            .collect::<Vec<_>>();
        let reload_label = format!("Reload metrics for {full_title}");
        let leading = h_flex()
            .id("metrics-context-leading")
            .flex_1()
            .min_w(px(0.))
            .h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .overflow_hidden()
            .child(
                Icon::new(design::kind_icon(target.kind_label()))
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(title_view)
            .when(!self.metrics.window.is_empty(), |this| {
                this.child(
                    label_small(format!("Window {}", self.metrics.window))
                        .color(Color::Muted)
                        .flex_none(),
                )
            })
            .into_any_element();
        let actions = h_flex()
            .id("metrics-context-actions")
            .flex_none()
            .min_w(px(0.))
            .h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.action_scroll)
            .child(
                div()
                    .debug_selector(|| "metrics-action-reload".to_owned())
                    .flex_none()
                    .child(
                        IconButton::new("metrics-reload", IconName::RotateCcw)
                            .size(ButtonSize::Medium)
                            .width(design::size::CONTROL)
                            .icon_size(IconSize::XSmall)
                            .tooltip(inspector_control_tooltip(
                                reload_label.clone(),
                                control_chord("k8s_inspector::RetryMetrics", cx),
                            ))
                            .aria_label(reload_label)
                            .track_focus(&self.reload_focus)
                            .tab_index(INSPECTOR_RELOAD_TAB_INDEX)
                            .on_click(cx.listener(|this, _, _, cx| this.retry_metrics(cx))),
                    ),
            )
            // No text label: the three durations are self-describing, and the
            // label would push the last one out of a 240px Inspector.
            .child(
                h_flex()
                    .id("metrics-range-group")
                    .flex_none()
                    .gap(space::XS)
                    .role(Role::Group)
                    .aria_label("Chart range")
                    .children(range),
            )
            .into_any_element();
        inspector_toolbar("metrics-context-toolbar", leading, actions, cx)
    }

    fn render_metrics(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(target) = self.metrics_target.clone() else {
            return empty_state(
                IconName::SignalHigh,
                "No metrics for this selection",
                "Select a node or pod to see its CPU and memory usage.",
            );
        };
        let colors = cx.theme().colors();
        let focus_border = colors.border_focused;
        let mut body = v_flex()
            .id("metrics-scroll")
            .debug_selector(|| "metrics-scroll".to_owned())
            .role(Role::Region)
            .aria_label("Metrics. Use the arrow keys to scroll.")
            .aria_keyshortcuts("ArrowUp ArrowDown PageUp PageDown Home End")
            .track_focus(&self.metrics_focus)
            .tab_index(INSPECTOR_METRICS_SCROLL_TAB_INDEX)
            // The rail is always reserved, so taking and leaving focus cannot slide the charts
            // sideways.
            .border_l_2()
            .border_color(colors.border_transparent)
            .focus_visible(move |style| style.border_color(focus_border))
            .on_key_down(cx.listener(Self::on_metrics_key_down))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.metrics_scroll)
            .p(space::SM)
            .gap(space::MD);
        match self.metrics_probe.clone() {
            MetricsProbeState::Checking => {
                body = body.child(self.render_metrics_checking(cx));
            }
            MetricsProbeState::Missing => {
                body = body.child(self.render_metrics_failure(
                    "Metrics unavailable",
                    "Install or enable metrics-server, then retry.",
                    METRICS_UNAVAILABLE.to_owned(),
                    None,
                    Severity::Warning,
                    cx,
                ));
            }
            MetricsProbeState::Forbidden { reason } => {
                body = body.child(self.render_metrics_failure(
                    "Not allowed to read metrics",
                    "Grant read access to metrics.k8s.io, then retry.",
                    reason,
                    None,
                    Severity::Warning,
                    cx,
                ));
            }
            MetricsProbeState::Error { reason } => {
                body = body.child(self.render_metrics_failure(
                    "Failed to check metrics",
                    "Retry, or make sure the cluster connection works.",
                    reason,
                    None,
                    Severity::Error,
                    cx,
                ));
            }
            MetricsProbeState::Available => {
                if self.metrics.is_empty() {
                    if let Some(error) = &self.metrics.last_error {
                        body = body.child(self.render_metrics_failure(
                            "Failed to sample metrics",
                            "Retry, or make sure the cluster connection works.",
                            error.clone(),
                            retry_delay_text(&self.metrics_scheduler),
                            Severity::Error,
                            cx,
                        ));
                    } else {
                        body = body.child(self.render_metrics_waiting(cx));
                    }
                } else {
                    if let Some(error) = &self.metrics.last_error {
                        body = body.child(self.render_metrics_status(error.clone(), cx));
                    }
                    body = body
                        .child(self.render_metric_section(
                            "CPU",
                            Unit::Cpu,
                            &self.cpu_chart,
                            "metrics-cpu-table",
                            cx,
                        ))
                        .child(self.render_metric_section(
                            "Memory",
                            Unit::Memory,
                            &self.memory_chart,
                            "metrics-memory-table",
                            cx,
                        ));
                }
            }
        }
        v_flex()
            .size_full()
            .min_h(px(0.))
            .bg(colors.panel_background.alpha(1.0))
            .child(self.render_metrics_toolbar(&target, cx))
            .child(body)
            .into_any_element()
    }

    fn metrics_retry_button(&self, cx: &Context<Self>) -> AnyElement {
        Button::new("metrics-retry", "Retry")
            .style(ButtonStyle::Tinted(TintColor::Accent))
            .size(ButtonSize::Medium)
            .width(px(RETRY_BUTTON_WIDTH))
            .track_focus(&self.metrics_retry_focus)
            .tab_index(METRICS_RETRY_TAB_INDEX)
            .aria_label("Retry metrics")
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.retry_metrics(cx)))
            .into_any_element()
    }

    fn metrics_loading_icon(cx: &App) -> AnyElement {
        waiting_glyph(cx)
    }

    fn render_metrics_checking(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .id("metrics-checking")
            .w_full()
            .py(space::XL)
            .items_center()
            .gap(space::XS)
            .role(Role::Status)
            .aria_label("Checking metrics availability")
            .child(Self::metrics_loading_icon(cx))
            .child(label_text("Checking metrics availability"))
            .child(label_small("Looking for metrics-server…").color(Color::Muted))
            .into_any_element()
    }

    fn render_metrics_waiting(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .id("metrics-waiting")
            .w_full()
            .py(space::XL)
            .items_center()
            .gap(space::XS)
            .role(Role::Status)
            .aria_label("Waiting for the first sample")
            .child(Self::metrics_loading_icon(cx))
            .child(label_text("Waiting for the first sample"))
            .child(
                label_small(sampling_interval_text(self.metrics_scheduler.interval()))
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_metrics_status(&self, reason: String, cx: &Context<Self>) -> AnyElement {
        let mut status = h_flex()
            .id("metrics-sample-status")
            .w_full()
            .gap(space::SM)
            .items_center()
            .role(Role::Alert)
            .aria_label("Metrics sampling failed")
            .aria_description("Retry metrics.");
        status
            .interactivity()
            .tooltip(Tooltip::text(reason.clone()));
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .child(
                status
                    .child(status_message(
                        Severity::Error,
                        "Failed to sample metrics",
                        None,
                        cx,
                    ))
                    .child(self.metrics_retry_button(cx)),
            )
            // The countdown answers "how long do I wait", so it gets its own line and
            // the info colour instead of sharing the hint's weight.
            .when_some(
                retry_delay_text(&self.metrics_scheduler),
                |this, backoff| {
                    this.child(label_small(backoff).color(Color::Custom(Severity::Info.marker(cx))))
                },
            )
            .into_any_element()
    }

    fn render_metrics_failure(
        &self,
        title: &'static str,
        hint: &'static str,
        reason: String,
        backoff: Option<String>,
        severity: Severity,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut state = v_flex()
            .id("metrics-failure")
            .w_full()
            .py(space::XL)
            .items_center()
            .gap(space::SM)
            .role(Role::Alert)
            .aria_label(format!("{title}. {hint}"));
        state.interactivity().tooltip(Tooltip::text(reason));
        state
            .child(
                Icon::new(IconName::Warning)
                    .size(state_icon_size())
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(label_text(title))
            .child(label_small(hint).color(Color::Muted))
            // The countdown answers "how long do I wait": its own line, info colour.
            .when_some(backoff, |this, backoff| {
                this.child(label_small(backoff).color(Color::Custom(Severity::Info.marker(cx))))
            })
            .child(self.metrics_retry_button(cx))
            .into_any_element()
    }

    fn render_metric_section(
        &self,
        title: &'static str,
        unit: Unit,
        chart: &Entity<LineChartView>,
        table_id: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let data = chart.read(cx).data_rc();
        // The current value is the one number on this row, and it is a sample, so it is set in
        // the data role at the size the reader configured. A sample table and a chart caption
        // that disagree about the data font size are two readings of the same measurement.
        let data_typography = crate::settings::data_typography(cx);
        // The series name trails the block name as a metadata label, separated by a
        // middle dot: `CPU · etcd` cannot read as one missing space.
        let series_names = h_flex()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .gap(space::XS)
            .items_center()
            .child(
                div()
                    .flex_none()
                    .child(label_small("·").color(Color::Muted)),
            )
            .children(self.metrics.series.iter().map(|series| {
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(
                        label_small(series.name.clone())
                            .color(Color::Muted)
                            .truncate(),
                    )
            }));
        // The current value is the only number on this row, so it sits on the trailing
        // edge of the plot area instead of floating in the middle. It shrinks before the
        // series names do, so a Pod with many containers cannot push the row wide.
        let current = h_flex()
            .flex_shrink_1()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .children(self.metrics.series.iter().map(|series| {
                let latest = match unit {
                    Unit::Cpu => series.latest_cpu(),
                    _ => series.latest_memory(),
                };
                let text = latest.map_or_else(|| "—".to_owned(), |value| unit.format(value));
                let tooltip = format!("{}: {text}", series.name);
                let mut view = div()
                    .flex_shrink_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(
                        Label::new(text)
                            .size(LabelSize::Custom(rems_from_px(f32::from(
                                data_typography.size,
                            ))))
                            .truncate(),
                    );
                view.interactivity().tooltip(Tooltip::text(tooltip));
                view
            }));
        let latest = h_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .items_center()
            // A CPU or Memory block is a region heading, so it takes the section scale.
            .child(label_section(title).flex_none())
            .child(series_names)
            .child(current);
        v_flex()
            .w_full()
            .min_w(px(0.))
            .gap(space::XS)
            .child(latest)
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(METRICS_CHART_HEIGHT))
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(chart.clone()),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .h(px(METRICS_TABLE_HEIGHT))
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .overflow_hidden()
                    .child(ChartTable::new(table_id, data)),
            )
            .into_any_element()
    }
}

fn tab_scroll_index(index: usize) -> usize {
    index
}

fn tab_focus_target(current: usize, count: usize, key: &str) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let current = current.min(count - 1);
    match key {
        "left" | "up" => Some((current + count - 1) % count),
        "right" | "down" => Some((current + 1) % count),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => None,
    }
}

fn object_display_identity(object: &ObjectRef) -> String {
    let kind = if object.resource.kind.is_empty() {
        "Resource"
    } else {
        object.resource.kind.as_str()
    };
    let name = if object.name.is_empty() {
        object.uid.as_str()
    } else {
        object.name.as_str()
    };
    match object
        .namespace
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(namespace) => format!("{kind} {name} in namespace {namespace}"),
        None => format!("{kind} {name}"),
    }
}

fn object_accessible_identity(object: &ObjectRef) -> String {
    let display = object_display_identity(object);
    if !object.uid.is_empty() {
        format!("{display}. UID {}", object.uid)
    } else {
        display
    }
}

fn yaml_matches_target(target: &ApplyTarget, yaml: &str) -> bool {
    let Ok(object) = serde_yaml_ng::from_str::<DynamicObject>(yaml) else {
        return false;
    };
    if object
        .metadata
        .name
        .as_deref()
        .is_some_and(|name| name != target.name)
    {
        return false;
    }
    match (
        target.namespace.as_deref(),
        object.metadata.namespace.as_deref(),
    ) {
        (Some(expected), Some(actual)) if expected != actual => return false,
        (None, Some(actual)) if !actual.is_empty() => return false,
        _ => {}
    }
    if object.metadata.uid.as_deref() != Some(target.uid.as_str()) {
        return false;
    }
    object.types.as_ref().is_none_or(|types| {
        types.api_version == target.resource.api_version && types.kind == target.resource.kind
    })
}

fn applied_object_matches(target: &ApplyTarget, object: &DynamicObject) -> bool {
    if object.metadata.name.as_deref() != Some(target.name.as_str())
        || object.metadata.uid.as_deref() != Some(target.uid.as_str())
    {
        return false;
    }
    match (
        target.namespace.as_deref(),
        object.metadata.namespace.as_deref(),
    ) {
        (Some(expected), Some(actual)) if expected != actual => return false,
        (Some(_), None) => return false,
        (None, Some(actual)) if !actual.is_empty() => return false,
        _ => {}
    }
    object.types.as_ref().is_none_or(|types| {
        types.api_version == target.resource.api_version && types.kind == target.resource.kind
    })
}

impl Render for InspectorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.yaml_observation.is_none() {
            let editor = self.yaml_view.clone();
            self.yaml_observation = Some(cx.observe(&editor, |panel, editor, cx| {
                if panel.problems_changed(editor.read(cx).diagnostics()) {
                    cx.notify();
                }
            }));
        }
        let active_tab = self.active_tab.min(self.tab_count().saturating_sub(1));
        let content = match active_tab {
            0 => self.render_yaml(cx),
            1 => self.render_describe(cx),
            2 => self.render_events(cx),
            _ => self.render_metrics(cx),
        };
        let colors = cx.theme().colors();
        let focus_border = colors.border_focused;
        let tab_label = self.tab_label(active_tab);
        // The panel is a tab panel, so the label names both the tab and the object it shows.
        let inspector_label = self.selection.as_ref().map_or_else(
            || format!("{tab_label}. Inspector. No resource selected."),
            |selection| {
                format!(
                    "{tab_label}. Inspector for {}",
                    object_accessible_identity(selection)
                )
            },
        );
        v_flex()
            .size_full()
            .min_w(px(0.))
            .bg(colors.panel_background.alpha(1.0))
            .text_color(colors.text)
            .key_context("Inspector")
            .child(self.render_identity(cx))
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("inspector-content")
                    .role(Role::TabPanel)
                    .aria_label(inspector_label)
                    .accessibility_id(format!("inspector-panel-{active_tab}"))
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .track_focus(&self.focus_handle)
                    .tab_index(INSPECTOR_CONTENT_TAB_INDEX)
                    // The rail is always reserved, so taking and leaving focus cannot slide the
                    // tab panel sideways.
                    .border_l_2()
                    .border_color(colors.border_transparent)
                    .focus_visible(move |style| style.border_color(focus_border))
                    .on_action(cx.listener(Self::reload_action))
                    .on_action(cx.listener(Self::metrics_retry_action))
                    .on_action(cx.listener(Self::range_5m))
                    .on_action(cx.listener(Self::range_15m))
                    .on_action(cx.listener(Self::range_1h))
                    .on_action(cx.listener(Self::confirm_action))
                    .on_action(cx.listener(Self::cancel_review_action))
                    .on_action(cx.listener(Self::revert_action))
                    .on_action(cx.listener(Self::copy_yaml_action))
                    .on_action(cx.listener(Self::toggle_value_action))
                    .on_action(cx.listener(Self::copy_value_action))
                    .on_action(cx.listener(Self::next_problem_action))
                    .child(content),
            )
    }
}

/// A toolbar band shared by the YAML, context, and metrics toolbars.
///
/// The three bars were written out three times, so a change to the band reached one of them. The
/// contents stay with their tab; only the frame, the height, and the hairline live here.
fn inspector_toolbar(
    id: &'static str,
    leading: AnyElement,
    actions: AnyElement,
    cx: &Context<InspectorPanel>,
) -> AnyElement {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex_none()
        .w_full()
        .min_w(px(0.))
        .h(design::size::TOOLBAR)
        .px(space::SM)
        .gap(space::SM)
        .items_center()
        .bg(colors.toolbar_background.alpha(1.0))
        .border_b_1()
        .border_color(colors.border)
        .child(leading)
        .child(actions)
        .into_any_element()
}

/// Tooltip for a control that also has a key: the label, then the chord that reaches the same
/// action, so a shortcut is discoverable from the surface that owns it.
///
/// The chord comes from the keymap, so a control with no binding yet shows its label alone rather
/// than advertising a key that does nothing.
fn inspector_control_tooltip(
    label: impl Into<SharedString>,
    chord: Option<String>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    let keystrokes = chord
        .as_deref()
        .and_then(|chord| Keystroke::parse(chord).ok())
        .map(|keystroke| vec![KeybindingKeystroke::from_keystroke(keystroke)]);
    Tooltip::element(move |_, _| {
        let mut tooltip = h_flex().gap(space::SM).items_center();
        tooltip = tooltip.child(div().child(label.clone()));
        if let Some(keystrokes) = keystrokes.clone() {
            tooltip = tooltip.child(
                KeyBinding::from_keystrokes(keystrokes.into(), false).style(KeyBindingStyle::Label),
            );
        }
        tooltip.into_any_element()
    })
}

/// The chord a control advertises, read from the keymap so the hint cannot drift from the key.
fn control_chord(action: &str, cx: &App) -> Option<String> {
    crate::keymap::binding_for_context(action, "Inspector", cx)
}

/// The size an Inspector empty or waiting state leads with.
///
/// It used to be Zed's `IconSize::XLarge`, 48px, while the shared empty state leads with
/// `design::size::ICON_LARGE`, 32px. Two empty states on the same screen at 1.5x the ink is two
/// designs, and `DESIGN.md` §3.3 lists the icon sizes as fixed dimensions.
fn state_icon_size() -> IconSize {
    IconSize::Custom(rems_from_px(f32::from(design::size::ICON_LARGE)))
}

/// A waiting marker, at the size an Inspector state uses.
///
/// The reduce-motion branch lives in `panels::common::spinner` and nowhere else. It sat here as a
/// private copy while twelve other call sites decided for themselves, and only this one read the
/// setting at all.
fn waiting_glyph(cx: &App) -> AnyElement {
    spinner(IconName::LoadCircle, Color::Accent, state_icon_size(), cx)
}

/// A waiting state that respects reduce motion, unlike the shared empty state.
fn loading_state(cx: &App, title: &'static str, hint: &'static str) -> AnyElement {
    v_flex()
        .id("inspector-loading")
        .debug_selector(|| "inspector-loading".to_owned())
        .size_full()
        .min_h(px(0.))
        .min_w(px(0.))
        .items_center()
        .justify_center()
        .gap(space::SM)
        .px(space::XL)
        .role(Role::Status)
        .aria_label(title)
        .aria_description(hint)
        .child(waiting_glyph(cx))
        .child(label_text(title))
        .child(label_small(hint).color(Color::Muted))
        .into_any_element()
}

/// Namespace of a metrics target. A Node is cluster scoped, so it says so instead of showing
/// nothing next to a name.
fn metrics_namespace(target: &MetricsTarget) -> &str {
    match target {
        MetricsTarget::Node { .. } => "cluster scoped",
        MetricsTarget::Pod { namespace, .. } if namespace.is_empty() => "default",
        MetricsTarget::Pod { namespace, .. } => namespace.as_str(),
    }
}

/// Identity of a metrics target in the same words [`object_display_identity`] uses.
fn metrics_identity(target: &MetricsTarget) -> String {
    match target {
        MetricsTarget::Node { name, .. } => format!("Node {name}"),
        MetricsTarget::Pod {
            namespace, name, ..
        } => {
            format!("Pod {name} in namespace {namespace}")
        }
    }
}

/// A local line diff between the text the cluster reported and the text to apply.
///
/// It runs on the two documents, not on the server, so it answers "what did I change" and never
/// "would the server accept this". A document pair too large to compare line by line falls back to
/// a count instead of an expensive table.
fn yaml_diff<'a>(old: &'a str, new: &'a str) -> Vec<DiffLine<'a>> {
    const MAX_CELLS: usize = 250_000;
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len().saturating_mul(new_lines.len()) > MAX_CELLS {
        let mut summary = Vec::new();
        if !old_lines.is_empty() {
            summary.push(DiffLine::Removed("the previous document"));
        }
        if !new_lines.is_empty() {
            summary.push(DiffLine::Added("the edited document"));
        }
        return summary;
    }
    // Longest common subsequence over lines, walked forward into a unified list.
    let rows = old_lines.len() + 1;
    let columns = new_lines.len() + 1;
    let mut table = vec![0u32; rows * columns];
    for row in (0..old_lines.len()).rev() {
        for column in (0..new_lines.len()).rev() {
            table[row * columns + column] = if old_lines[row] == new_lines[column] {
                table[(row + 1) * columns + column + 1] + 1
            } else {
                table[(row + 1) * columns + column].max(table[row * columns + column + 1])
            };
        }
    }
    let mut diff = Vec::new();
    let (mut row, mut column) = (0usize, 0usize);
    while row < old_lines.len() && column < new_lines.len() {
        if old_lines[row] == new_lines[column] {
            diff.push(DiffLine::Context(old_lines[row]));
            row += 1;
            column += 1;
        } else if table[(row + 1) * columns + column] >= table[row * columns + column + 1] {
            diff.push(DiffLine::Removed(old_lines[row]));
            row += 1;
        } else {
            diff.push(DiffLine::Added(new_lines[column]));
            column += 1;
        }
    }
    while row < old_lines.len() {
        diff.push(DiffLine::Removed(old_lines[row]));
        row += 1;
    }
    while column < new_lines.len() {
        diff.push(DiffLine::Added(new_lines[column]));
        column += 1;
    }
    diff
}

/// What a read failure says, and the control that tries again.
///
/// One struct rather than seven arguments: the copy, the reason and the two focus values travel
/// together, and the only thing that differs between Describe, Events and the YAML tab is which
/// sentences are shown and where Retry sits in the tab order.
struct LoadErrorParts {
    title: &'static str,
    hint: &'static str,
    retry_label: &'static str,
    reason: String,
    retry_focus: FocusHandle,
    retry_tab_index: isize,
}

/// The shared read-failure state: a severity, an alert role, the reason, and a way to try again.
///
/// `retry_tab_index` is part of the state because each caller owns its own focus handle, and two
/// controls that answered to the same tab index would be reachable by the same Tab press. The
/// YAML failure is the newest caller: it replaces the whole tab body, so it cannot borrow the
/// Describe or Events handles.
fn load_error(
    parts: LoadErrorParts,
    cx: &App,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let LoadErrorParts {
        title,
        hint,
        retry_label,
        reason,
        retry_focus,
        retry_tab_index,
    } = parts;
    let mut panel = v_flex()
        .id("inspector-load-error")
        .debug_selector(|| "inspector-load-error".to_owned())
        .w_full()
        .items_center()
        .gap(space::SM)
        .px(space::MD)
        .py(space::XL)
        .role(Role::Alert)
        .aria_label(format!("{title}. {hint}"));
    panel.interactivity().tooltip(Tooltip::text(reason));
    panel
        .child(
            Icon::new(IconName::Warning)
                .size(state_icon_size())
                .color(Color::Custom(Severity::Error.marker(cx))),
        )
        .child(label_text(title))
        .child(label_small(hint).color(Color::Muted))
        .child(
            div().debug_selector(|| "inspector-retry".to_owned()).child(
                Button::new("inspector-retry", "Retry")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .size(ButtonSize::Medium)
                    .width(px(RETRY_BUTTON_WIDTH))
                    .track_focus(&retry_focus)
                    .tab_index(retry_tab_index)
                    .aria_label(retry_label)
                    .on_click(on_retry),
            ),
        )
        .into_any_element()
}

fn section(
    title: &'static str,
    rows: Vec<AnyElement>,
    footer: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let section_selector = format!("inspector-describe-section-{title}");
    let title_selector = format!("inspector-describe-section-title-{title}");
    let colors = cx.theme().colors();
    v_flex()
        .debug_selector(move || section_selector.clone())
        .w_full()
        .min_w(px(0.))
        // Rows keep the shared row rhythm; only the title steps away from them.
        .gap(space::XS)
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .mb(space::SM)
                .gap(space::SM)
                .items_center()
                .child(
                    div()
                        .flex_none()
                        .debug_selector(move || title_selector.clone())
                        .child(label_section(title).color(Color::Muted)),
                )
                // The rule is a real structure line, not a hairline that disappears.
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h(design::border::LINE)
                        .bg(colors.border),
                ),
        )
        .children(rows)
        .when_some(footer, |this, footer| this.child(footer))
        .into_any_element()
}

fn field_row(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    field_row_with_icon(
        label,
        value,
        style,
        stacked,
        None,
        format!("inspector-describe-field-{label}"),
        values,
        cx,
    )
}

/// A field row that carries a severity marker, so the marked and unmarked rows share one layout.
#[expect(clippy::too_many_arguments)]
fn status_field_row(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    severity: Severity,
    selector: String,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    field_row_with_icon(
        label,
        value,
        style,
        stacked,
        Some(severity),
        selector,
        values,
        cx,
    )
}

/// One field row, with the keyboard affordances that make a long value reachable.
///
/// A row is a tab stop: Enter expands it to its full wrapped value, Ctrl or Cmd+C copies the
/// whole value, and every other key falls through to the scroll container. Truncation is a
/// display decision, so the text is always one keystroke or one shortcut away.
#[expect(clippy::too_many_arguments)]
fn field_row_with_icon(
    label: &str,
    value: &str,
    style: ValueStyle,
    stacked: bool,
    severity: Option<Severity>,
    selector: String,
    values: &ValueRows,
    cx: &App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let key_selector = format!("{selector}-key");
    let value_selector = format!("{selector}-value");
    let expanded = values.is_expanded(&selector);
    let focus = values.take_focus(&selector, value);
    let selected_bg = colors.element_selected;
    // A value the cluster sent is data, and the reader can set the data font size. The metadata
    // role beside it is not scaled by that setting, so both sizes have to come from their own
    // source or a raised data font leaves two sizes inside one row. The row is as tall as the
    // configured data line, for the reason the setting's own help text gives: a taller glyph in
    // a shorter box is cropped.
    let data_typography = style.data.then(|| crate::settings::data_typography(cx));
    let value_size = data_typography
        .as_ref()
        .map_or(design::text::METADATA, |data| data.size);
    let value_line_height = data_typography
        .as_ref()
        .map_or(design::text::METADATA_LINE_HEIGHT, |data| data.line_height);
    let row_height = data_typography.as_ref().map_or_else(
        || design::size::ROW,
        |data| design::row_height(data.line_height),
    );
    // The slot is always present, so a marked row and an unmarked row share one key
    // column and one value start.
    let marker = h_flex()
        .w(DESCRIBE_MARKER_SLOT)
        .flex_none()
        .h(row_height)
        .items_center()
        .when_some(severity, |this, severity| {
            this.child(describe_severity_icon(severity, cx))
        })
        // An expanded row says it can be collapsed, in the slot that never moves anything.
        .when(expanded && severity.is_none(), |this| {
            this.child(
                Icon::new(IconName::ChevronUp)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
        });
    let mut key = div()
        .h(row_height)
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(design::text::METADATA)
        .line_height(design::text::METADATA_LINE_HEIGHT)
        .text_color(colors.text_muted)
        .debug_selector({
            let key_selector = key_selector.clone();
            move || key_selector.clone()
        })
        .child(SharedString::from(label.to_owned()));
    key.interactivity().tooltip(Tooltip::text(label.to_owned()));
    let value_text = SharedString::from(value.to_owned());
    let mut inline_value = div()
        .h(row_height)
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(rems_from_px(f32::from(value_size)))
        .line_height(rems_from_px(f32::from(value_line_height)))
        .text_color(colors.text)
        .debug_selector({
            let value_selector = value_selector.clone();
            move || value_selector.clone()
        })
        .child(value_text.clone());
    if style.mono {
        inline_value = inline_value.font(buffer_font(cx));
    }
    inline_value
        .interactivity()
        .tooltip(Tooltip::text(value.to_owned()));
    // A long value shares one line with its key badly, so it moves under the key and
    // wraps inside the readable measure. Nothing is dropped, and the tooltip still holds
    // the full text when a wrapped value has to clip.
    let mut wrapped_value = div()
        .w_full()
        .min_w(px(0.))
        .min_h(row_height)
        .pl(DESCRIBE_MARKER_SLOT + space::XS)
        .overflow_hidden()
        .whitespace_normal()
        .text_size(rems_from_px(f32::from(value_size)))
        .line_height(rems_from_px(f32::from(value_line_height)))
        .text_color(colors.text)
        .debug_selector({
            let value_selector = value_selector.clone();
            move || value_selector.clone()
        })
        .child(value_text);
    if style.mono {
        wrapped_value = wrapped_value.font(buffer_font(cx));
    }
    wrapped_value
        .interactivity()
        .tooltip(Tooltip::text(value.to_owned()));

    let row_selector = selector.clone();
    // An expanded row always takes the wrapped layout, because the point of expanding is to see
    // the whole value.
    let wrap = stacked || expanded || value.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT;
    let layout: Div = if wrap {
        let key_line = h_flex()
            .w_full()
            .min_w(px(0.))
            .h(row_height)
            .gap(space::XS)
            .items_center()
            .child(marker)
            .child(div().flex_1().min_w(px(0.)).child(key));
        v_flex()
            .debug_selector(move || row_selector.clone())
            .w_full()
            .min_w(px(0.))
            .children([key_line, wrapped_value])
    } else {
        h_flex()
            .debug_selector(move || row_selector.clone())
            .w_full()
            .min_w(px(0.))
            .h(row_height)
            .gap(space::XS)
            .items_center()
            .child(marker)
            .child(
                div()
                    .w(px(DESCRIBE_KEY_WIDTH))
                    .flex_none()
                    .min_w(px(0.))
                    .child(key),
            )
            .child(inline_value)
    };
    let mut row = layout
        .id((SharedString::from(selector.clone()), 0))
        .role(Role::Group)
        .aria_label(format!("{label}: {value}"))
        .aria_expanded(expanded)
        .aria_keyshortcuts("Enter Control+c Meta+c")
        .when(expanded, |this| this.bg(colors.element_hover));
    if let Some(focus) = focus {
        let panel = values.panel.clone();
        let row_selector = selector.clone();
        let copied = values.copied(&selector);
        // The two chords this row answers to, revealed while the row is hovered or focused.
        //
        // A Describe view holds hundreds of rows, so painting the chords on every row would
        // turn the field list into a wall of keycaps and bury the values, which are the reason
        // the panel exists. Revealing them on approach teaches the shortcut to whoever is using
        // a pointer or a keyboard, and leaves the resting surface quiet for everyone else. The
        // chords themselves come from the keymap, so a rebinding shows up here for free.
        let group: SharedString = format!("{selector}-row").into();
        let chords = h_flex()
            .flex_none()
            .gap(space::XS)
            .items_center()
            .opacity(0.)
            .group_hover(group.clone(), |style| style.opacity(1.))
            .debug_selector({
                let selector = format!("{selector}-chords");
                move || selector.clone()
            })
            .child(UiKeyBinding::for_action_in(
                &ToggleValueExpansion,
                &focus,
                cx,
            ))
            .child(UiKeyBinding::for_action_in(&CopyValue, &focus, cx));
        row = row
            .group(group)
            .track_focus(&focus)
            .tab_stop(true)
            .focus_visible(move |style| style.bg(selected_bg))
            .child(chords)
            .when(copied, |this| this.bg(colors.element_hover))
            .on_key_down(
                move |event: &KeyDownEvent, _window: &mut Window, cx: &mut App| {
                    let key = event.keystroke.key.as_str();
                    let modifiers = event.keystroke.modifiers;
                    let Some(panel) = panel.upgrade() else {
                        return;
                    };
                    if matches!(key, "enter" | "return" | "space") {
                        panel.update(cx, |panel, cx| {
                            panel.toggle_value_expansion(&row_selector, cx)
                        });
                    } else if key == "c" && (modifiers.control || modifiers.platform) {
                        panel.update(cx, |panel, cx| panel.copy_value(&row_selector, cx));
                    } else {
                        return;
                    }
                    cx.stop_propagation();
                },
            );
    }
    row.into_any_element()
}

fn describe_severity_icon(severity: Severity, cx: &App) -> AnyElement {
    // The marker sits on the panel background, so it is solved against that surface and not
    // against the canvas the default marker colour assumes.
    //
    // It is `design::size::STATUS_MARKER`, not the 12px it used to be. The slot around it is the
    // same token, so a marked row and an unmarked one still share one key column and one value
    // start - the glyph got bigger without the layout moving.
    Icon::new(design::severity_icon(severity))
        .size(IconSize::Custom(rems_from_px(f32::from(
            design::size::STATUS_MARKER,
        ))))
        .color(Color::Custom(
            severity.marker_on(cx, design::surface::panel(cx).alpha(1.0)),
        ))
        .into_any_element()
}

/// Height of the problems list at the cap: the rows a reader sees before scrolling.
fn problems_list_max_height() -> gpui::Pixels {
    design::size::ROW * PROBLEMS_VISIBLE_ROWS as f32
}

fn visible_detail_rows(
    mut rows: Vec<FieldRow>,
    limit: usize,
    expanded: bool,
) -> (Vec<FieldRow>, usize) {
    let total = rows.len();
    let hidden = total.saturating_sub(limit);
    if !expanded {
        rows.truncate(limit);
    }
    (rows, hidden)
}

fn label_rows(object: &DynamicObject) -> Vec<FieldRow> {
    let mut rows: Vec<FieldRow> = object
        .metadata
        .labels
        .as_ref()
        .into_iter()
        .flatten()
        // A label value is a string, so the type flag is true and the font decision follows it.
        .map(|(key, value)| (key.clone(), value.clone(), true))
        .collect();
    rows.sort();
    rows
}

/// Flattens every scalar under a prefix, tagging each row with the JSON type of its value.
///
/// The type is what decides the font, so a column cannot change size halfway down and an address
/// reads as code without a string-matching heuristic.
fn flatten_scalars(value: &Value, prefix: &str, out: &mut Vec<FieldRow>) {
    match value {
        Value::Object(map) => {
            if map.is_empty() && !prefix.is_empty() {
                out.push((prefix.to_owned(), "{}".to_owned(), false));
                return;
            }
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_scalars(child, &path, out);
            }
        }
        Value::Array(items) => {
            if items.is_empty() && !prefix.is_empty() {
                out.push((prefix.to_owned(), "[]".to_owned(), false));
                return;
            }
            for (index, child) in items.iter().enumerate() {
                flatten_scalars(child, &format!("{prefix}[{index}]"), out);
            }
        }
        _ => {
            if let Some(text) = scalar_text(value)
                && !prefix.is_empty()
            {
                out.push((prefix.to_owned(), text, matches!(value, Value::String(_))));
            }
        }
    }
}

/// Flattens the fields of a subtree, dropping the keys `skip` names.
///
/// A Describe block that already reads a subtree must not repeat it under raw JSON
/// paths, so the caller lists the keys it handles itself.
fn flatten_scalars_below(value: &Value, skip: &[&str], out: &mut Vec<FieldRow>) {
    let Value::Object(map) = value else {
        flatten_scalars(value, "", out);
        return;
    };
    for (key, child) in map {
        if skip.contains(&key.as_str()) {
            continue;
        }
        flatten_scalars(child, key, out);
    }
}

/// Pod status fields the Status block names itself, and the subtrees the Conditions and
/// Containers blocks already read.
const POD_STATUS_HANDLED: [&str; 7] = [
    "phase",
    "reason",
    "message",
    "conditions",
    "containerStatuses",
    "initContainerStatuses",
    "ephemeralContainerStatuses",
];

/// Status rows for a known kind: the phase with the reason and message behind it, then
/// every remaining status field. A Pod gets named rows so an operator reads "Pending,
/// Unschedulable" instead of `conditions[0].reason`.
fn describe_status_rows(object: &DynamicObject, kind: &str) -> Vec<FieldRow> {
    let Some(status) = object.data.get("status") else {
        return Vec::new();
    };
    let handled: &[&str] = if kind == "Pod" {
        &POD_STATUS_HANDLED
    } else {
        &[]
    };
    let mut rows = Vec::new();
    if kind == "Pod" {
        for (label, path) in [
            ("Phase", "phase"),
            ("Reason", "reason"),
            ("Message", "message"),
        ] {
            if let Some(value) = status.pointer(path)
                && let Some(text) = scalar_text(value)
            {
                rows.push((label.to_owned(), text, matches!(value, Value::String(_))));
            }
        }
    }
    flatten_scalars_below(status, handled, &mut rows);
    rows
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Null => Some("null".to_owned()),
        _ => None,
    }
}

/// Whether a condition type is healthy when its status is `True` or `False`.
///
/// Most conditions are healthy when `True` (`Ready`, `Available`, `Initialized`).
/// Failure and pressure conditions are the opposite: `Failed=False` and
/// `MemoryPressure=False` mean the workload is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionPolarity {
    Healthy,
    Unhealthy,
}

fn condition_polarity(condition_type: &str) -> ConditionPolarity {
    match condition_type {
        "Failed" | "OOMKilled" | "Evicted" | "MemoryPressure" | "DiskPressure" | "PIDPressure"
        | "NetworkUnavailable" | "KernelDeadlock" => ConditionPolarity::Unhealthy,
        _ => ConditionPolarity::Healthy,
    }
}

fn condition_rows(
    object: &DynamicObject,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    let Some(conditions) = object
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    conditions
        .iter()
        .flat_map(|condition| {
            let kind = condition
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("Condition")
                .to_owned();
            let status = condition
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_owned();
            let reason = condition
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let message = condition
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let severity =
                match (status.as_str(), condition_polarity(&kind)) {
                    ("True", ConditionPolarity::Healthy)
                    | ("False", ConditionPolarity::Unhealthy) => Severity::Success,
                    ("True", ConditionPolarity::Unhealthy)
                    | ("False", ConditionPolarity::Healthy) => Severity::Warning,
                    _ if status.eq_ignore_ascii_case("critical") => Severity::Error,
                    _ => Severity::Muted,
                };
            let summary = if reason.is_empty() {
                status
            } else {
                format!("{status} · {reason}")
            };
            let mut rows = vec![status_field_row(
                &kind,
                &summary,
                ValueStyle::prose(),
                stacked,
                severity,
                format!("inspector-describe-condition-{kind}"),
                values,
                cx,
            )];
            if !message.is_empty() {
                rows.push(field_row_with_icon(
                    "Message",
                    &message,
                    ValueStyle::prose(),
                    stacked,
                    None,
                    format!("inspector-describe-condition-message-{kind}"),
                    values,
                    cx,
                ));
            }
            rows
        })
        .collect()
}

/// One container group of a Pod: app containers, init containers, and ephemeral debuggers.
struct ContainerGroup {
    label: &'static str,
    spec_path: &'static str,
    status_path: &'static str,
    /// Init containers must finish before the app containers start.
    must_succeed: bool,
}

const CONTAINER_GROUPS: [ContainerGroup; 3] = [
    ContainerGroup {
        label: "Containers",
        spec_path: "/spec/containers",
        status_path: "/status/containerStatuses",
        must_succeed: false,
    },
    ContainerGroup {
        label: "Init Containers",
        spec_path: "/spec/initContainers",
        status_path: "/status/initContainerStatuses",
        must_succeed: true,
    },
    ContainerGroup {
        label: "Ephemeral Containers",
        spec_path: "/spec/ephemeralContainers",
        status_path: "/status/ephemeralContainerStatuses",
        must_succeed: false,
    },
];

fn container_rows(
    object: &DynamicObject,
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    for group in CONTAINER_GROUPS {
        let containers = object
            .data
            .pointer(group.spec_path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if containers.is_empty() {
            continue;
        }
        let statuses = object
            .data
            .pointer(group.status_path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut section_rows = container_group_rows(
            group.must_succeed,
            &containers,
            &statuses,
            stacked,
            values,
            cx,
        );
        if section_rows.is_empty() {
            continue;
        }
        if rows.is_empty() && group.label == "Containers" {
            rows.append(&mut section_rows);
            continue;
        }
        rows.push(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .h(design::size::ROW)
                .pl(DESCRIBE_MARKER_SLOT)
                .items_center()
                .child(label_body(group.label).color(Color::Muted))
                .into_any_element(),
        );
        rows.append(&mut section_rows);
    }
    rows
}

fn container_group_rows(
    must_succeed: bool,
    containers: &[Value],
    statuses: &[Value],
    stacked: bool,
    values: &ValueRows,
    cx: &App,
) -> Vec<AnyElement> {
    containers
        .iter()
        .flat_map(|container| {
            let name = container
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("container")
                .to_owned();
            let image = container
                .get("image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let status = statuses
                .iter()
                .find(|status| status.get("name").and_then(Value::as_str) == Some(name.as_str()));
            let ready = status
                .and_then(|status| status.get("ready"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let restarts = status
                .and_then(|status| status.get("restartCount"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let state = status
                .and_then(|status| status.get("state"))
                .map(container_state)
                .unwrap_or_default();
            let succeeded = status
                .and_then(|status| status.pointer("/state/terminated/exitCode"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code == 0);
            // A container that terminated on a failure will not come back on its own, so it is
            // the one state that is an error rather than a caution.
            let failed = status
                .and_then(|status| status.pointer("/state/terminated/exitCode"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
            // An init container is expected to finish, not to stay ready. Only its exit
            // code decides whether the Pod can start.
            let severity = if must_succeed {
                match status {
                    Some(_) if succeeded => Severity::Success,
                    Some(_) => Severity::Warning,
                    None => Severity::Muted,
                }
            } else if failed {
                Severity::Error
            } else if !ready {
                // The row reads "Not ready", so the marker cannot stay neutral. A container the
                // API has not reported a state for yet is still not ready, and colour, icon,
                // text and shape have to agree on the state.
                Severity::Warning
            } else if restarts > 0 {
                // A container that came back is serving, but the restarts are the reason to
                // look at this row.
                Severity::Warning
            } else {
                Severity::Success
            };
            let readiness = if must_succeed {
                if succeeded { "Completed" } else { "Incomplete" }
            } else if ready {
                "Ready"
            } else {
                "Not ready"
            };
            let summary = if state.is_empty() {
                format!("{readiness} · Restart Count: {restarts}")
            } else {
                format!("{state} · {readiness} · Restart Count: {restarts}")
            };
            let selector = format!("inspector-describe-container-{name}");
            let mut rows = vec![status_field_row(
                &name,
                &summary,
                ValueStyle::prose(),
                stacked,
                severity,
                if must_succeed {
                    format!("{selector}-init")
                } else {
                    selector
                },
                values,
                cx,
            )];
            if !image.is_empty() {
                rows.push(field_row_with_icon(
                    "Image",
                    &image,
                    // An image reference is a string, so it takes the buffer font.
                    ValueStyle::data(true),
                    stacked,
                    None,
                    format!("inspector-container-image-{name}"),
                    values,
                    cx,
                ));
            }
            rows
        })
        .collect()
}

fn container_state(state: &Value) -> String {
    for key in ["running", "waiting", "terminated"] {
        if let Some(inner) = state.get(key) {
            let reason = inner.get("reason").and_then(Value::as_str);
            return match reason {
                Some(reason) => format!("{key} ({reason})"),
                None => key.to_owned(),
            };
        }
    }
    String::new()
}

fn events_newest_first(events: Vec<DynamicObject>) -> Vec<DynamicObject> {
    let mut dated = Vec::with_capacity(events.len());
    let mut undated = Vec::new();
    for event in events {
        match event_timestamp(&event) {
            Some(timestamp) => dated.push((timestamp, event)),
            None => undated.push(event),
        }
    }
    dated.sort_by(|(left, _), (right, _)| right.cmp(left));
    dated
        .into_iter()
        .map(|(_, event)| event)
        .chain(undated)
        .collect()
}

/// Sorts events and rejects a payload for a different object.
///
/// The request resolves by name, so an object recreated under that name answers with a
/// new UID. That is a reloadable failure, not content to show.
fn prepare_describe_data(data: DescribeData, expected_uid: &str) -> Result<DescribeData, String> {
    if !expected_uid.is_empty() && data.object.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(OBJECT_REPLACED_REASON.to_owned());
    }
    Ok(DescribeData {
        events: events_newest_first(data.events),
        ..data
    })
}

fn event_timestamp(event: &DynamicObject) -> Option<i128> {
    [
        "/lastTimestamp",
        "/eventTime",
        "/series/lastObservedTime",
        "/firstTimestamp",
    ]
    .iter()
    .find_map(|pointer| {
        event
            .data
            .pointer(pointer)
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<jiff::Timestamp>().ok())
            .map(|time| time.as_nanosecond())
    })
    .or_else(|| {
        event
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|created| created.0.as_nanosecond())
    })
}

fn event_seconds(event: &DynamicObject) -> Option<i64> {
    event_timestamp(event)
        .map(|timestamp| timestamp.div_euclid(1_000_000_000))
        .and_then(|seconds| i64::try_from(seconds).ok())
}

fn event_type(event: &DynamicObject) -> String {
    event
        .data
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Normal")
        .to_owned()
}

fn event_reason(event: &DynamicObject) -> String {
    event
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn event_object(event: &DynamicObject) -> String {
    let involved = event.data.get("involvedObject");
    let kind = involved
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = involved
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}/{name}"),
        (true, false) => name.to_owned(),
        _ => String::new(),
    }
}

fn event_message(event: &DynamicObject) -> String {
    event
        .data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn event_summary_row(event: &DynamicObject, cx: &App) -> AnyElement {
    let kind = event_type(event);
    let severity = if kind == "Warning" {
        Severity::Warning
    } else {
        Severity::Muted
    };
    let reason = event_reason(event);
    let age = event_age(event);
    h_flex()
        .w_full()
        .min_w(px(0.))
        .h(design::size::ROW)
        .gap(space::XS)
        .items_center()
        .child(
            Icon::new(design::severity_icon(severity))
                .size(IconSize::XSmall)
                .color(Color::Custom(severity.marker(cx))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label_small(reason).truncate()),
        )
        .child(label_small(age).color(Color::Muted).flex_none())
        .into_any_element()
}

fn event_row(event: &DynamicObject, cx: &App) -> AnyElement {
    let kind = event_type(event);
    let severity = if kind == "Warning" {
        Severity::Warning
    } else {
        Severity::Muted
    };
    let object = event_object(event);
    let message = event_message(event);
    let reason = event_reason(event);
    let age = event_age(event);
    let summary = if reason.is_empty() {
        kind.clone()
    } else {
        format!("{kind} · {reason}")
    };
    let aria_subject = if object.is_empty() {
        message.clone()
    } else if message.is_empty() {
        object.clone()
    } else {
        format!("{object}. {message}")
    };
    let aria_label = format!("{summary}, {age}. {aria_subject}");
    let mut object_view = div()
        .min_w(px(0.))
        .h(design::text::METADATA_LINE_HEIGHT)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(label_small(object.clone()).color(Color::Muted).truncate());
    object_view
        .interactivity()
        .tooltip(Tooltip::text(object.clone()));
    let mut message_view = div()
        .min_w(px(0.))
        .h(design::text::METADATA_LINE_HEIGHT)
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(label_small(message.clone()).color(Color::Muted).truncate());
    message_view
        .interactivity()
        .tooltip(Tooltip::text(message.clone()));
    h_flex()
        .id(("event-row", event_identity(event)))
        .role(Role::ListItem)
        .aria_label(aria_label)
        .w_full()
        .min_w(px(0.))
        .h(design::size::ROW * 2.)
        .px(space::SM)
        .gap(space::SM)
        .overflow_hidden()
        .items_start()
        .child(
            Icon::new(design::severity_icon(severity))
                .size(IconSize::XSmall)
                .color(Color::Custom(severity.marker(cx))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .overflow_hidden()
                .child(
                    h_flex()
                        .w_full()
                        .h(design::text::METADATA_LINE_HEIGHT)
                        .min_w(px(0.))
                        .gap(space::XS)
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(label_small(summary).truncate()),
                        )
                        .child(label_small(age).color(Color::Muted).flex_none()),
                )
                .when(!object.is_empty(), |this| this.child(object_view))
                .when(!message.is_empty(), |this| this.child(message_view)),
        )
        .into_any_element()
}

fn event_identity(event: &DynamicObject) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event_timestamp(event).hash(&mut hasher);
    event.metadata.uid.hash(&mut hasher);
    event.metadata.name.hash(&mut hasher);
    event_reason(event).hash(&mut hasher);
    event_message(event).hash(&mut hasher);
    hasher.finish()
}

fn event_age(event: &DynamicObject) -> String {
    event_seconds(event).map_or_else(|| "—".to_owned(), format_age)
}

fn format_age(seconds: i64) -> String {
    let now = jiff::Timestamp::now().as_second();
    let age = now.saturating_sub(seconds).max(0);
    if age >= 86_400 {
        format!("{}d", age / 86_400)
    } else if age >= 3_600 {
        format!("{}h", age / 3_600)
    } else if age >= 60 {
        format!("{}m", age / 60)
    } else {
        format!("{age}s")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{Entity, TestAppContext};
    use k8s_core::cluster_data::{ClusterDataPort, DataFuture};
    use k8s_core::metrics::{MetricsError, NodeMetric, PodMetric};
    use k8s_core::overview::Overview;
    use serde_json::json;
    use theme::LoadThemes;

    use super::*;
    use crate::panels::inspector_data::{DescribeData, InspectorSource, ObjectRef};

    fn init_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
    }

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        yaml: &str,
    ) -> (Entity<InspectorPanel>, &'a mut gpui::VisualTestContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        panel.update(cx, |panel, cx| {
            panel.set_yaml(Some(yaml.to_owned()), cx);
        });
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel
                    .yaml_view
                    .update(cx, |view, cx| view.focus(window, cx));
            });
        });
        (panel, cx)
    }

    fn assert_toolbar_actions(
        cx: &mut gpui::VisualTestContext,
        toolbar_selector: &'static str,
        action_selectors: &[&'static str],
    ) {
        let toolbar = cx
            .debug_bounds(toolbar_selector)
            .unwrap_or_else(|| panic!("{toolbar_selector}"));
        assert_eq!(
            f32::from(toolbar.size.height),
            f32::from(design::size::TOOLBAR)
        );
        let toolbar_left = f32::from(toolbar.origin.x);
        let toolbar_right = f32::from(toolbar.origin.x + toolbar.size.width);
        let toolbar_top = f32::from(toolbar.origin.y);
        let toolbar_bottom = f32::from(toolbar.origin.y + toolbar.size.height);
        for selector in action_selectors {
            let action = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            let action_left = f32::from(action.origin.x);
            let action_right = f32::from(action.origin.x + action.size.width);
            let action_top = f32::from(action.origin.y);
            let action_bottom = f32::from(action.origin.y + action.size.height);
            assert!(
                action_left >= toolbar_left
                    && action_right <= toolbar_right
                    && action_top >= toolbar_top
                    && action_bottom <= toolbar_bottom,
                "{selector} is outside {toolbar_selector}"
            );
            assert!(
                ((action_top - toolbar_top) - (toolbar_bottom - action_bottom)).abs() <= 1.0,
                "{selector} is not vertically centered in {toolbar_selector}"
            );
        }
    }

    fn yaml_text(panel: &Entity<InspectorPanel>, cx: &mut gpui::VisualTestContext) -> String {
        cx.update(|_, cx| panel.read(cx).yaml_view.read(cx).text().unwrap_or_default())
    }

    fn dirty(panel: &Entity<InspectorPanel>, cx: &mut gpui::VisualTestContext) -> bool {
        cx.update(|_, cx| panel.read(cx).is_dirty(cx))
    }

    /// How far a tab-order walk goes before it gives up. The tab order wraps, so one pass over
    /// the panel reaches everything the value focus pool and the scroll regions register.
    const TAB_ORDER_WALK_LIMIT: usize = VALUE_FOCUS_POOL_SIZE + 64;

    /// Typing pause before the YAML editor parses the document, mirroring
    /// `yaml_editor::VALIDATE_DEBOUNCE`, which is private to that module.
    ///
    /// A copy of a private constant is a test that goes quiet instead of going red: raise the
    /// real debounce and the test still waits on this number, so the coverage is gone and
    /// nothing says so. `yaml_editor` has to publish the constant for this to be read instead of
    /// mirrored, and until it does, the test that uses it brackets the value from both sides -
    /// the editor is silent one millisecond before this number and has reported by it - so a
    /// change to the real debounce fails here loudly.
    const VALIDATE_DEBOUNCE_TEST: Duration = Duration::from_millis(300);

    /// Walks the rendered tab order and reports whether the keyboard reaches `target`.
    ///
    /// `Window::focus` accepts any handle, mounted or not, so reaching a handle proves less than
    /// Tab does. This is the check that a scroll region or a value row is a real tab stop.
    fn reaches_in_tab_order(
        cx: &mut gpui::VisualTestContext,
        entry: &FocusHandle,
        target: &FocusHandle,
    ) -> bool {
        cx.update(|window, cx| window.focus(entry, cx));
        for _ in 0..TAB_ORDER_WALK_LIMIT {
            cx.update(|window, cx| window.focus_next(cx));
            if cx.update(|window, _| target.is_focused(window)) {
                return true;
            }
        }
        false
    }

    fn pod_object(name: &str, uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": uid,
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                    "labels": { "app": "web", "tier": "frontend" },
                },
                "spec": { "containers": [{ "name": "app", "image": "web:1" }] },
                "status": {
                    "phase": "Running",
                    "podIP": "10.244.0.5",
                    "conditions": [{
                        "type": "Ready", "status": "True", "reason": "ContainersReady",
                    }],
                    "containerStatuses": [{
                        "name": "app", "ready": true, "restartCount": 2,
                        "state": { "running": {} },
                    }],
                },
            }))
            .expect("pod"),
        )
    }

    /// UID shared by the layout fixture object and the selection that loads it.
    const LAYOUT_UID: &str = "layout-uid";

    /// A scheduler message past the inline limit, so it must wrap under its key.
    const LAYOUT_LONG_MESSAGE: &str = "CRITICAL: readiness dependency has been unavailable for a very long time and must not wrap";
    /// An image digest past the inline limit.
    const LAYOUT_LONG_IMAGE: &str = "registry.example.com/platform/api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Wide values for the layout assertions. The UID must match the selection that
    /// asks for it, otherwise the load is rejected as a replaced object.
    fn layout_pod_object(uid: &str) -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web-frontend-7d9f8c6b5d4-abcde",
                    "namespace": "production-platform",
                    "uid": uid,
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                    "labels": {
                        "app": "web",
                        "company.example.com/team-platform/extremely-long-label-name": "a very long label value that must remain on one line"
                    }
                },
                "spec": {
                    "containers": [{
                        "name": "api",
                        "image": LAYOUT_LONG_IMAGE
                    }]
                },
                "status": {
                    "phase": "Running",
                    "configHash": "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                    "conditions": [{
                        "type": "Ready",
                        "status": "CRITICAL",
                        "reason": "DependencyUnavailable",
                        "message": LAYOUT_LONG_MESSAGE
                    }],
                    "containerStatuses": [{
                        "name": "api",
                        "ready": false,
                        "restartCount": 17,
                        "state": { "waiting": { "reason": "CrashLoopBackOff" } }
                    }]
                }
            }))
            .expect("layout pod"),
        )
    }

    fn pod_ref(uid: &str) -> ObjectRef {
        ObjectRef {
            resource: kube_core::ApiResource::from_gvk_with_plural(
                &kube_core::GroupVersionKind::gvk("", "v1", "Pod"),
                "pods",
            ),
            namespace: Some("default".to_owned()),
            name: "web-0".to_owned(),
            uid: uid.to_owned(),
        }
    }

    fn matching_apply_yaml(uid: &str) -> String {
        matching_apply_yaml_for(uid, "web-0")
    }

    /// YAML that identifies the object it claims to describe.
    fn matching_apply_yaml_for(uid: &str, name: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {name}\n  namespace: default\n  uid: {uid}\n"
        )
    }

    struct FakeSource {
        describes: AtomicUsize,
        events: AtomicUsize,
        object: Option<Arc<DynamicObject>>,
        event_count: usize,
    }

    impl InspectorSource for FakeSource {
        fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData> {
            self.describes.fetch_add(1, Ordering::Relaxed);
            let name = object.name.clone();
            let uid = object.uid.clone();
            let object = self.object.clone();
            Box::pin(async move {
                Ok(DescribeData {
                    object: object.unwrap_or_else(|| pod_object(&name, &uid)),
                    events: Vec::new(),
                    owners: vec![("ReplicaSet".to_owned(), "web-rs".to_owned())],
                })
            })
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            self.events.fetch_add(1, Ordering::Relaxed);
            let events = (0..self.event_count)
                .map(|index| {
                    serde_json::from_value(json!({
                        "apiVersion": "v1",
                        "kind": "Event",
                        "metadata": { "name": format!("event-{index}") },
                        "type": "Normal",
                        "reason": "Synthetic",
                        "message": format!("Event {index}"),
                        "lastTimestamp": "2026-09-23T09:00:00Z"
                    }))
                    .expect("event")
                })
                .collect();
            Box::pin(async move { Ok(events) })
        }
    }

    /// Source that never answers, so a `Loading` entry can be observed.
    struct HangingSource {
        describes: AtomicUsize,
        events: AtomicUsize,
        /// Events fail instead of hanging, because events are best effort.
        events_fail: bool,
    }

    impl HangingSource {
        fn new(events_fail: bool) -> Self {
            Self {
                describes: AtomicUsize::new(0),
                events: AtomicUsize::new(0),
                events_fail,
            }
        }
    }

    impl InspectorSource for HangingSource {
        fn describe(&self, _object: &ObjectRef) -> OpsFuture<DescribeData> {
            self.describes.fetch_add(1, Ordering::Relaxed);
            Box::pin(std::future::pending())
        }

        fn events(&self, _object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>> {
            self.events.fetch_add(1, Ordering::Relaxed);
            if self.events_fail {
                return Box::pin(async { Err("events are forbidden by the cluster".to_owned()) });
            }
            Box::pin(std::future::pending())
        }
    }

    struct EmptyMetricsPort;
    impl ClusterDataPort for EmptyMetricsPort {
        fn overview(&self, _metrics: bool) -> DataFuture<Overview, String> {
            Box::pin(async { Ok(Overview::default()) })
        }

        fn metrics_probe(&self) -> DataFuture<(), MetricsError> {
            Box::pin(async { Ok(()) })
        }

        fn metrics_nodes(&self) -> DataFuture<Vec<NodeMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn metrics_pods(
            &self,
            _namespace: Option<String>,
        ) -> DataFuture<Vec<PodMetric>, MetricsError> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn namespaces(&self) -> DataFuture<Vec<String>, String> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn cluster_uid(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn server_version(&self) -> DataFuture<String, String> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[test]
    fn detail_rows_keep_every_field_available_after_expansion() {
        let rows = (0..20)
            .map(|index| (format!("key{index}"), format!("value{index}"), true))
            .collect::<Vec<_>>();
        let (collapsed, hidden) = visible_detail_rows(rows.clone(), 8, false);
        assert_eq!(collapsed.len(), 8);
        assert_eq!(hidden, 12);
        let (expanded, hidden) = visible_detail_rows(rows, 8, true);
        assert_eq!(expanded.len(), 20);
        assert_eq!(hidden, 12);
    }

    #[test]
    fn scalar_flattening_keeps_deep_and_array_fields() {
        let value = json!({
            "deep": { "nested": { "items": [{ "name": "api" }, null] } },
            "emptyObject": {},
            "emptyArray": []
        });
        let mut rows = Vec::new();
        flatten_scalars(&value, "", &mut rows);
        assert!(rows.contains(&(
            "deep.nested.items[0].name".to_owned(),
            "api".to_owned(),
            true
        )));
        assert!(rows.contains(&("deep.nested.items[1]".to_owned(), "null".to_owned(), false)));
        assert!(rows.contains(&("emptyObject".to_owned(), "{}".to_owned(), false)));
        assert!(rows.contains(&("emptyArray".to_owned(), "[]".to_owned(), false)));
    }

    // The monospace decision follows the value's JSON type, so a column cannot change size
    // halfway down the way a string-matching heuristic made it do.
    #[test]
    fn value_type_drives_the_font_and_not_the_text_of_the_value() {
        let value = json!({
            "host": "registry.k8s.io",
            "podIP": "10.244.0.5",
            "restarts": 3,
            "ready": true,
        });
        let mut rows = Vec::new();
        flatten_scalars(&value, "", &mut rows);
        let style_of = |key: &str| {
            let (_, _, is_string) = rows
                .iter()
                .find(|(path, _, _)| path == key)
                .unwrap_or_else(|| panic!("{key}"));
            ValueStyle::data(*is_string)
        };
        for key in ["host", "podIP"] {
            let style = style_of(key);
            assert!(style.data, "{key} sits in a data column");
            assert!(style.mono, "{key} is a string, so it takes the buffer font");
        }
        for key in ["restarts", "ready"] {
            let style = style_of(key);
            assert!(style.data, "{key} sits in a data column");
            assert!(
                !style.mono,
                "{key} is not a string, so it keeps the UI font"
            );
        }
    }

    // The review compares the two documents locally. A cluster-side dry run would need a request
    // this panel does not own, so the copy says what the comparison is.
    #[test]
    fn a_review_diff_names_every_change_and_falls_back_when_too_large() {
        let diff = yaml_diff("a: 1\nb: 2\n", "a: 1\nb: 3\nc: 4\n");
        assert_eq!(
            diff,
            vec![
                DiffLine::Context("a: 1"),
                DiffLine::Removed("b: 2"),
                DiffLine::Added("b: 3"),
                DiffLine::Added("c: 4"),
            ]
        );
        let large = "x\n".repeat(1_000);
        let large_edited = format!("{large}y\n");
        let summary = yaml_diff(&large, &large_edited);
        assert!(
            summary.len() <= APPLY_REVIEW_DIFF_LINES + 2,
            "a huge change is summarised, not diffed line by line: {}",
            summary.len()
        );
        assert!(summary.contains(&DiffLine::Added("the edited document")));
    }

    #[test]
    fn object_identity_is_explicit_in_visible_and_accessible_text() {
        let object = pod_ref("uid-1");
        assert_eq!(
            object_display_identity(&object),
            "Pod web-0 in namespace default"
        );
        assert_eq!(
            object_accessible_identity(&object),
            "Pod web-0 in namespace default. UID uid-1"
        );
    }

    #[test]
    fn tab_focus_target_moves_horizontally_and_to_edges() {
        assert_eq!(tab_scroll_index(0), 0);
        assert_eq!(tab_scroll_index(3), 3);
        assert_eq!(tab_focus_target(0, 4, "left"), Some(3));
        assert_eq!(tab_focus_target(0, 4, "right"), Some(1));
        assert_eq!(tab_focus_target(2, 4, "home"), Some(0));
        assert_eq!(tab_focus_target(2, 4, "end"), Some(3));
        assert_eq!(tab_focus_target(2, 0, "right"), None);
    }

    #[gpui::test]
    fn inspector_tabs_begin_at_the_content_edge(cx: &mut TestAppContext) {
        let (_panel, cx) = setup(cx, "name: app");
        cx.run_until_parked();
        let strip = cx.debug_bounds("inspector-tabs").expect("Inspector tabs");
        let first = cx.debug_bounds("inspector-tab-0").expect("YAML tab");
        assert!((f32::from(first.origin.x - strip.origin.x)).abs() <= 8.0);
    }

    /// A fetch that failed and an empty selection are different facts. One is an anomaly with a
    /// severity, a reason, and a way to try again; the other is an instruction. The YAML tab
    /// rendered both as `Select a row to inspect its YAML.`, which during an incident reads as
    /// "you have not clicked anything yet" and sends the reader back to the table.
    #[gpui::test]
    fn a_failed_yaml_fetch_is_not_an_empty_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(
                Some(pod_ref("uid-1")),
                "pods is forbidden: User cannot get resource pods".to_owned(),
                cx,
            );
        });
        cx.run_until_parked();

        let error = cx
            .debug_bounds("inspector-load-error")
            .expect("the YAML failure is reported as a failure");
        assert!(f32::from(error.size.height) > 0.0);
        assert!(
            cx.debug_bounds("inspector-retry").is_some(),
            "the failure offers a Retry, not an instruction to select a row"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "the empty selection state stays reserved for an empty selection"
        );
        let yaml_error = panel.read_with(cx, |panel, _| panel.yaml_error().map(str::to_owned));
        assert_eq!(
            yaml_error.as_deref(),
            Some("pods is forbidden: User cannot get resource pods")
        );
    }

    /// The two states are exclusive: a document that arrives resolves the last failure, so a stale
    /// reason cannot sit over a document that is on screen.
    #[gpui::test]
    fn a_document_that_arrives_clears_the_previous_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        assert!(panel.read_with(cx, |panel, _| panel.yaml_error().is_some()));
        panel.update(cx, |panel, cx| {
            panel.set_yaml(Some("name: app".to_owned()), cx)
        });
        assert!(
            panel.read_with(cx, |panel, _| panel.yaml_error().is_none()),
            "a document resolves the failure that preceded it"
        );
    }

    /// Retry has to leave the panel, because the Inspector does not read YAML itself: the table
    /// hands it the text it already read. A Retry that only cleared the error would put the reader
    /// back where they started, which is the failure this state exists to fix.
    #[gpui::test]
    fn retry_asks_for_the_document_again(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        let asked = Rc::new(RefCell::new(0usize));
        let sink = Rc::clone(&asked);
        panel.update(cx, |panel, _| {
            panel.set_yaml_reload(move |_cx| {
                *sink.borrow_mut() += 1;
            });
        });
        panel.update(cx, |panel, cx| {
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        cx.run_until_parked();
        let retry = cx
            .debug_bounds("inspector-retry")
            .expect("the failure offers a Retry");
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(asked.borrow().clone(), 1, "Retry re-requests the document");
        assert!(
            panel.read_with(cx, |panel, _| panel.yaml_error().is_none()),
            "the error clears either way, so Retry is never a no-op"
        );
    }

    /// The Reload control on the YAML toolbar used to do nothing: `reload_active_tab` had no arm
    /// for tab 0, because there was nothing to report.
    #[gpui::test]
    fn reloading_the_yaml_tab_asks_for_the_document(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        let asked = Rc::new(RefCell::new(0usize));
        let sink = Rc::clone(&asked);
        panel.update(cx, |panel, cx| {
            panel.set_yaml_reload(move |_cx| {
                *sink.borrow_mut() += 1;
            });
            panel.set_yaml_error(None, "the watch closed".to_owned(), cx);
        });
        panel.update(cx, |panel, cx| panel.reload_active_tab(cx));
        assert_eq!(asked.borrow().clone(), 1);
    }

    /// The YAML band names its document, not the object. At the 336px default width the old
    /// `YAML · {kind} {name} in namespace {ns}` title kept about 27 of its 53 characters, and the
    /// identity bar forty pixels above already said the same thing at full contrast.
    #[gpui::test]
    fn the_yaml_band_does_not_repeat_the_object_identity(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        // A source that can apply, because the band under test is the clean one: with no handler
        // the toolbar spends its left half on the "Apply unavailable" status instead.
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(|_request, _| {});
        });
        select(&panel, "uid-1", cx);
        cx.simulate_resize(gpui::size(px(336.), px(640.)));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("yaml-clean-metadata").is_some(),
            "the clean band still shows the document glyph"
        );
        assert!(
            cx.debug_bounds("inspector-identity").is_some(),
            "the identity bar is where the object is named"
        );
        let band = cx
            .debug_bounds("yaml-clean-metadata")
            .expect("the YAML band");
        // The band's own width cannot carry this: its slot is a block box, so the band is as wide
        // as the slot whether it holds one glyph or a clipped sentence. Its height can, because
        // every line of text the app prints is at least `metadata` tall and the glyph is not.
        assert!(
            f32::from(band.size.height) < f32::from(design::text::METADATA_LINE_HEIGHT),
            "the band holds the document glyph, not a line of the object identity: {band:?}"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.yaml_error().map(str::to_owned)),
            None,
            "a clean load has no failure to report"
        );
    }

    #[gpui::test]
    fn narrow_inspector_bounds_reveal_roving_focus(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        cx.simulate_resize(gpui::size(px(120.), px(640.)));
        cx.run_until_parked();

        let focus = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        assert_eq!(panel.read_with(cx, |panel, _| panel.focused_tab), 1);
        let viewport = panel.read_with(cx, |panel, _| panel.tabs_scroll.bounds());
        let describe = cx.debug_bounds("inspector-tab-1").expect("Describe tab");
        assert!(describe.left() >= viewport.left());
        assert!(describe.right() <= viewport.right());

        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        assert_eq!(panel.read_with(cx, |panel, _| panel.focused_tab), 2);
        let viewport = panel.read_with(cx, |panel, _| panel.tabs_scroll.bounds());
        let events = cx.debug_bounds("inspector-tab-2").expect("Events tab");
        assert!(events.left() >= viewport.left());
        assert!(events.right() <= viewport.right());
    }

    #[gpui::test]
    fn tab_arrows_roving_focus_and_enter_activates(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        let focus = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("right");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.active_tab(), InspectorTab::Yaml);
            assert_eq!(panel.focused_tab, 1);
        });
        assert!(cx.update(|window, _| focus.is_focused(window)));
        cx.simulate_keystrokes("enter");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            InspectorTab::Describe
        );
    }

    #[gpui::test]
    fn yaml_tab_is_editable_by_default(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        cx.simulate_input("x");
        assert_eq!(yaml_text(&panel, cx), "xname: app");
        assert!(dirty(&panel, cx));
    }

    #[gpui::test]
    fn yaml_toolbar_is_fixed_and_cancel_replaces_edit(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        cx.run_until_parked();
        assert_toolbar_actions(
            cx,
            "yaml-action-toolbar",
            &["yaml-action-apply", "yaml-action-copy"],
        );
        assert!(cx.debug_bounds("yaml-action-cancel").is_none());
        assert!(cx.debug_bounds("yaml-action-edit").is_none());

        cx.simulate_input("x");
        cx.run_until_parked();
        assert!(cx.debug_bounds("yaml-action-cancel").is_some());
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui::test]
    fn inspector_context_toolbars_are_fixed_and_actions_stay_inside(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
        });

        let cases: &[(InspectorTab, &'static str, &'static [&'static str])] = &[
            (
                InspectorTab::Describe,
                "inspector-context-toolbar",
                &["inspector-action-reload"],
            ),
            (
                InspectorTab::Events,
                "inspector-context-toolbar",
                &["inspector-action-reload"],
            ),
            (
                InspectorTab::Metrics,
                "metrics-context-toolbar",
                &[
                    "metrics-range-action-5m",
                    "metrics-range-action-15m",
                    "metrics-range-action-1h",
                ],
            ),
        ];
        for &(tab, toolbar_selector, action_selectors) in cases {
            panel.update(cx, |panel, cx| panel.show_tab(tab, cx));
            cx.run_until_parked();
            assert_toolbar_actions(cx, toolbar_selector, action_selectors);
            let tabs = cx.debug_bounds("inspector-tabs").expect("Inspector tabs");
            assert_eq!(
                f32::from(tabs.size.height),
                f32::from(design::size::TAB_BAR)
            );
        }
    }

    #[gpui::test]
    fn apply_reports_invalid_yaml_and_keeps_dirty(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("{oops");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_some()));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx));
    }

    fn inline_diagnostics(
        panel: &Entity<InspectorPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<Diagnostic> {
        cx.update(|_, cx| panel.read(cx).yaml_view.read(cx).diagnostics().to_vec())
    }

    #[gpui::test]
    fn invalid_apply_sets_inline_diagnostics_and_edit_or_success_clears(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("@oops");
        panel.update(cx, |panel, cx| panel.apply(cx));
        let diagnostics = inline_diagnostics(&panel, cx);
        assert_eq!(
            diagnostics.len(),
            1,
            "Validation failure must show an inline error"
        );
        assert_eq!(
            diagnostics[0].line, 0,
            "The @ symbol is on line 1: {diagnostics:?}"
        );
        assert!(
            diagnostics[0].message.contains("Line 1, column 1"),
            "{diagnostics:?}"
        );

        cx.simulate_input("x");
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "Editing clears diagnostics"
        );

        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "Apply clears diagnostics"
        );
    }

    #[gpui::test]
    fn apply_calls_callback_and_marks_saved(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let applied: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = applied.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply_request(move |request: ApplyRequest| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            applied.borrow().is_none(),
            "Apply must not write before the review is confirmed"
        );
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(
            applied
                .borrow()
                .as_ref()
                .map(|request| request.yaml.as_str()),
            Some(yaml.as_str())
        );
        assert_eq!(
            applied
                .borrow()
                .as_ref()
                .map(|request| request.target.uid.as_str()),
            Some("uid-1")
        );
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_none()));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui::test]
    fn clean_apply_does_not_call_a_write_handler(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply(move |_request| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.applied_at.is_none()));
    }

    #[gpui::test]
    fn missing_apply_handler_never_marks_yaml_saved(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(dirty(&panel, cx));
        panel.read_with(cx, |panel, _| {
            assert!(!panel.applying);
            assert_eq!(panel.apply_error.as_deref(), Some(APPLY_UNAVAILABLE_REASON));
            assert!(panel.applied_at.is_none());
        });
    }

    #[gpui::test]
    fn editing_clears_stale_apply_feedback(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, _| {
            panel.validation_error = Some("stale validation".to_owned());
            panel.apply_error = Some("stale apply".to_owned());
            panel.conflict_owners = Some(vec!["kubectl".to_owned()]);
            panel.applied_at = Some(Instant::now());
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.validation_error.is_none());
            assert!(panel.apply_error.is_none());
            assert!(panel.conflict_owners.is_none());
            assert!(panel.applied_at.is_none());
        });
    }

    #[gpui::test]
    fn async_apply_waits_for_result_and_keeps_content_on_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        assert_eq!(request.yaml, yaml.as_str());
        assert_eq!(request.target.uid, "uid-1");
        assert!(panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(!panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        cx.simulate_input("x");
        assert_eq!(yaml_text(&panel, cx), yaml.as_str());
        assert!(
            dirty(&panel, cx),
            "Keep the YAML dirty until the result arrives"
        );
        cx.run_until_parked();
        assert!(cx.debug_bounds("yaml-action-copy").is_some());

        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request.clone(),
                Err("Kubernetes request failed".to_owned()),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
        assert!(panel.read_with(cx, |panel, _| !panel.applying));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx), "A failed apply keeps the edited content");
        assert_eq!(yaml_text(&panel, cx), yaml.as_str());

        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let conflict_request = requests.borrow()[1].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                conflict_request,
                Ok(ApplyOutcome::Conflict {
                    owners: vec!["kubectl".to_owned(), "helm".to_owned()],
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.conflict_owners.as_deref(),
                Some(["kubectl".to_owned(), "helm".to_owned()].as_slice())
            );
        });
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(dirty(&panel, cx), "A conflict keeps the edited content");

        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let success_request = requests.borrow()[2].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                success_request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[test]
    fn apply_error_classifier_keeps_validation_definite() {
        assert!(InspectorPanel::apply_error_is_unknown("connection reset"));
        assert!(InspectorPanel::apply_error_is_unknown("request timed out"));
        assert!(!InspectorPanel::apply_error_is_unknown(
            "Invalid value: connection"
        ));
        assert!(!InspectorPanel::apply_error_is_unknown("Conflict"));
    }

    #[gpui::test]
    fn apply_unknown_result_asks_for_refresh_without_claiming_failure(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Unknown {
                    reason: "connection reset".to_owned(),
                }),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| {
            panel.apply_error.as_deref() == Some(APPLY_UNKNOWN_REASON)
        }));
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
        assert!(panel.read_with(cx, |panel, _| panel.applied_at.is_none()));
    }

    #[gpui::test]
    fn apply_transport_error_uses_unknown_result_copy(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow()[0].clone();
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Err("Apply request failed: HyperError: connection reset".to_owned()),
                cx,
            );
        });
        assert!(panel.read_with(cx, |panel, _| {
            panel.apply_error.as_deref() == Some(APPLY_UNKNOWN_REASON)
        }));
    }

    #[gpui::test]
    fn ctrl_enter_opens_the_review_and_the_confirmation_writes(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_on_apply(move |_request| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        cx.simulate_keystrokes("ctrl-enter");
        cx.run_until_parked();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "Ctrl+Enter must not write before the review is confirmed"
        );
        assert!(
            dirty(&panel, cx),
            "An unconfirmed change stays dirty, so the way back is still visible"
        );
        assert!(
            cx.debug_bounds("yaml-apply-review").is_some(),
            "Ctrl+Enter opens the review"
        );
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!dirty(&panel, cx), "A confirmed apply marks the YAML saved");
        assert!(panel.read_with(cx, |panel, _| panel.validation_error.is_none()));
    }

    #[gpui::test]
    fn dirty_selection_switch_is_deferred_until_cancel(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel.set_yaml(Some("b: 3".to_owned()), cx);
        });
        assert_eq!(
            yaml_text(&panel, cx),
            "a: 2",
            "A new selection must not replace dirty YAML"
        );
        assert!(panel.read_with(cx, |panel, _| panel.has_pending()));
        panel.update(cx, |panel, cx| panel.revert(cx));
        assert_eq!(
            yaml_text(&panel, cx),
            "b: 3",
            "Cancel loads the pending selection"
        );
        assert!(!dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, cx| panel.is_editing(cx)));
    }

    #[gpui::test]
    fn dirty_selection_keeps_the_original_target_until_discard(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-1"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.pending_selection().unwrap().object.uid),
            "uid-2"
        );
        panel.update(cx, |panel, cx| panel.discard_changes(cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selection().unwrap().uid.clone()),
            "uid-2"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        assert!(!dirty(&panel, cx));
    }

    #[gpui::test]
    fn apply_captures_the_exact_target_and_ignores_a_stale_completion(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        let cluster_id = k8s_core::cluster::ClusterId::derive("ctx", "https://cluster.example");
        panel.update(cx, |panel, _| panel.set_cluster_id(Some(cluster_id)));
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow().clone().expect("request");
        assert_eq!(request.yaml, yaml);
        assert_eq!(request.target.uid, "uid-1");
        assert_eq!(request.target.object_ref().uid, "uid-1");
        assert_eq!(request.target.name, "web-0");
        assert_eq!(request.target.resource.kind, "Pod");
        assert!(request.target.is_complete());
        assert_eq!(request.target.cluster_id(), Some(cluster_id));

        let mut wrong_request = request.clone();
        wrong_request.id += 1;
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(wrong_request, Err("stale".to_owned()), cx);
        });
        assert!(panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(dirty(&panel, cx));

        panel.update(cx, |panel, cx| panel.reset(cx));
        select(&panel, "uid-2", cx);
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
    }

    #[gpui::test]
    fn session_change_invalidates_an_in_flight_target(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Option<ApplyRequest>>> = Rc::new(RefCell::new(None));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                *sink.borrow_mut() = Some(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = matching_apply_yaml("uid-1");
        cx.simulate_input(&yaml);
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        let request = requests.borrow().clone().expect("request");
        panel.update(cx, |panel, _| panel.set_session_epoch(99));
        assert!(panel.read_with(cx, |panel, _| panel.apply_target().is_none()));
        panel.update(cx, |panel, cx| {
            panel.apply_finished_for(
                request,
                Ok(ApplyOutcome::Applied(pod_object("web-0", "uid-1"))),
                cx,
            );
        });
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
        assert!(dirty(&panel, cx));
    }

    #[gpui::test]
    fn apply_rejects_yaml_without_an_exact_target(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
        assert!(!panel.read_with(cx, |panel, _| panel.is_applying()));
    }

    #[gpui::test]
    fn apply_requires_selected_uid_before_dispatch(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        for yaml in [
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-0\n  namespace: default\n",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web-0\n  namespace: default\n  uid: uid-2\n",
        ] {
            cx.simulate_keystrokes("ctrl-a");
            cx.simulate_input(yaml);
            panel.update(cx, |panel, cx| panel.apply(cx));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(dirty(&panel, cx));
            assert!(panel.read_with(cx, |panel, _| {
                !panel.applying && panel.current_apply_request().is_none()
            }));
        }
        assert!(panel.read_with(cx, |panel, _| {
            panel
                .apply_error
                .as_deref()
                .is_some_and(|error| error.contains("UID"))
        }));
    }

    #[gpui::test]
    fn apply_rejects_yaml_for_a_different_object(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let calls = Rc::new(AtomicUsize::new(0));
        let sink = calls.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |_request: ApplyRequest, _| {
                sink.fetch_add(1, Ordering::Relaxed);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("metadata:\n  name: other\n  uid: uid-1");
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(dirty(&panel, cx));
        assert!(panel.read_with(cx, |panel, _| panel.apply_error.is_some()));
    }

    #[gpui::test]
    fn reset_discards_dirty_content_and_pending_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-2"),
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        panel.update(cx, |panel, cx| panel.reset(cx));
        assert!(!panel.read_with(cx, |panel, _| panel.has_yaml()));
        assert!(!panel.read_with(cx, |panel, _| panel.has_pending()));
        assert!(panel.read_with(cx, |panel, _| panel.selection().is_none()));
        assert!(panel.read_with(cx, |panel, _| panel.apply_target().is_none()));
        assert!(!dirty(&panel, cx));
    }

    fn source_setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: None,
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    fn layout_source_setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: Some(layout_pod_object(LAYOUT_UID)),
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    fn events_source_setup(
        cx: &mut TestAppContext,
        event_count: usize,
    ) -> (
        Entity<InspectorPanel>,
        Arc<FakeSource>,
        &mut gpui::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: None,
            event_count,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    /// Panel with a source that never answers Describe.
    fn hanging_setup(
        cx: &mut TestAppContext,
        events_fail: bool,
    ) -> (
        Entity<InspectorPanel>,
        Arc<HangingSource>,
        &mut gpui::VisualTestContext,
    ) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(HangingSource::new(events_fail));
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        (panel, source, cx)
    }

    fn select(panel: &Entity<InspectorPanel>, uid: &str, cx: &mut gpui::VisualTestContext) {
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref(uid),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn describe_loads_lazily_and_is_cached_by_uid(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        assert_eq!(
            source.describes.load(Ordering::Relaxed),
            0,
            "YAML tab does not load Describe"
        );

        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });

        // Return to the cached tab without another request.
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1, "Cache by UID");

        // A new selection misses the cache and loads again.
        select(&panel, "uid-2", cx);
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 2);
    }

    #[gpui::test]
    fn describe_rows_use_measured_width_and_keep_long_values_bounded(cx: &mut TestAppContext) {
        // The marker slot is reserved on every row so a marked row and an unmarked one share one
        // key column and one value start. The glyph in it grew to `design::size::STATUS_MARKER`,
        // so the slot has to have grown with it or the two would not be the same shape.
        assert_eq!(
            DESCRIBE_MARKER_SLOT,
            design::size::STATUS_MARKER,
            "the marker slot and the marker are one size: a glyph wider than its slot is a \
             different key column for a marked row"
        );
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        // The fixture must keep its long values long, or this test stops covering the
        // wrapping branch.
        assert!(LAYOUT_LONG_MESSAGE.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT);
        assert!(LAYOUT_LONG_IMAGE.chars().count() > DESCRIBE_INLINE_VALUE_LIMIT);

        for width in [240.0, 288.0, 336.0, 480.0] {
            cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(640.0)));
            cx.run_until_parked();
            cx.run_until_parked();

            let body = cx
                .debug_bounds("inspector-describe-body")
                .expect("Describe body");
            assert!(
                f32::from(body.size.width) <= f32::from(design::size::INSPECTOR_MAX),
                "the Describe body tracks the Inspector, which DESIGN.md §3.3 caps at \
                 design::size::INSPECTOR_MAX"
            );
            // The last flag marks a value longer than the inline limit, which must wrap
            // under its key instead of running along one line.
            let selectors = [
                (
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name",
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name-key",
                    "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name-value",
                    false,
                ),
                (
                    "inspector-describe-field-configHash",
                    "inspector-describe-field-configHash-key",
                    "inspector-describe-field-configHash-value",
                    false,
                ),
                (
                    "inspector-describe-condition-Ready",
                    "inspector-describe-condition-Ready-key",
                    "inspector-describe-condition-Ready-value",
                    false,
                ),
                (
                    "inspector-describe-condition-message-Ready",
                    "inspector-describe-condition-message-Ready-key",
                    "inspector-describe-condition-message-Ready-value",
                    true,
                ),
                (
                    "inspector-container-image-api",
                    "inspector-container-image-api-key",
                    "inspector-container-image-api-value",
                    true,
                ),
            ];
            // The panel decides the branch from the width it measured, so the
            // expectation follows the same state instead of guessing from the window.
            let two_column = panel.read_with(cx, |panel, _| !panel.describe_stacked());
            for (selector, key_selector, value_selector, long_value) in selectors {
                let row = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector}"));
                let key = cx
                    .debug_bounds(key_selector)
                    .unwrap_or_else(|| panic!("{key_selector}"));
                let value = cx
                    .debug_bounds(value_selector)
                    .unwrap_or_else(|| panic!("{value_selector}"));
                assert!(row.origin.x >= body.origin.x);
                assert!(
                    f32::from(row.origin.x + row.size.width)
                        <= f32::from(body.origin.x + body.size.width) + 1.0
                );
                assert!(f32::from(key.size.width) > 0.0);
                assert!(f32::from(value.size.width) > 0.0);
                assert!(
                    f32::from(value.origin.x + value.size.width)
                        <= f32::from(row.origin.x + row.size.width) + 1.0
                );
                assert!(f32::from(key.size.height) <= f32::from(design::size::ROW));
                if two_column && !long_value {
                    assert_eq!(f32::from(row.size.height), f32::from(design::size::ROW));
                    assert!(f32::from(key.size.width) >= 120.0);
                    // The marker sits in a fixed slot, so a marked row and an unmarked
                    // row start their value at the same edge.
                    let marked = cx
                        .debug_bounds("inspector-describe-condition-Ready-value")
                        .expect("marked value");
                    let value_start = f32::from(value.origin.x);
                    assert!((f32::from(marked.origin.x) - value_start).abs() <= 0.5);
                } else {
                    // Key above, value below and wrapping: two lines or more.
                    assert!(f32::from(row.size.height) >= f32::from(design::size::ROW) * 2.);
                    assert!(f32::from(value.size.height) >= f32::from(design::size::ROW));
                    assert!(f32::from(key.origin.y) < f32::from(value.origin.y));
                }
            }
            let labels = cx
                .debug_bounds("inspector-describe-section-Labels")
                .expect("Labels section");
            let conditions = cx
                .debug_bounds("inspector-describe-section-Conditions")
                .expect("Conditions section");
            let gap =
                f32::from(conditions.origin.y) - f32::from(labels.origin.y + labels.size.height);
            assert!(
                (gap - f32::from(space::MD)).abs() <= 1.0,
                "section gap: {gap}"
            );
            // The block title is a section role, not another metadata label.
            let title = cx
                .debug_bounds("inspector-describe-section-title-Conditions")
                .expect("Conditions title");
            assert!(
                f32::from(title.size.height) > f32::from(design::text::METADATA_LINE_HEIGHT),
                "block title must outrank a field key"
            );
        }
    }

    /// The two-column layout is a promise about width, and the promise is arithmetic.
    ///
    /// `DESIGN.md` §3.2 lets a component keep a size that is not a token, but only if it says
    /// why that size is independent. The key column and the value column are one decision: a
    /// wider key beside the same minimum would promise a two-column layout its own numbers say
    /// does not fit, so the minimum is derived from the two rather than written out beside them.
    #[test]
    fn the_two_column_minimum_is_the_two_columns_it_protects() {
        assert_eq!(
            DESCRIBE_TWO_COLUMN_MIN_WIDTH,
            DESCRIBE_KEY_WIDTH + DESCRIBE_VALUE_MIN_WIDTH,
            "the minimum is the pair, so raising either column moves it"
        );
        assert_eq!(
            DESCRIBE_TWO_COLUMN_MIN_WIDTH, 384.0,
            "deriving the minimum must not move the width the layout starts at"
        );
        assert!(
            DESCRIBE_TWO_COLUMN_MIN_WIDTH <= f32::from(design::size::INSPECTOR_MAX),
            "the two-column layout has to exist at all, or the minimum describes nothing"
        );
        assert!(
            DESCRIBE_TWO_COLUMN_MIN_WIDTH > f32::from(design::size::INSPECTOR_MIN),
            "below the narrowest Inspector every row is stacked, so the minimum is never consulted"
        );
    }

    /// A Describe value the cluster sent follows the data font, and its row grows with it.
    ///
    /// The value is set in the data role, so reading the default `design::text::DATA` would
    /// leave this surface at 12px while the resource table grows. The row is the other half of
    /// the promise: a taller glyph in a 28px box is cropped, and the setting's own help text says
    /// the row grows so text is never cropped. The prose rows beside it must not move, or the
    /// setting would be scaling the whole panel rather than the data in it.
    #[gpui::test]
    #[gpui::test]
    fn a_describe_data_value_follows_the_configured_data_font(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        // The widest Inspector, so the rows under test are the one-line two-column layout.
        cx.simulate_resize(gpui::size(gpui::px(480.0), gpui::px(640.0)));
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.describe_stacked()),
            "this test is about the one-line row, and 480px is the width that keeps it"
        );
        // A Status field is a data column; a label is prose.
        const DATA_ROW: &str = "inspector-describe-field-configHash";
        const PROSE_ROW: &str =
            "inspector-describe-field-company.example.com/team-platform/extremely-long-label-name";

        let before = f32::from(
            cx.debug_bounds(DATA_ROW)
                .expect("the data row is laid out")
                .size
                .height,
        );
        assert!((before - f32::from(design::size::ROW)).abs() <= 1.0);

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

        let expected = cx.update(|_, cx| crate::settings::data_typography(cx).row_height());
        assert!(
            f32::from(expected) > f32::from(design::size::ROW),
            "the fixture has to actually raise the data font, or this test proves nothing"
        );
        let data_height = f32::from(
            cx.debug_bounds(DATA_ROW)
                .expect("the data row is still laid out")
                .size
                .height,
        );
        assert!(
            (data_height - f32::from(expected)).abs() <= 1.0,
            "a data value is cropped in a {before}px row once the reader raises the data font: \
             the row drew {data_height}px and the configured line needs {expected}px"
        );
        let prose_height = f32::from(
            cx.debug_bounds(PROSE_ROW)
                .expect("the prose row is laid out")
                .size
                .height,
        );
        assert!(
            (prose_height - f32::from(design::size::ROW)).abs() <= 1.0,
            "a label is prose, not data: the data font size must not restyle the whole panel"
        );
    }

    // A truncated value is a display decision, so the panel has to offer the whole text to the
    // keyboard. This asserts the affordances exist, not that a value stays clipped.
    #[gpui::test]
    /// A shortcut nobody can see is not a shortcut. The field rows answer to two chords, so
    /// the rows have to actually show them once the reader is on the row.
    #[gpui::test]
    fn a_value_row_shows_the_chords_it_answers_to(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();

        let chords = cx
            .debug_bounds("inspector-describe-field-configHash-chords")
            .expect("a field row renders its chords");
        // The chords must not take the room the value needs. The value column is the flexible
        // one, so the pair has to fit inside the row rather than pushing the text out.
        assert!(
            chords.size.width > px(0.),
            "the chord pair occupies space, so it cannot have collapsed to nothing"
        );
        assert!(
            chords.size.height <= design::size::ROW,
            "the chords stay inside the row rhythm instead of growing it"
        );
    }

    #[gpui::test]
    fn every_value_row_is_a_tab_stop_with_an_expand_and_a_copy_path(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        for selector in [
            "inspector-describe-field-configHash",
            "inspector-container-image-api",
            "inspector-describe-condition-message-Ready",
        ] {
            assert!(
                panel
                    .read_with(cx, |panel, _| panel.value_focus_handle(selector))
                    .is_some(),
                "{selector} must be reachable with the keyboard"
            );
        }
        // A handle the panel holds is not yet a tab stop, so walk the rendered order for one of
        // them. The pool is handed out in render order, so the rows keep the reading order.
        let entry = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        let row_focus = panel
            .read_with(cx, |panel, _| {
                panel.value_focus_handle("inspector-describe-field-configHash")
            })
            .expect("the row keeps its handle");
        assert!(
            reaches_in_tab_order(cx, &entry, &row_focus),
            "a Describe value row is reached by Tab"
        );

        // A row expands to its full value and copies the text the ellipsis hides.
        let selector = "inspector-describe-field-configHash";
        let bounds = |cx: &mut gpui::VisualTestContext| {
            let row = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            let key = cx
                .debug_bounds("inspector-describe-field-configHash-key")
                .expect("key");
            let value = cx
                .debug_bounds("inspector-describe-field-configHash-value")
                .expect("value");
            (row, key, value)
        };
        let (collapsed_row, collapsed_key, collapsed_value) = bounds(cx);
        if !panel.read_with(cx, |panel, _| panel.describe_stacked()) {
            assert!(
                (f32::from(collapsed_key.origin.y) - f32::from(collapsed_value.origin.y)).abs()
                    < 1.0,
                "a collapsed row keeps its value beside the key"
            );
        }
        panel.update(cx, |panel, cx| panel.toggle_value_expansion(selector, cx));
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.value_is_expanded(selector)),
            "Enter expands the focused row"
        );
        let (expanded_row, expanded_key, expanded_value) = bounds(cx);
        assert!(
            f32::from(expanded_key.origin.y) < f32::from(expanded_value.origin.y),
            "an expanded row moves the key above the value, so the value gets the whole width"
        );
        assert!(
            f32::from(expanded_row.size.height) > f32::from(collapsed_row.size.height),
            "an expanded row grows instead of clipping: {:?} then {:?}",
            collapsed_row.size.height,
            expanded_row.size.height
        );

        panel.update(cx, |panel, cx| panel.copy_value(selector, cx));
        let clipboard = cx
            .read_from_clipboard()
            .expect("clipboard text")
            .text()
            .expect("clipboard string");
        assert!(
            clipboard.contains("sha256:abcdef"),
            "the copy takes the whole value: {clipboard:?}"
        );
        assert!(panel.read_with(cx, |panel, _| panel.value_copied(selector)));
    }

    // Every scrollable region in the panel is focusable, so the keyboard can reach the content
    // that the pointer can only drag.
    #[gpui::test]
    fn every_scrollable_region_has_a_focus_handle_and_scrolls(cx: &mut TestAppContext) {
        let (panel, _source, cx) = layout_source_setup(cx);
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        // A short viewport guarantees the content overflows, so a scroll key has somewhere to go.
        cx.simulate_resize(gpui::size(gpui::px(240.), gpui::px(200.)));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("describe-scroll").is_some(),
            "the Describe scroll region renders"
        );
        let describe = panel.read_with(cx, |panel, _| panel.describe_focus.clone());
        let entry = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &describe),
            "the Describe scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&describe, cx));
        assert!(cx.update(|window, _| describe.is_focused(window)));
        // A GPUI scroll offset is the distance from the top of the content to the top of the
        // viewport, so moving the view down makes it more negative. That is the direction the
        // mouse wheel uses too.
        let before = panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y);
        cx.simulate_keystrokes("pagedown");
        cx.run_until_parked();
        let after = panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y);
        assert!(
            after < before,
            "Page Down moves the Describe view: {before} then {after}"
        );
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            f32::from(panel.read_with(cx, |panel, _| panel.describe_scroll.offset().y)),
            0.0,
            "Home returns to the first row"
        );

        // The Events list scrolls the same way. The Describe fixture holds no events, and a list
        // with nothing to scroll proves nothing, so the panel gets a source that has them.
        panel.update(cx, |panel, _| {
            panel.set_source(Arc::new(FakeSource {
                describes: AtomicUsize::new(0),
                events: AtomicUsize::new(0),
                object: Some(layout_pod_object(LAYOUT_UID)),
                event_count: 200,
            }) as Arc<dyn InspectorSource>);
        });
        // A new source is a new session, so it drops the selection. Events render for the
        // selected object, and an empty selection renders no list to scroll at all.
        select(&panel, LAYOUT_UID, cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("events-scroll").is_some());
        let events = panel.read_with(cx, |panel, _| panel.events_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &events),
            "the Events scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&events, cx));
        assert!(cx.update(|window, _| events.is_focused(window)));
        let events_offset =
            |panel: &InspectorPanel| panel.events_scroll.0.borrow().base_handle.offset().y;
        let before = panel.read_with(cx, |panel, _| events_offset(panel));
        cx.simulate_keystrokes("pagedown");
        cx.run_until_parked();
        let after = panel.read_with(cx, |panel, _| events_offset(panel));
        assert!(
            after < before,
            "Page Down moves the event list: {before} then {after}"
        );

        // The Metrics charts scroll through the same contract.
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("metrics-scroll").is_some());
        let charts = panel.read_with(cx, |panel, _| panel.metrics_focus.clone());
        assert!(
            reaches_in_tab_order(cx, &entry, &charts),
            "the Metrics scroll region is reached by Tab"
        );
        cx.update(|window, cx| window.focus(&charts, cx));
        assert!(cx.update(|window, _| charts.is_focused(window)));
    }

    // Apply is gated on validity, and the problems list is a list the keyboard can walk.
    #[gpui::test]
    fn a_parse_problem_blocks_apply_and_the_list_can_be_walked(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("@oops");
        // The editor parses after a typing pause, so the test waits out the same debounce the
        // user waits out. `run_until_parked` does not move the clock. The two steps bracket
        // `VALIDATE_DEBOUNCE_TEST` from both sides, so a change to the real debounce fails here
        // instead of leaving the test waiting on a stale copy of the number.
        cx.executor()
            .advance_clock(VALIDATE_DEBOUNCE_TEST - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            inline_diagnostics(&panel, cx).is_empty(),
            "validation waits for the typing to pause"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            !inline_diagnostics(&panel, cx).is_empty(),
            "the editor reports the problem while typing"
        );
        assert!(
            cx.debug_bounds("yaml-problems").is_some(),
            "the problems list replaces the silent disabled button"
        );
        // The gate is the point: broken YAML never reaches a review, let alone a request.
        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()),
            "a document with a diagnostic must not open a review"
        );
        let cursor = panel.read_with(cx, |panel, _| panel.problem_cursor);
        assert_eq!(cursor, 0);
        panel.update(cx, |panel, _| panel.move_problem_cursor(1));
        panel.update(cx, |panel, _| panel.move_problem_cursor(1));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_cursor),
            0,
            "the cursor stops at the last problem"
        );
        assert!(cx.debug_bounds("yaml-problem-0").is_some());
    }

    // A list longer than the cap is capped, and the cap drops nothing.
    #[gpui::test]
    fn a_long_problem_list_is_capped_and_keeps_every_problem(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        // The parser stops at the first syntax error, so a long list arrives the way a server
        // check would deliver it: as a batch the panel has to render.
        let count = 16;
        assert!(
            count > PROBLEMS_VISIBLE_ROWS,
            "the fixture has to be longer than the cap"
        );
        let diagnostics = (0..count)
            .map(|index| Diagnostic {
                line: index * 2,
                column: 0,
                message: format!("problem {index}"),
            })
            .collect::<Vec<_>>();
        let editor = panel.read_with(cx, |panel, _| panel.yaml_view.clone());
        editor.update(cx, |view, cx| view.set_diagnostics(diagnostics, cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_count),
            count,
            "the panel counts every problem the editor reported"
        );
        let list = cx
            .debug_bounds("yaml-problem-list")
            .expect("the problems list");
        assert!(
            list.size.height <= problems_list_max_height(),
            "a long list is capped instead of pushing the editor out of the panel: {}",
            f32::from(list.size.height)
        );
        assert!(
            cx.debug_bounds("yaml-problem-0").is_some(),
            "the first problem stays in the list"
        );
        // The cap is a viewport, not a truncation: the row past the cap is still rendered, and
        // the keyboard walk reaches it.
        assert!(
            cx.debug_bounds("yaml-problem-15").is_some(),
            "a capped list drops nothing"
        );
        let last = count - 1;
        panel.update(cx, |panel, _| panel.move_problem_cursor(last as isize));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.problem_cursor),
            last,
            "the cursor reaches the last problem"
        );
    }

    // The review names the object a request would touch and shows the local change.
    #[gpui::test]
    /// The review has to be honest about what it has and has not verified. A local diff cannot
    /// see a schema violation or an immutable field, so until the server has answered, the copy
    /// says the server has not seen the document.
    #[gpui::test]
    fn the_review_says_when_the_server_has_not_checked_the_document(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        let status = cx
            .debug_bounds("yaml-review-check-status")
            .expect("the review states what has been verified");
        assert!(status.size.height > px(0.), "the status line is rendered");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::NotRun,
            "opening a review must not imply the server was asked"
        );

        // Asking is a separate, explicit action, and it never starts a write.
        panel.update(cx, |panel, _| {
            panel.set_targeted_check_handler(|_, _| {});
        });
        panel.update(cx, |panel, cx| panel.check_pending_apply(cx));
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_applying()),
            "a server check must not apply the change"
        );
    }

    /// A verdict belongs to the document that was checked, so it must not survive the review.
    #[gpui::test]
    fn a_verdict_does_not_outlive_the_review_it_checked(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            panel.apply_check_finished(Ok(ApplyVerdict::Valid), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::Valid
        );

        panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_check.clone()),
            ApplyCheckState::NotRun,
            "a stale verdict must not greet the next document"
        );
        assert!(
            cx.debug_bounds("yaml-apply-review").is_none(),
            "the review is gone"
        );
    }

    /// Every verdict has to read as words, because a person has to act on it.
    #[gpui::test]
    fn every_verdict_has_a_readable_outcome(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();

        for (result, expected) in [
            (Ok(ApplyVerdict::Valid), ApplyCheckState::Valid),
            (
                Ok(ApplyVerdict::Conflict {
                    owners: vec!["kube-controller-manager".to_owned()],
                }),
                ApplyCheckState::Conflict {
                    owners: vec!["kube-controller-manager".to_owned()],
                },
            ),
            (
                Ok(ApplyVerdict::Conflict { owners: Vec::new() }),
                ApplyCheckState::Conflict { owners: Vec::new() },
            ),
            (
                Err("The server could not be reached.".to_owned()),
                ApplyCheckState::Failed {
                    reason: "The server could not be reached.".to_owned(),
                },
            ),
        ] {
            panel.update(cx, |panel, cx| panel.apply_check_finished(result, cx));
            cx.run_until_parked();
            assert_eq!(
                panel.read_with(cx, |panel, _| panel.apply_check.clone()),
                expected
            );
            assert!(
                cx.debug_bounds("yaml-review-check-status").is_some(),
                "every verdict renders a line the reader can act on"
            );
        }
    }

    #[gpui::test]
    fn the_review_names_the_target_and_shows_the_change(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        cx.run_until_parked();
        let review = panel.read_with(cx, |panel, _| panel.reviewed_change().cloned());
        let review = review.expect("a reviewed change");
        assert_eq!(review.target.uid, "uid-1");
        assert_eq!(review.target.name, "web-0");
        // Opening the review is not the write. Only `confirm_pending_apply` starts a request, so
        // this is the half of "the write needs an explicit action" that a selector cannot prove.
        assert!(
            !panel.read_with(cx, |panel, _| panel.is_applying()),
            "Apply only opens a review; it must not start a write"
        );
        assert!(cx.debug_bounds("yaml-apply-review").is_some());
        assert!(
            cx.debug_bounds("yaml-review-apply").is_some(),
            "the write needs an explicit action"
        );
        assert!(
            cx.debug_bounds("yaml-review-keep-editing").is_some(),
            "the safe action is in the same strip"
        );
        // The claim is about the tab order the user walks, so walk it. Both buttons are
        // focusable tabs, and the one Tab reaches first decides what Enter does.
        let entry = panel.read_with(cx, |panel, _| panel.tab_focus.clone());
        let keep_editing = panel.read_with(cx, |panel, _| panel.review_keep_editing_focus.clone());
        let write = panel.read_with(cx, |panel, _| panel.review_apply_focus.clone());
        cx.update(|window, cx| window.focus(&entry, cx));
        let mut first_review_stop = None;
        for _ in 0..TAB_ORDER_WALK_LIMIT {
            cx.update(|window, cx| window.focus_next(cx));
            if cx.update(|window, _| keep_editing.is_focused(window)) {
                first_review_stop = Some("Keep editing");
                break;
            }
            if cx.update(|window, _| write.is_focused(window)) {
                first_review_stop = Some("Apply to cluster");
                break;
            }
        }
        assert_eq!(
            first_review_stop,
            Some("Keep editing"),
            "the destructive action must not be the first tab stop of the review"
        );

        // Cancelling writes nothing and leaves the change on screen.
        panel.update(cx, |panel, cx| panel.cancel_pending_apply(cx));
        assert!(panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()));
        assert!(dirty(&panel, cx));
    }

    // The text the cluster reported stays recoverable after a write.
    #[gpui::test]
    fn revert_after_an_apply_restores_the_text_the_cluster_reported(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, _| panel.set_on_apply(|_| {}));
        let server_text = panel.read_with(cx, |panel, _| panel.original.clone());
        let server_text = server_text.expect("the selection text");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert!(!dirty(&panel, cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.original.clone()),
            Some(server_text.clone()),
            "an apply must not overwrite the text the cluster reported"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.applied_text().map(str::to_owned)),
            Some(matching_apply_yaml("uid-1")),
            "the applied text is kept beside it"
        );
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("yaml-action-cancel").is_some(),
            "the way back stays available after the editor is marked saved"
        );
        panel.update(cx, |panel, cx| panel.revert(cx));
        assert_eq!(yaml_text(&panel, cx), server_text);
    }

    // The Metrics tab is hidden for a reason, and the fallback says which one.
    #[gpui::test]
    fn a_hidden_metrics_tab_explains_itself(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        assert!(
            panel.read_with(cx, |panel, _| !panel.metrics_tab_visible()),
            "the fixture has no metrics source"
        );
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Metrics, cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab()),
            InspectorTab::Yaml
        );
        let notice = panel
            .read_with(cx, |panel, _| panel.metrics_notice.clone())
            .expect("a reason for the missing tab");
        assert!(
            notice.contains("metrics API"),
            "the fallback must name the cause: {notice}"
        );
    }

    #[gpui::test]
    fn events_load_only_when_the_tab_is_opened(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        assert_eq!(source.events.load(Ordering::Relaxed), 0);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.events_states.get("uid-1").map(|entry| &entry.state),
                Some(LoadState::Ready(_))
            ));
        });
    }

    #[gpui::test]
    fn large_event_result_uses_the_uniform_list_handle(cx: &mut TestAppContext) {
        let (panel, _source, cx) = events_source_setup(cx, 2_000);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("inspector-events-list").is_some());
        panel.read_with(cx, |panel, _| {
            assert!(panel.events_scroll.0.borrow().last_item_size.is_some());
            assert!(matches!(
                panel.events_states.get("uid-1").map(|entry| &entry.state),
                Some(LoadState::Ready(events)) if events.len() == 2_000
            ));
        });
    }

    #[test]
    fn event_display_order_sorts_by_time_and_keeps_undated_events_last() {
        let event = |name: &str, body: &str| {
            serde_yaml_ng::from_str::<DynamicObject>(&format!(
                "apiVersion: v1\nkind: Event\nmetadata:\n  name: {name}\n{body}"
            ))
            .expect("event")
        };
        let events = vec![
            event("missing", ""),
            event("old", "lastTimestamp: \"2026-09-23T09:00:00.100Z\"\n"),
            event("new", "lastTimestamp: \"2026-09-23T09:00:00.200Z\"\n"),
            event(
                "created",
                "  creationTimestamp: \"2026-09-23T10:00:00.300Z\"\n",
            ),
            event("tie", "lastTimestamp: \"2026-09-23T09:00:00.200Z\"\n"),
        ];
        let names = events_newest_first(events)
            .into_iter()
            .filter_map(|event| event.metadata.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["created", "new", "tie", "old", "missing"]);
    }

    #[gpui::test]
    fn metrics_retry_state_resets_when_hidden_shown_or_target_changes(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
            assert!(panel.metrics_should_sample());
            // Becoming visible queues one immediate sample. This test owns the clock, so it
            // has to collect that decision before the backoff can be observed.
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );

            panel.record_metrics_sample(Err("cluster unavailable".to_owned()), cx);
            assert_eq!(
                retry_delay_text(&panel.metrics_scheduler).as_deref(),
                Some("Retrying in 11 seconds")
            );
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::Wait(Duration::from_secs(11))
            );

            panel.set_metrics_visible(false, cx);
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            panel.set_metrics_visible(true, cx);
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );

            panel.record_metrics_sample(Err("cluster unavailable".to_owned()), cx);
            assert!(retry_delay_text(&panel.metrics_scheduler).is_some());
            let mut object = pod_ref("uid-2");
            object.name = "web-1".to_owned();
            panel.set_selection(
                Some(InspectorSelection {
                    object,
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
            assert_eq!(retry_delay_text(&panel.metrics_scheduler), None);
            assert_eq!(
                panel.metrics_scheduler.decide(Duration::ZERO),
                SampleDecision::SampleNow
            );
        });
    }

    #[gpui::test]
    fn apply_is_blocked_while_a_selection_is_pending(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        let requests: Rc<RefCell<Vec<ApplyRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = requests.clone();
        panel.update(cx, |panel, _| {
            panel.set_targeted_apply_handler(move |request: ApplyRequest, _| {
                sink.borrow_mut().push(request);
            });
        });
        cx.simulate_keystrokes("ctrl-a");
        let yaml = format!("{}\n", matching_apply_yaml("uid-1"));
        cx.simulate_input(&yaml);
        let pending = ObjectRef {
            name: "web-1".to_owned(),
            ..pod_ref("uid-2")
        };
        // The deferred selection carries the YAML of the object it navigates to, so the
        // object and its document must agree on the name.
        let pending_yaml = format!("{}\n", matching_apply_yaml_for("uid-2", &pending.name));
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pending,
                    yaml: pending_yaml.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        panel.update(cx, |panel, cx| panel.apply(cx));
        assert!(
            requests.borrow().is_empty(),
            "Apply must not write to the object the user navigated away from"
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.reviewed_change().is_none()),
            "A blocked change never reaches a review"
        );
        panel.read_with(cx, |panel, _| {
            let (title, reason) = panel
                .apply_blocked_by_pending()
                .expect("a pending selection blocks Apply");
            assert_eq!(title, "Apply paused");
            assert!(
                reason.contains("web-0"),
                "The message must name the object the YAML belongs to: {reason}"
            );
        });

        // Cancel resolves the state: the pending object loads and Apply targets it again.
        panel.update(cx, |panel, cx| panel.revert(cx));
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |panel, _| panel.has_pending()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.apply_target().unwrap().uid),
            "uid-2"
        );
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&pending_yaml);
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| panel.apply(cx));
        panel.update(cx, |panel, cx| panel.confirm_pending_apply(cx));
        assert_eq!(requests.borrow().len(), 1);
        assert_eq!(requests.borrow()[0].target.uid, "uid-2");
        assert_eq!(requests.borrow()[0].target.name, "web-1");
    }

    #[gpui::test]
    fn pending_apply_button_is_disabled(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(&matching_apply_yaml("uid-1"));
        cx.run_until_parked();
        let enabled = cx.debug_bounds("yaml-action-apply").expect("Apply button");
        assert!(f32::from(enabled.size.width) > 0.0);

        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: matching_apply_yaml("uid-2"),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        let disabled = cx
            .debug_bounds("yaml-action-apply")
            .expect("Apply button stays in place");
        assert_eq!(
            disabled.size.width, enabled.size.width,
            "A disabled Apply must not resize the toolbar"
        );
        assert!(
            cx.debug_bounds("yaml-clean-metadata").is_none(),
            "The pending state replaces the clean metadata with a status"
        );
    }

    #[gpui::test]
    fn undo_to_the_saved_text_loads_the_pending_selection(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "a: 1");
        select(&panel, "uid-1", cx);
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("a: 2");
        assert!(dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: ObjectRef {
                        name: "web-1".to_owned(),
                        ..pod_ref("uid-2")
                    },
                    yaml: "b: 3".to_owned(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.has_pending()));

        cx.simulate_keystrokes("ctrl-z");
        cx.run_until_parked();
        assert!(
            !panel.read_with(cx, |panel, _| panel.has_pending()),
            "Undoing back to the saved text resolves the pending selection"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.selection().unwrap().uid.clone()),
            "uid-2"
        );
        assert_eq!(yaml_text(&panel, cx), "b: 3");
    }

    #[gpui::test]
    fn a_repeated_selection_update_keeps_the_caret(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "alpha: 1\nbeta: 2");
        select(&panel, "uid-1", cx);
        assert!(!dirty(&panel, cx));
        panel.update(cx, |panel, cx| {
            panel
                .yaml_view
                .update(cx, |view, cx| view.place_caret(4, 8, cx));
        });
        let before = panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret());
        assert_eq!(before, (8, 4));

        // Same object, same content: the caret and selection stay put.
        select(&panel, "uid-1", cx);
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret()),
            before,
            "An identical selection update must not reset the selection"
        );

        // Same object, new content: a live update keeps the caret instead of jumping home.
        let updated = "kind: Pod\nmetadata: {}\nstatus: {}".to_owned();
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-1"),
                    yaml: updated.clone(),
                }),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, cx| panel.yaml_view.read(cx).caret()),
            before,
            "A live update must not move the caret"
        );
        assert_eq!(yaml_text(&panel, cx), updated);
        assert!(!dirty(&panel, cx));
    }

    #[gpui::test]
    fn returning_to_a_previous_object_reloads_it(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        let selection = |uid: &str| {
            Some(InspectorSelection {
                object: pod_ref(uid),
                yaml: "kind: Pod".to_owned(),
            })
        };
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);

        // Leave A while its request is still in flight, then come back.
        panel.update(cx, |panel, cx| panel.set_selection(selection("uid-2"), cx));
        panel.update(cx, |panel, cx| panel.set_selection(selection("uid-1"), cx));
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(
            source.describes.load(Ordering::Relaxed),
            3,
            "A must load again instead of waiting for a request that was dropped"
        );
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });
    }

    #[test]
    fn describe_payload_for_another_object_is_rejected() {
        let data = DescribeData {
            object: pod_object("web-0", "uid-new"),
            events: Vec::new(),
            owners: Vec::new(),
        };
        let error = prepare_describe_data(data, "uid-old").expect_err("mismatched UID");
        assert_eq!(error, OBJECT_REPLACED_REASON);
        assert_eq!(
            load_failure_hint(&error),
            "The object was replaced. Select the object again, then reload."
        );
        let matching = DescribeData {
            object: pod_object("web-0", "uid-old"),
            events: Vec::new(),
            owners: Vec::new(),
        };
        assert!(prepare_describe_data(matching, "uid-old").is_ok());
    }

    #[test]
    fn condition_polarity_follows_the_condition_type() {
        assert_eq!(condition_polarity("Ready"), ConditionPolarity::Healthy);
        assert_eq!(condition_polarity("Available"), ConditionPolarity::Healthy);
        assert_eq!(condition_polarity("Failed"), ConditionPolarity::Unhealthy);
        assert_eq!(
            condition_polarity("MemoryPressure"),
            ConditionPolarity::Unhealthy
        );
        assert_eq!(
            condition_polarity("NetworkUnavailable"),
            ConditionPolarity::Unhealthy
        );
    }

    #[gpui::test]
    fn pod_conditions_and_all_container_groups_are_listed(cx: &mut TestAppContext) {
        init_app(cx);
        let object = Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": { "name": "web-0", "namespace": "default", "uid": "uid-1" },
                "spec": {
                    "initContainers": [{
                        "name": "migrate", "image": "app:1",
                    }],
                    "containers": [{ "name": "app", "image": "app:1" }],
                    "ephemeralContainers": [{ "name": "debugger", "image": "busybox:1" }],
                },
                "status": {
                    "conditions": [
                        { "type": "Ready", "status": "True", "reason": "ContainersReady" },
                        { "type": "PodReadyToStartContainers", "status": "False" },
                    ],
                    "initContainerStatuses": [{
                        "name": "migrate", "ready": true, "restartCount": 0,
                        "state": { "terminated": { "exitCode": 0, "reason": "Completed" } },
                    }],
                    "containerStatuses": [{
                        "name": "app", "ready": false, "restartCount": 3,
                        "state": { "waiting": { "reason": "CrashLoopBackOff" } },
                    }],
                    "ephemeralContainerStatuses": [{
                        "name": "debugger", "ready": true, "restartCount": 0,
                        "state": { "running": {} },
                    }],
                },
            }))
            .expect("pod"),
        );
        let (panel, cx) = cx.add_window_view(|_, cx| InspectorPanel::new(cx));
        let source = Arc::new(FakeSource {
            describes: AtomicUsize::new(0),
            events: AtomicUsize::new(0),
            object: Some(object),
            event_count: 0,
        });
        panel.update(cx, |panel, _| {
            panel.set_source(source.clone() as Arc<dyn InspectorSource>);
        });
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();

        for selector in [
            "inspector-describe-container-app",
            "inspector-describe-container-migrate-init",
            "inspector-describe-container-debugger",
            "inspector-describe-condition-Ready",
            "inspector-describe-condition-PodReadyToStartContainers",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "Describe must list {selector}"
            );
        }
    }

    #[gpui::test]
    fn metrics_toolbar_actions_stay_inside_a_narrow_inspector(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(240.), gpui::px(640.)));
        cx.run_until_parked();
        cx.run_until_parked();
        assert_toolbar_actions(
            cx,
            "metrics-context-toolbar",
            &[
                "metrics-action-reload",
                "metrics-range-action-5m",
                "metrics-range-action-15m",
                "metrics-range-action-1h",
            ],
        );
    }

    #[gpui::test]
    fn metrics_range_and_retry_commands_change_the_panel(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Missing, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
        });
        cx.run_until_parked();
        // The Metrics toolbar binds these actions to the panel handle. The YAML editor
        // owned the focus in `setup`, and it is unmounted on this tab, so a window
        // dispatch would find no node to route the action from.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            DEFAULT_RANGE_MS
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(MetricsRange1h), cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            60 * 60 * 1000,
            "The 1h command must reach the panel"
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(MetricsRange15m), cx));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.metrics_range_ms),
            15 * 60 * 1000
        );
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();

        // Retry without a probe owner must not leave the panel stuck in Checking.
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_retry_handler(|_| {});
            panel.set_metrics_probe_state(MetricsProbeState::Missing, cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            panel.metrics_probe_retry = None;
            panel.set_metrics_source(
                Some(MetricsHandle::new(
                    metrics_runtime.handle().clone(),
                    Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
                )),
                MetricsProbeState::Missing,
                cx,
            );
            panel.retry_metrics(cx);
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.metrics_probe_task.is_some(),
                "Retry must run its own probe instead of waiting for an owner"
            );
        });
    }

    #[gpui::test]
    fn metrics_probe_state_keeps_error_reason_when_unavailable_arrives(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_state(
                MetricsProbeState::Error {
                    reason: "connection refused".to_owned(),
                },
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.metrics_probe,
                MetricsProbeState::Error { ref reason } if reason == "connection refused"
            ));
            assert_eq!(
                panel.metrics.last_error.as_deref(),
                Some("connection refused")
            );
        });
        panel.update(cx, |panel, cx| panel.set_metrics_available(false, cx));
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.metrics_probe,
                MetricsProbeState::Error { .. }
            ));
        });
        panel.update(cx, |panel, cx| {
            panel.set_metrics_probe_state(MetricsProbeState::Missing, cx);
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.metrics_probe, MetricsProbeState::Missing);
        });
    }

    // A `Loading` entry is cached so a second visit does not start a duplicate request, so
    // a request that never resolves must not pin the tab in a spinner.
    #[gpui::test]
    fn a_describe_that_never_answers_offers_retry(cx: &mut TestAppContext) {
        let (panel, source, cx) = hanging_setup(cx, false);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Loading)
            ));
        });

        cx.executor().advance_clock(LOAD_DEADLINE);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(
                    panel.describe_states.get("uid-1"),
                    Some(LoadState::Failed(reason)) if reason == LOAD_TIMEOUT_REASON
                ),
                "a stuck load becomes a retryable failure"
            );
            assert_eq!(
                load_failure_hint(LOAD_TIMEOUT_REASON),
                "The cluster is slow to answer. Retry, or check the cluster connection."
            );
            assert!(
                panel.describe_task.is_none(),
                "the stuck request is dropped so Retry starts a new one"
            );
        });
        assert!(
            cx.debug_bounds("inspector-load-error").is_some(),
            "the tab shows the failure with a next step"
        );

        // Retry asks the source again.
        panel.update(cx, |panel, cx| panel.ensure_describe(true, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 2);
    }

    // The deadline must not expire data that already arrived.
    #[gpui::test]
    fn a_loaded_describe_survives_the_load_deadline(cx: &mut TestAppContext) {
        let (panel, _source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(LOAD_DEADLINE * 3);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.describe_states.get("uid-1"),
                Some(LoadState::Ready(_))
            ));
        });
    }

    // Events are secondary: a failure is recoverable and leaves the rest of the panel
    // working.
    #[gpui::test]
    fn a_failing_events_load_is_best_effort_and_retryable(cx: &mut TestAppContext) {
        let (panel, source, cx) = hanging_setup(cx, true);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        panel.read_with(cx, |panel, _| {
            assert!(matches!(
                panel.events_states.get("uid-1"),
                Some(EventsEntry {
                    state: LoadState::Failed(_),
                    ..
                })
            ));
        });
        assert!(
            cx.debug_bounds("inspector-load-error").is_some(),
            "the Events tab explains the failure"
        );

        // The object data is unaffected, so the other tabs keep working.
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Yaml, cx));
        cx.run_until_parked();
        assert_eq!(yaml_text(&panel, cx), "kind: Pod");
        assert!(panel.read_with(cx, |panel, _| panel.selection().is_some()));

        // Retry asks the source again.
        panel.update(cx, |panel, cx| panel.ensure_events(true, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 2);
    }

    #[gpui::test]
    fn a_hanging_events_load_offers_retry(cx: &mut TestAppContext) {
        let (panel, _source, cx) = hanging_setup(cx, false);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(LOAD_DEADLINE);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(
                    panel.events_states.get("uid-1"),
                    Some(EventsEntry { state: LoadState::Failed(reason), .. }) if reason == LOAD_TIMEOUT_REASON
                ),
                "a stuck event load becomes a retryable failure"
            );
        });
    }

    // The Reload command must reach the toolbar of the tab that is open, and it has to
    // bypass the cache.
    #[gpui::test]
    fn the_reload_command_bypasses_the_describe_and_events_cache(cx: &mut TestAppContext) {
        let (panel, source, cx) = source_setup(cx);
        select(&panel, "uid-1", cx);
        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Describe, cx));
        cx.run_until_parked();
        assert_eq!(source.describes.load(Ordering::Relaxed), 1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();
        assert_eq!(
            source.describes.load(Ordering::Relaxed),
            2,
            "Reload asks for the object again"
        );

        panel.update(cx, |panel, cx| panel.show_tab(InspectorTab::Events, cx));
        cx.run_until_parked();
        assert_eq!(source.events.load(Ordering::Relaxed), 1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.focus_handle().focus(window, cx));
        });
        cx.update(|window, cx| window.dispatch_action(Box::new(ReloadActiveTab), cx));
        cx.run_until_parked();
        assert_eq!(
            source.events.load(Ordering::Relaxed),
            2,
            "Reload asks for the events again"
        );
    }

    // The Metrics target carries the UID, so a pod recreated under the same name starts
    // an empty series instead of charting its predecessor.
    #[gpui::test]
    fn a_recreated_object_restarts_the_metrics_series(cx: &mut TestAppContext) {
        let (panel, cx) = setup(cx, "name: app");
        select(&panel, "uid-1", cx);
        let metrics_runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("metrics runtime");
        let metrics = MetricsHandle::new(
            metrics_runtime.handle().clone(),
            Arc::new(EmptyMetricsPort) as Arc<dyn ClusterDataPort>,
        );
        panel.update(cx, |panel, cx| {
            panel.set_metrics_source(Some(metrics), MetricsProbeState::Available, cx);
            panel.set_metrics_visible(true, cx);
            panel.show_tab(InspectorTab::Metrics, cx);
            panel.record_metrics_sample(
                Ok(SamplePayload {
                    containers: vec![super::super::metrics::ContainerSample {
                        name: "app".to_owned(),
                        cpu_millicores: Some(100.0),
                        memory_bytes: Some(1024.0),
                    }],
                    window: "10s".to_owned(),
                    at_ms: 1_000,
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.metrics.series.len(), 1);
            assert_eq!(
                panel.metrics_target.as_ref().map(|target| target.uid()),
                Some("uid-1")
            );
        });

        // Same name, new UID: the samples of the old pod must not survive.
        panel.update(cx, |panel, cx| {
            panel.set_selection(
                Some(InspectorSelection {
                    object: pod_ref("uid-2"),
                    yaml: "kind: Pod".to_owned(),
                }),
                cx,
            );
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.metrics_target.as_ref().map(|target| target.uid()),
                Some("uid-2")
            );
            assert!(
                panel.metrics.is_empty(),
                "a recreated pod starts without the old samples"
            );
        });
    }
}
