//! Renders resource tables: their rows, their states, and their row actions.
//!
//! The 40px resource header that sits above this region — the kind icon, the
//! title, the query box, the `\u{22ef}` menu — belongs to `shell/panels.rs`. This
//! file owns everything below it: the summary strip, the inline error bar, the
//! header row, the body, and the selection action bar docked at the bottom.

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::assets::IconName;
use gpui_kit::base::Button as BaseButton;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::table::{
    Column, ColumnSort, DataTable, TableDelegate, TableEvent as SharedTableEvent, TableState,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::*;
use gpui_kit::{Focusable as _, TextRun};

use gpui_kit::{
    Anchor, AnyElement, AnyView, App, Bounds, ClickEvent, ClipboardItem, DismissEvent, Div,
    ElementId, Entity, FocusHandle, Font, FontFeatures, FontWeight, Hsla, KeyBinding, KeyDownEvent,
    Keystroke, MouseButton, Pixels, Point, Role, ScrollStrategy, SharedString, Stateful,
    Subscription, Task, WeakEntity, Window, div, font, px,
};
use k8s_core::machines::TableEvent;
use k8s_core::projection::{
    CellValue, Compare, Field, Filter, IndexSnapshot, Pred, QueryError, Right, Row, Sort,
};
use kube_core::DynamicObject;
use serde_json::Value;

use super::actions::{
    ClearFilter, DeleteSelection, FocusFilter, OpenDetails, OpenRowActions, Refresh, SelectNext,
    SelectNextColumn, SelectPrevious, SelectPreviousColumn, SortSelectedColumn, ToggleChurn,
    ToggleProblemsOnly, ToggleUpdates,
};
use super::columns::{
    ColumnClass, ResourceColumn, age_seconds, cell_paddings, columns_for, declines_table,
    default_hidden_columns, fit_columns, is_known_kind,
};
// The ink ladder is resolved by `columns::CellInk::color`, next to the
// declaration, so no kind decides a cell's colour for itself. The name is
// imported here for the one reader that has to name the role rather than call it:
// this module's test that asserts the identity column really did declare the
// primary ink.
#[allow(unused_imports)]
use super::columns::CellInk;
use super::host::{PendingOp, SourceFactory, TableHost, TableStatus};
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
const RETRY_LIVE_UPDATES: &str = "Retry live updates";
const RETRY_LOADING_RESOURCES: &str = "Retry";
/// Reloads a table that has no rows to show.
const REFRESH_RESOURCES: &str = "Refresh resources";
const RETRY_LIVE_UPDATES_GUIDANCE: &str =
    "Live updates stopped. The rows below are the last ones the cluster sent.";
const RETRY_LOADING_RESOURCES_GUIDANCE: &str =
    "Loading failed. The rows below are the last ones the cluster sent.";
const START_PORT_FORWARD: &str = "Start port forward";
/// Estimates the average glyph width as a fraction of the font size.
///
/// 0.52 is Inter's advance at 13px, and it is measured rather than guessed:
/// the previous 0.6 was a monospace advance, and a monospace advance applied to
/// a proportional face overestimates by 15%, so every cell thought it was
/// overflowing and none of them could prove it. The overflow check is what
/// decides whether a value gets a tooltip, so a wrong constant here is a wrong
/// tooltip on every cell in the table.
const CHAR_WIDTH_RATIO: f32 = 0.52;
/// Shortest usable width for a data column. `UI-SPEC` §10.2 gives `Age` 48, which
/// is this floor, so a design width can never be clipped by it.
const COLUMN_MIN_WIDTH: f32 = 48.0;
/// Longest width a column can reach from the menu.
///
/// The cap belongs to a column a reader drags, and only to that one. It used to
/// be applied to every column, including the flex column — and the flex column
/// is the one whose whole job is to take the room the others leave, so on any
/// table wider than about 1530px the cap bound first and the columns stopped
/// short of the panel: 1624px of columns in a 1623px centre column, resolved
/// `Node` to 734, and the component clamped it to 640. The 94px that vanished
/// was not empty space a reader would thank anyone for; it was the right end of
/// the table, and in the header band it showed up as a strip of the header's own
/// surface with no column in it.
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
const NO_PROBLEMS_GUIDANCE: &str = "The status filter is hiding every healthy row.";
/// Defines the key context for table actions.
const TABLE_CONTEXT: &str = "Table";
/// The table's keyboard contract, read by a screen reader before the first row.
///
/// Seven short steps and nothing else. It names the three things a table does
/// not do on its own — extend a range, jump by name, and give the selection up —
/// because each of them is a native list behaviour a reader expects to find and
/// an absence of which reads as a broken table rather than as a missing feature.
const TABLE_ACCESSIBILITY_DESCRIPTION: &str = "Use Up and Down to move between rows, or Shift with them to extend the selection. Use Tab to select a data column and Ctrl+Tab to leave the table. Use Shift+Enter to sort the selected column. Press F10, Shift+F10, or Menu to open Row Actions. Press Enter to open the details for the selected row. Type to jump to a row by name, and press Escape to clear a multi-row selection. Scroll horizontally to view more columns.";
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
/// The gap between the status cell's dot and its word.
///
/// `UI-SPEC` §4.4 fixes it at 6px, and it is `design::space::ICON` rather than a
/// restated number: it *is* the gap between a mark and a label, and the scale
/// already has that gap. §2.1's ten values govern the space *between* elements
/// and this is the gap inside one mark beside one word — the same 6px as the dot
/// it separates, because a gap measured on the grid between a 6px dot and a 13px
/// word is a gap that belongs to neither of them. The dot itself is
/// `design::size::STATUS_DOT`, which is 6 for the same reason — a dot sized on
/// the grid reads as a square.
const STATUS_DOT_GAP: Pixels = design::space::ICON;

/// The one leading edge every band of the table region insets to.
///
/// The summary line, the column header, the body cells, the inline error bar,
/// the loading skeleton and the selection bar are six siblings describing one
/// level of the interface, and §"Alignment details" asks them for one content
/// inset between them rather than for six that happen to be close. It is
/// [`design::space::LG`] — the same value [`super::columns::cell_paddings`]
/// charges every cell — so the first cell's text and the first header's label
/// and the first word of the summary all start on the panel's own content line.
///
/// One named constant rather than six calls to `space::LG`, because the reason
/// the bars had to be fixed one at a time is that nothing said they were the same
/// line: the selection bar was centred, the error bar carried a 3px rule inside
/// its own padding, and the skeleton had its own idea of the padding.
const TABLE_CONTENT_INSET: Pixels = design::space::LG;

/// `UI-SPEC` §4.15: the inline error bar is 32px, under the table body.
/// `design::size` has no token for it; `TABLE_HEADER` and `SUMMARY_STRIP` are
/// both 32 and both mean something else, so borrowing one of them would make the
/// two bands able to drift apart in the future.
const INLINE_ERROR_HEIGHT: Pixels = px(32.0);

/// The width of the summary strip's proportion mark.
///
/// The spec says 120px and the mark is 96, which is a deliberate departure: at 96
/// the mid-range segments are still 30px apiece — far past the point where the
/// eye stops resolving them — while the band spends a quarter less ink on it. A
/// lane that is too long stops being a proportion and becomes a chart, and the
/// band sits under a 32px table header; a chart there competes with the rows.
///
/// The width is also what [`RAIL_MIN_SEGMENT`] and [`RAIL_MAX_SHARE`] are
/// calibrated against, so it is the one number in this mark that cannot move for
/// a look: the floor is 4.2% of it and the cap is a share of it.
const SUMMARY_MICROBAR_WIDTH: Pixels = px(96.0);
/// Height of the proportion mark, and the reason it is thinner than a dot.
///
/// [`design::space::XS`], four pixels, and not [`design::size::STATUS_DOT`] which
/// is what it used to be. Six is half the cap height of the `text::LABEL` the mark
/// sits beside, so at six the healthy bucket was eighty pixels of solid
/// `status.success` and the strip's loudest object — louder than the `2 rows in
/// error` it was reporting, and heavier than the `NAME` column one band below.
/// A mark that weighs half the type beside it is a bar; a quarter of it is a
/// mark. It cannot go thinner than this and stay a mark: at three it read as a
/// rule — a flat line with two coloured ticks on it, which a reader could not
/// tell from the header's own hairline two bands below.
const SUMMARY_MICROBAR_HEIGHT: Pixels = design::space::XS;
/// The mark's corner radius, applied to the lane and inherited by every segment.
///
/// [`design::radius::XS`] is the same radius the 6px status dot is drawn at, and a
/// radius must govern the whole visible surface rather than the border alone — the
/// lane clips its segments, so the lane's radius is the radius of the first and
/// last segment whatever they are drawn at.
const SUMMARY_MICROBAR_RADIUS: Pixels = design::radius::XS;
/// Narrowest a non-zero bucket is ever drawn at, in logical pixels.
///
/// Twice the 2px gap that separates segments, so the smallest bucket on screen is
/// a mark and not a hairline between two others, and so a 1-in-20 population is
/// legible at the size the mark is actually drawn. 4px of a 96px lane is 4.2%,
/// which is the largest this glyph will ever over-report a bucket by.
const RAIL_MIN_SEGMENT: f32 = 4.0;
/// Largest share of the rail any one bucket may fill.
///
/// The cap is the half of the fix that a floor cannot do. A floor makes 1%
/// visible; it does not stop 99% from painting the entire lane, and a full lane
/// of one ink is the exact thing a glance at this table must never say.
/// Bounding every bucket leaves the tail of the lane empty, so the mark reads
/// "nearly all of it" instead of "all of it" — and it costs the dominant bucket
/// at most 12% of a 96px lane, which the numbers beside it state exactly.
const RAIL_MAX_SHARE: f32 = 0.88;

/// Height of one placeholder bar in the loading skeleton.
///
/// [`design::space::SM`], the same token the summary strip's own proportion mark
/// takes its height from: a placeholder bar is a mark in a band, and a mark's
/// height belongs to the space scale rather than to a number written beside it.
const SKELETON_BAR_HEIGHT: Pixels = design::space::SM;

/// The fixed lane a row reserves at its trailing edge, and the gap that keeps it
/// off the value beside it.
///
/// A trailing control has to live in a lane of its own, or it is a thing that
/// appears on some rows and not others and pushes the last column's value around
/// when it does. The table has exactly one such mark — the observation-confidence
/// glyph — and it used to be appended to the last cell only when it had something
/// to say, so a stale table silently right-aligned the last column's values and a
/// fresh one left them where they were. The lane is the same 32px
/// [`ColumnClass::Action`] reserved: an icon button's box plus one `space::SM`.
const TRAILING_LANE: Pixels = design::size::ICON_BUTTON;
/// The gap between a row's trailing value and its lane.
const TRAILING_LANE_GAP: Pixels = design::space::SM;

/// `UI-SPEC` §4.14: a list that resolves inside this window shows nothing at all.
/// A spinner that appears for 40ms and disappears is a flash, and a flash reads
/// as a glitch rather than as speed. This is the first rung of [`LoadingTier`],
/// which is what a view actually reads; the constant is the number that rung is
/// named after.
const LOADING_INVISIBLE: Duration = Duration::from_millis(200);
/// `UI-SPEC` §4.14: past this the reader has waited long enough to be told how
/// many rows are on the way.
const SKELETON_AFTER: Duration = Duration::from_millis(500);
/// Past this the reader is owed a quantity as well as a spinner.
const LOADING_PROGRESS: Duration = Duration::from_secs(2);
/// The skeleton's one second-and-a-bit breathe, `UI-SPEC` §4.14: `.05 \u{2194} .09`.
const SKELETON_BREATHE: Duration = Duration::from_millis(1_600);

/// `UI-SPEC` §0 铁律三 grades `Pending` by age: under this it is a pod that has
/// only just been scheduled and reads grey like any healthy row.
const PENDING_WARNING_AFTER: Duration = Duration::from_secs(30);
/// Past this a `Pending` pod is not waiting for anything, and has to be
/// findable among the thousands that are.
const PENDING_DANGER_AFTER: Duration = Duration::from_secs(300);

/// How long a type-ahead prefix survives before the next keystroke starts a new
/// one. Native lists use about a second.
const TYPEAHEAD_RESET: Duration = Duration::from_millis(1_000);

/// Keeps a tooltip from growing past a comfortable reading measure
/// (`UI-SPEC` §4.12`).
const TOOLTIP_MAX_WIDTH: Pixels = px(280.0);
/// Caps the tooltip height so a long value stays scrollable.
const TOOLTIP_MAX_HEIGHT: Pixels = px(240.0);
/// Width of the name filter above a table. One number owns the field and the
/// wrapper that paints its error line and tooltip, so they cannot drift.
const FILTER_INPUT_WIDTH: Pixels = px(240.);
/// Width of a column's value filter popover: the same measure the name filter
/// takes, since both are pick-a-value fields.
const COLUMN_FILTER_WIDTH: Pixels = FILTER_INPUT_WIDTH;
/// Twelve 26px rows before the popover scrolls.
const COLUMN_FILTER_MAX_HEIGHT: Pixels = px(320.);
/// Shapes a value only when its width estimate is within this factor of the
/// cell width, which is the only range where the estimate can be wrong.
const TOOLTIP_SHAPE_FACTOR: f32 = 2.0;
/// Caps how much of one value is shaped for measurement.
const TOOLTIP_SHAPE_MAX_CHARS: usize = 512;
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

/// Stores the Pod or Service a Port Forward starts from, and the namespace it lives in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardTarget {
    pub namespace: Option<String>,
    pub name: SharedString,
    /// The ports the target offers, so the dialog can offer them as choices: a Pod's
    /// declared `containerPort` values, or a Service's own `targetPort` list.
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

/// Lists resource kinds that can start a Port Forward.
///
/// A forward is a Pod stream, so a Pod is the target itself. A Service is a second legal
/// starting point because a Service is usually what the reader has in front of them, and
/// its `spec.ports` name the same ports under the name the Pod calls its `targetPort`. Every
/// other kind has no port of its own, so nothing it can offer would ever connect.
const FORWARDABLE_KINDS: [&str; 2] = ["Pod", "Service"];

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

/// How tall a table row is (`UI-SPEC` §4.4, `PROMPT` §2.1 #13).
///
/// Comfort is the default and the reason: the default view is already filtered
/// down to the rows that need attention, so the common case is a dozen rows and
/// there is nothing to gain by making them tight. Dense is for the sweep over
/// all 10,000, and it is a decision the reader makes, not one the app makes for
/// them at the worst moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    #[default]
    Comfort,
    Normal,
    Dense,
}

impl Density {
    /// The row height this density starts at.
    ///
    /// `Comfort` reads [`design::size::ROW`] rather than restating 32, so the
    /// density ladder and the product's row token cannot drift apart.
    fn floor(self) -> Pixels {
        match self {
            Self::Comfort => design::size::ROW,
            Self::Normal => design::size::ROW_NORMAL,
            Self::Dense => design::size::ROW_DENSE,
        }
    }
}

/// The reader's density, shared by every table in the app.
///
/// `UI-REDESIGN` L11 makes it a per-cluster remembered setting, and remembering
/// it means one value for every table: two tables in one window at two different
/// densities read as two different products. Persistence lives in `settings.rs`,
/// which is a shared contract this file does not own, so the value is exposed
/// through [`PodsView::set_density`] and [`PodsView::density`] for Wave 2 to
/// wire to a control and a store.
#[derive(Clone)]
struct DensitySetting(Rc<Cell<Density>>);

impl Density {
    fn read(cx: &App) -> Self {
        cx.try_global::<DensitySetting>()
            .map_or(Density::Comfort, |setting| setting.0.get())
    }
}

impl gpui_kit::Global for DensitySetting {}

/// `UI-SPEC` §4.14's loading ladder: nothing, a spinner, a skeleton, then a
/// spinner with progress.
///
/// The first rung is the one the old code skipped. It showed a spinner from the
/// first frame, so a list that resolved in 40ms produced a spinner that appeared
/// and vanished inside a single frame — a flash, which reads as a glitch rather
/// than as speed. `PROMPT` §2.1 #12 is explicit: under 200ms, show nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum LoadingTier {
    /// Under [`LOADING_INVISIBLE`]. Draw no loading state at all.
    #[default]
    Nothing,
    /// [`LOADING_INVISIBLE`] to [`SKELETON_AFTER`]: a spinner and nothing else.
    Spinner,
    /// Past [`SKELETON_AFTER`]: the skeleton, which is the only place one belongs.
    Skeleton,
    /// Past [`LOADING_PROGRESS`]: the skeleton plus a row count.
    Progress,
}

/// The severity tally the summary strip draws.
///
/// The four buckets are a *partition* of the rows, cut by the status word the
/// reader can see in the Status column rather than by a grade they cannot. That
/// distinction is the whole reason the strip reads correctly: a Pod that has been
/// `Pending` for six minutes is graded `Error` — that is `UI-SPEC` §0 铁律三, and
/// the dot in its cell is red — but it has not *failed*, and a strip that counted
/// it under "rows failed" put `9,903 rows failed` above a table where 9,903 rows
/// said `Pending`. It also counted those rows a second time, because `pending` was
/// a cross-cutting note rather than a bucket, and the strip said the same number
/// twice with two different words. Nothing in the table could be reconciled with
/// either figure.
///
/// So: a row lands in exactly one bucket, the bucket is named after the word in
/// its cell, and the *ink* is what carries the grade. A strip that says
/// `110 rows healthy · 9,903 pending` in red is saying the same thing about the
/// same rows as the column below it, in one line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowSummary {
    healthy: usize,
    /// Neither healthy nor waiting, at the warning grade.
    warning: usize,
    /// Neither healthy nor waiting, at the error grade.
    danger: usize,
    /// Pods that have not been scheduled yet, whatever their grade. A *status
    /// word*, not a severity: 9,900 pods that were scheduled a second ago are a
    /// queue, and 9,900 that have been waiting five minutes are a cluster that
    /// has stopped making progress.
    pending: usize,
    /// The worst grade any pending row actually reached.
    pending_severity: Option<Severity>,
}

impl RowSummary {
    fn total(&self) -> usize {
        self.healthy + self.warning + self.danger + self.pending
    }

    /// Folds one row's grade into the tally.
    fn record(&mut self, pending: bool, severity: Severity) {
        // A waiting row is counted once, as waiting. Counting it here as well is
        // what made the strip report one population twice.
        if pending {
            self.pending += 1;
            self.pending_severity = Some(worst(self.pending_severity, severity));
            return;
        }
        match severity {
            Severity::Error => self.danger += 1,
            Severity::Warning => self.warning += 1,
            _ => self.healthy += 1,
        }
    }
}

/// The worse of two grades, where a missing grade is better than any grade.
fn worst(so_far: Option<Severity>, next: Severity) -> Severity {
    match (so_far, next) {
        (Some(Severity::Error), _) | (_, Severity::Error) => Severity::Error,
        (Some(Severity::Warning), _) | (_, Severity::Warning) => Severity::Warning,
        _ => Severity::Success,
    }
}

/// Lays the strip's bar out for a tally: one width in logical pixels per bucket,
/// healthy first, zero for a bucket that is not there.
///
/// This is not a proportion, and that is the whole point. A stacked bar is
/// honest only while every segment is wide enough to survive antialiasing, and
/// a 10,010-row cluster with 110 healthy rows is the case where it is least
/// true: 1.1% of 96px is one pixel, the reader's eye resolves nothing there,
/// and the bar reports 100% failed while the 110 that are fine sit invisibly
/// inside it. So every bucket that exists is floored at [`RAIL_MIN_SEGMENT`]
/// and every bucket is capped at [`RAIL_MAX_SHARE`], which bounds the mark's
/// error at *both* ends — at most 4px too wide for the smallest bucket, at most
/// 12% of the lane too narrow for the largest — and leaves the mid-range
/// exact, so 50/50 is 46 and 46.
///
/// The gaps come out of the budget before the shares do, which is what makes
/// that 46/46 fit: 48 + 2 + 48 is 98px inside a 96px lane, and the old mark
/// spent those last 2px clipped by its own container. Three buckets then cannot
/// overflow what is left: the cap binds on at most one of them, so the worst
/// case is `0.88u + 2x3` against `u = 96 - 4`, and `84.9 < 92`.
///
/// Four buckets, because a waiting row is now a bucket of its own rather than a
/// note across the other three. The bound still holds — `0.88u + 2(n-1) < u`
/// with `u = 96 - 2(n-1)` needs `n <= 6` — and the segments keep the same order
/// as the figures beside them, so the mark and the numbers are read the same way.
///
/// Pure, so the extremes — the case a window is the slow way to reach and the
/// only case worth a test — can be measured directly.
fn rail_widths(summary: &RowSummary) -> [f32; 4] {
    let buckets = [
        summary.healthy,
        summary.warning,
        summary.danger,
        summary.pending,
    ];
    let present = buckets.iter().filter(|count| **count > 0).count();
    let total = summary.total();
    if present == 0 || total == 0 {
        return [0.0; 4];
    }
    let gap = f32::from(design::space::XXS);
    let usable = f32::from(SUMMARY_MICROBAR_WIDTH) - gap * (present as f32 - 1.0);
    buckets.map(|count| {
        if count == 0 {
            return 0.0;
        }
        let share = count as f32 / total as f32;
        (share * usable)
            .min(RAIL_MAX_SHARE * usable)
            .max(RAIL_MIN_SEGMENT)
    })
}

pub struct PodsView {
    spec: ResourceSpec,
    host: Entity<TableHost>,
    /// Stores all columns used for values and sorting.
    columns: Arc<Vec<ResourceColumn>>,
    visible_columns: Vec<usize>,
    hidden_columns: HashSet<String>,
    /// Stores the last saved width for each column.
    ///
    /// *The reader's own widths, and only those.* The width policy in
    /// [`Self::resolve_surplus_width`] computes a width for almost every column on
    /// almost every frame, and this map used to absorb the result — so a policy
    /// output came back on the next launch as though the reader had chosen it, and
    /// a table whose columns the reader had never touched was permanently pinned
    /// to whatever the last window size happened to be. Only [`Self::reader_widths`]
    /// is written here now.
    saved_widths: BTreeMap<String, f32>,
    /// The columns whose width the reader has set, by drag or by the menu.
    ///
    /// This is the one thing the width policy is not allowed to have an opinion
    /// about, and it cannot be inferred from a width being unusual: the policy
    /// produces unusual widths by design. So the origin is recorded where it is
    /// knowable — the drag that reports a whole width set, the menu entry that
    /// moves one column — and seeded from [`Self::saved_widths`], which is the
    /// record of those same choices across launches.
    reader_widths: HashSet<String>,
    /// The shared table. It owns the column widths, the vertical and horizontal
    /// scroll handles, and the virtualization of the body.
    ///
    /// `TableState::new` reads the window, and an entity can only be created
    /// inside a frame, so the table is built on the first render rather than in
    /// the constructor. Everything that reads the scroll handles asks for the
    /// handle through [`Self::table`], which is `None` only before that frame.
    table: Option<Entity<TableState<ResourceTableDelegate>>>,
    /// One width per visible column, in display order. The shared table holds
    /// the live values; this copy is what the header, the tooltips, the resize
    /// overlay and the saved settings all measure against.
    column_widths: Vec<f32>,
    widths_epoch: u64,
    widths_task: Option<Task<()>>,
    filter: Entity<TextInput>,
    filter_epoch: Rc<Cell<u64>>,
    filter_task: Option<Task<()>>,
    filter_pending: bool,
    /// Keeps the selected row across snapshot rebuilds.
    selected_uid: Option<SharedString>,
    /// Keeps every selected row. It always contains `selected_uid`.
    selected_uids: Arc<HashSet<SharedString>>,
    /// Marks where a Shift range selection starts.
    selection_anchor: Option<SharedString>,
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
    /// The listed position [`Self::reveal_row`] last asked to scroll into view.
    pending_row_reveal: Option<usize>,
    context_menu: Option<Entity<PopupMenu>>,
    context_menu_previous_focus: Option<FocusHandle>,
    context_menu_position: Point<Pixels>,
    /// Which column's value popover is open, if one is.
    ///
    /// The popover itself is gpui-kit's [`Popover`], hung off the header control
    /// that opens it, so it anchors to the header by construction rather than by a
    /// position this file computes — and its rows are re-read every frame, so the
    /// counts and the ticks are never a frame behind the click that changed them.
    column_filter_column: Option<usize>,
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
    /// The failure the reader dismissed from the inline error bar. Held as the
    /// reason itself rather than as a flag, so a *new* failure shows its bar
    /// again while the dismissed one stays dismissed.
    dismissed_error: Option<String>,
    /// Whether the focus the table currently holds came from a key.
    ///
    /// gpui 0.6.6 exposes no `focus_visible` helper, and
    /// `contains_focused` answers a different question: it is true after a
    /// click. `PROMPT` §2.1 #10 is that a mouse click produces no focus ring, so
    /// the origin has to be recorded. Every key press sets it and every mouse
    /// press clears it, and `RowVisual::FocusRing` and the filter control's ring
    /// gate on it — which is the whole difference between a focus ring that
    /// means "you are here" and one that means "something is focused".
    keyboard_focus: Cell<bool>,
    /// The row a type-ahead keystroke is accumulating towards, and when the last
    /// keystroke landed. Empty after [`TYPEAHEAD_RESET`].
    ///
    /// The timestamp is an `Option` so the whole pair is `Default`, which is what
    /// lets [`Cell::take`] hand it out and put a fresh one back without a clone
    /// of the prefix on every keystroke.
    typeahead: Rc<Cell<(String, Option<Instant>)>>,
    /// Which of `UI-SPEC` §4.14's four loading tiers the reader is in.
    ///
    /// The tier is advanced by *timers*, not by comparing elapsed time on every
    /// frame. A frame that arrives late then cannot skip a tier, and the whole
    /// ladder is reachable from a test's clock rather than from wall time — which
    /// is the difference between a loading rule and a loading rule nobody can
    /// check.
    loading_tier: LoadingTier,
    /// When the list started, for the skeleton's one breath. A visual, so the
    /// wall clock is the right one; the tier above is not.
    loading_since: Option<Instant>,
    /// The severity tally behind the summary strip, and the snapshot generation
    /// it was counted from. 10,000 rows is one pass per rebuild, never one per
    /// frame — a per-frame pass over the listed rows is the one O(n) this
    /// virtualized table cannot afford.
    summary: RowSummary,
    summary_generation: Option<u64>,
    /// The horizontal viewport the flex column was last resolved against, so a
    /// window resize re-resolves it and nothing else re-runs the pass.
    resolved_viewport: f32,
    /// The parsed query, kept so the header filter, the popover and the row
    /// visibility pass all read one answer rather than each re-parsing the box.
    ///
    /// The *string* in the box is the state; this is that string's meaning, and it
    /// is only ever written by [`Self::commit_filter`].
    preds: Vec<Pred>,
    /// The clause the query box could not read, if it could not read one. Shown
    /// inline under the box rather than as a toast: the error is about one token,
    /// and the reader has to be looking at the box to fix it.
    query_error: Option<QueryError>,
    /// Shows the high-latency notice once.
    latency_notified: bool,
    latency_auto_paused: bool,
    focus_handle: FocusHandle,
    /// The table's own tab stop. The shared table keeps a focus handle of its
    /// own for its selection model, but the app's keyboard contract — the
    /// `Table` key context, the problems filter, and every row action — is
    /// dispatched from here, so the view keeps and tracks this one.
    table_focus: FocusHandle,
    retry_focus: FocusHandle,
    /// Makes the failure reason reachable by keyboard and screen readers.
    error_reason_focus: FocusHandle,
    multi_delete: Option<MultiDeleteRequest>,
    stale_retry_focus: FocusHandle,
    empty_action_focus: FocusHandle,
    empty_namespace_focus: FocusHandle,
    multi_delete_confirm_focus: FocusHandle,
    multi_delete_cancel_focus: FocusHandle,
    _host_observation: Subscription,
    /// Watches the shared table for the column widths a resize drag produced.
    _width_changes: Option<Subscription>,
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

        // Tests do not read or write user column settings, so the default-hidden
        // set below is what every test sees too: that is deliberate, because the
        // default Pod row *is* the seven columns of `UI-SPEC` §10.2 and a test
        // that rendered the other two was not testing the shipping table.
        let mut hidden_columns: HashSet<String> = default_hidden_columns(&spec.kind)
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        let saved_widths: BTreeMap<String, f32> = if cfg!(test) {
            BTreeMap::new()
        } else {
            crate::settings::column_widths(&spec.kind)
                .into_iter()
                .collect()
        };
        if !cfg!(test) {
            // A reader who has already chosen their columns keeps them: the
            // design's default is only ever a starting point, never an override.
            hidden_columns.extend(crate::settings::hidden_columns(&spec.kind));
        }
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

        let column_widths: Vec<f32> = visible_columns
            .iter()
            .map(|&index| {
                let column = &columns[index];
                saved_widths
                    .get(&column.column.id)
                    .copied()
                    .unwrap_or_else(|| column_default_width(column))
            })
            .collect();
        // The table keeps its own focus handle for its selection model. The app
        // dispatches the `Table` key context from `table_focus` below, so the
        // shared handle is never focused and its arrow, Tab, Home and End
        // bindings stay out of the way.
        let table_focus = cx.focus_handle().tab_stop(true).tab_index(0);

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
            // The one owner of the filter's width: the wrapper paints the
            // error line below the field, so the field alone stretches it.
            .with_width(FILTER_INPUT_WIDTH)
        });

        // Everything the reader chose in an earlier session is a reader choice in
        // this one; `saved_widths` holds nothing else, so its keys are the set.
        let reader_widths: HashSet<String> = saved_widths.keys().cloned().collect();

        Self {
            spec,
            host,
            columns,
            visible_columns,
            hidden_columns,
            saved_widths,
            reader_widths,
            column_widths,
            widths_epoch: 0,
            widths_task: None,
            table: None,
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
            pending_row_reveal: None,
            context_menu: None,
            context_menu_previous_focus: None,
            context_menu_position: gpui_kit::point(px(0.), px(0.)),
            column_filter_column: None,
            preds: Vec::new(),
            query_error: None,
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
            dismissed_error: None,
            keyboard_focus: Cell::new(false),
            typeahead: Rc::new(Cell::new((String::new(), None))),
            loading_tier: LoadingTier::Nothing,
            loading_since: None,
            summary: RowSummary::default(),
            summary_generation: None,
            resolved_viewport: 0.0,
            latency_notified: false,
            latency_auto_paused: false,
            focus_handle: cx.focus_handle(),
            table_focus,
            retry_focus: cx.focus_handle(),
            error_reason_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            multi_delete: None,
            stale_retry_focus: cx.focus_handle(),
            empty_action_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            empty_namespace_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            multi_delete_confirm_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            multi_delete_cancel_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            _host_observation: host_observation,
            _width_changes: None,
        }
    }

    /// The shared table, once a frame has built it.
    fn table_state(&self) -> Option<&Entity<TableState<ResourceTableDelegate>>> {
        self.table.as_ref()
    }

    /// Builds the shared table on the first frame that has a window, and answers
    /// with it on every frame after that.
    fn ensure_table(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TableState<ResourceTableDelegate>> {
        if let Some(table) = &self.table {
            return table.clone();
        }
        let typography = DataTypography::from_theme_settings(cx);
        let delegate =
            ResourceTableDelegate::new(cx.weak_entity(), self.column_widths.clone(), typography);
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .cell_selectable(false)
                // The header draws the sort control and calls back with the
                // order it wants. `column_order` above is that order read back,
                // so the third state is the app's own default column order and
                // not the component's idea of "no sorting".
                .sortable(true)
                .col_movable(false)
        });
        // A resize drag reports the whole set of widths when it ends, and the
        // view is the only thing that can turn those into saved settings.
        let owner = cx.weak_entity();
        self._width_changes = Some(cx.subscribe(&table, move |_table, _entity, event, cx| {
            let SharedTableEvent::ColumnWidthsChanged(widths) = event else {
                return;
            };
            let Some(view) = owner.upgrade() else {
                return;
            };
            view.update(cx, |view, cx| view.note_resized_widths(widths.clone(), cx));
        }));
        self.table = Some(table.clone());
        // The shared table builds its column geometry from the delegate once,
        // when it is created, and a delegate that has not been handed the
        // visible set yet builds none of it: no header cell to click, and a
        // cell list with nothing in it, so the body painted empty rows.
        self.sync_columns(cx);
        table
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
            // The reader's own set, not the set this viewport can draw: the
            // question is whether they would be left with nothing, and a
            // column given up for width is not one they chose to lose.
            if visible_indices(&self.columns, &self.hidden_columns).len() <= 1 {
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

    /// Rebuilds the visible column set while preserving known widths.
    fn rebuild_columns_state(&mut self, cx: &mut Context<Self>) {
        let current = self.current_widths();
        self.visible_columns = visible_indices(&self.columns, &self.hidden_columns);
        if self.visible_columns.is_empty() {
            // Recover from an invalid empty visibility set.
            self.visible_columns.push(0);
            self.hidden_columns.remove(&self.columns[0].column.id);
        }
        let widths: Vec<f32> = self
            .visible_columns
            .iter()
            .map(|&index| {
                let column = &self.columns[index];
                current
                    .get(&column.column.id)
                    .copied()
                    .or_else(|| self.saved_widths.get(&column.column.id).copied())
                    .unwrap_or_else(|| column_default_width(column))
            })
            .collect();
        let fallback = self.default_sort_column();
        self.column_widths = widths;
        self.sync_columns(cx);
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

    /// Gives the table the columns a viewport of this width can hold, at the
    /// widths those columns need.
    ///
    /// Two things are decided here, and they are one decision. gpui's `Column`
    /// width is absolute, so §10.2's "flex" is a resolution pass rather than a
    /// column property; and §10.2's numeric widths — `Ready` 56, `Restarts` 64,
    /// `Age` 48 — are the width of their *values*, which is narrower than their
    /// own headers by 40 to 60px. A pass that only grew the flex column left
    /// those three at their designed widths forever, which is where `REST…` and
    /// a bare `…` came from, and it ran once on the first frame and never again,
    /// so `Node` sat at its 170 floor in a 1188px table.
    ///
    /// The order is [`fit_columns`]: a column that will not fit is given up
    /// rather than narrowed, because a header that cannot be read names nothing.
    /// It re-runs only when the reader changes a column or the window moves —
    /// [`Self::viewport_moved`] is what a frame is allowed to ask.
    fn resolve_columns(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(viewport) = self.table_viewport(cx) else {
            self.resolved_viewport = 0.0;
            return false;
        };
        self.resolved_viewport = viewport;
        // The panel's own width, recovered from the scroll viewport.
        //
        // The scroll handle is tracked on the *scrolling* part of the header band,
        // and §10.1's identifier column is drawn `fixed_left` beside it — so the
        // handle is the sticky column's width narrower than the table, and a budget
        // measured against the handle alone is a budget short by exactly that much.
        //
        // It has to be the width the sticky column is *drawn* at rather than the
        // one this function is about to compute: the policy below moves `Name`, so
        // a budget built from the incoming 252 shrinks by whatever `Name` grew,
        // the handle shrinks by the same amount on the next frame, and the two
        // chase each other — a 1920px table whose `Name` oscillated 475 → 401 → 549
        // while the total stayed pinned to a number no longer equal to the panel.
        // Measured from the *previous* frame's widths, the fixed point is exact:
        // `viewport + sticky_now` is the panel on every frame, before and after.
        let panel = viewport + self.fixed_left_width();
        // The reader's own set, not the set the last frame drew: a column given
        // up for width has to come back when the window opens again, and only
        // `hidden_columns` knows which columns the reader meant to have.
        let wanted = visible_indices(&self.columns, &self.hidden_columns);
        let protected = self.protected_columns(cx);
        let kept = fit_columns(&self.columns, &wanted, &protected, panel);
        // A reader's own width survives being given up and given back, so
        // narrowing the window and opening it again is not a silent reset. The
        // flex column's resolved width is re-resolved below either way.
        let current = self.current_widths();
        let before = (self.visible_columns.clone(), self.column_widths.clone());
        self.visible_columns = kept;
        self.column_widths = self
            .visible_columns
            .iter()
            .map(|&index| {
                let column = &self.columns[index];
                // A width is only carried forward when the reader chose it. Every
                // other column starts from the kind's designed width and is then
                // resolved by the surplus policy, so the policy reads the same
                // baseline on every launch instead of inheriting its own output
                // from the last one — which is what pinned a table whose columns
                // nobody had ever dragged to whatever width the previous window
                // size produced.
                let reader = self.reader_widths.contains(&column.column.id);
                current
                    .get(&column.column.id)
                    .copied()
                    .filter(|_| reader)
                    .or_else(|| {
                        reader
                            .then(|| self.saved_widths.get(&column.column.id).copied())
                            .flatten()
                    })
                    .unwrap_or_else(|| column_default_width(column))
                    .max(column.min_width())
            })
            .collect();
        self.resolve_surplus_width(panel, cx);
        before != (self.visible_columns.clone(), self.column_widths.clone())
    }

    /// Reports whether the viewport moved since the columns were last resolved.
    ///
    /// A window resize is the only thing a plain frame can change that the
    /// columns depend on, and re-resolving per frame would rebuild the shared
    /// table's geometry sixty times a second for a float compare.
    fn viewport_moved(&self, cx: &Context<Self>) -> bool {
        match self.table_viewport(cx) {
            Some(viewport) => (viewport - self.resolved_viewport).abs() > 0.5,
            None => self.resolved_viewport != 0.0,
        }
    }

    /// The width the columns have to fill, which is the table's own content box
    /// and not the window: the window also carries the shell's rails and the
    /// Inspector.
    fn table_viewport(&self, cx: &Context<Self>) -> Option<f32> {
        let viewport = self
            .table
            .as_ref()
            .map(|table| {
                let table = table.read(cx);
                f32::from(table.horizontal_scroll_handle.bounds().size.width)
            })
            .unwrap_or(0.0);
        (viewport.is_finite() && viewport > 0.0).then_some(viewport)
    }

    /// The width the `fixed_left` columns take out of the scroll viewport.
    fn fixed_left_width(&self) -> f32 {
        self.visible_columns
            .iter()
            .zip(self.column_widths.iter())
            .filter(|(index, _)| self.columns[**index].class == ColumnClass::Identifier)
            .map(|(_, width)| *width)
            .sum()
    }

    /// The columns the table cannot lose, whatever the viewport is.
    ///
    /// The identifier is the row's own name and the sticky column everything
    /// else is read under; the status column is where §4.4's problems filter
    /// lives; and the sorted column carries the control that says what order the
    /// rows are in, so dropping it would take the answer away with the question.
    fn protected_columns(&self, cx: &Context<Self>) -> Vec<usize> {
        let sorted = self.host.read(cx).sort().map(|sort| sort.column);
        [
            self.columns
                .iter()
                .position(|column| column.class == ColumnClass::Identifier),
            self.status_column_index(),
            sorted,
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Hands the shared table the visible column set and its widths.
    ///
    /// The table reads both from the delegate, so a change to either is a change
    /// to the columns it would build: the delegate is updated and the table
    /// re-reads it.
    fn sync_columns(&mut self, cx: &mut Context<Self>) {
        self.resolve_columns(cx);
        self.push_columns(cx);
    }

    /// Hands the flex column the room the scrollable columns leave over.
    ///
    /// The floor is the column's declared `min` and never less, because the node
    /// name is the column a reader uses to tell which machine is failing and a
    /// flex column that collapsed to 60px is a column that cannot be read.
    ///
    /// `viewport` is the scroll viewport, which does not include the sticky
    /// column, so the widths subtracted from it are the scrollable ones only. The
    /// version that subtracted all of them took 252px off a table it was then
    /// putting back — which is why `Node` sat at its floor in an 1188px table
    /// with 346px of dead space beside it.
    ///
    /// This is the *narrow* table's rule: it is reached only when the columns do
    /// not fit, and its answer is the only one available — there is no room to
    /// distribute. [`Self::resolve_surplus_width`] is what a table with room to
    /// spare does instead.
    fn resolve_flex_width(&mut self, panel: f32) {
        let viewport = panel - self.fixed_left_width();
        let Some(position) = self
            .visible_columns
            .iter()
            .position(|&index| self.columns[index].is_flex())
        else {
            return;
        };
        let min = self.columns[self.visible_columns[position]].min_width();
        let fixed: f32 = self
            .visible_columns
            .iter()
            .zip(self.column_widths.iter())
            .enumerate()
            .filter(|(at, (index, _))| {
                *at != position && self.columns[**index].class != ColumnClass::Identifier
            })
            .map(|(_, (_, width))| *width)
            .sum();
        let next = (viewport - fixed).max(min);
        if (next - self.column_widths[position]).abs() > 0.5
            && let Some(width) = self.column_widths.get_mut(position)
        {
            *width = next;
        }
    }

    /// Gives the room the table has left to the columns that can use it.
    ///
    /// The design's question is not "how wide should this table be" — it is that
    /// one, and the answer is the panel. It is *where inside it the pixels go*,
    /// and the previous answer was "all of them to the last column", because the
    /// only column the policy knew about was the one that had to take what was
    /// left. Measured at 1920 with the inspector closed: `Name` 252, `Namespace`
    /// 164, `Node` **1,030**, the row's content ending 614px short of the panel's
    /// trailing edge — a fifth of the screen with no column in it, while the one
    /// column a reader scans down shortened every long name in the table.
    ///
    /// So the surplus is resolved in two passes, and both of them ask the content
    /// rather than the panel:
    ///
    /// 1. **Every prose column takes a content-complete share first.**
    ///    [`ResourceColumn::content_width`] is the width a name, a namespace, a
    ///    node name or a status word needs to be read whole, and the pool is
    ///    shared *equally* between the columns still short of it. Sharing rather
    ///    than first-come is what makes the result the same whether the table is
    ///    30px short or 900px over: a column that would swallow the whole pool
    ///    gets its equal share and the rest of the pool keeps going, so no single
    ///    column can ever be the reason another one truncates.
    /// 2. **Whatever is still over the table is split between the two columns
    ///    that hold prose** — the identity column and the flex column — because
    ///    a clipped name is the most visible defect in the table and a 1,000px
    ///    `Node` column is the least useful place to put a pixel. A kind that
    ///    declares no flex column gives the whole residue to its identity column,
    ///    which is the same rule with one fewer participant.
    ///
    /// Two consequences are worth stating because they are the policy's contract:
    ///
    /// * **A reader's own width always wins.** Every column here is only ever
    ///   *grown*, and only up to the width its content needs; above that the
    ///   width is the reader's and nothing here moves it. Dragging a column edge
    ///   down below the width its own longest value needs does not stick across
    ///   a window resize — which is the one direction in which the table is
    ///   entitled to overrule them, because the row has to fit.
    /// * **A table with no room at all keeps today's behaviour.** A viewport
    ///   narrower than the columns' own floors has no surplus to distribute, so
    ///   it falls through to [`Self::resolve_flex_width`] and the flex column
    ///   takes the remainder, as it always has.
    ///
    /// The identity column's share is bounded by [`MAX_COLUMN_WIDTH`], which is
    /// the width the shared table clamps a non-flex column to and therefore the
    /// width beyond which the row would stop filling the panel anyway.
    fn resolve_surplus_width(&mut self, panel: f32, cx: &App) {
        let surplus = panel - self.drawn_column_width();
        if !surplus.is_finite() || surplus <= 0.0 {
            self.resolve_flex_width(panel);
            return;
        }
        let advance = f32::from(table_typography(cx).size) * CHAR_WIDTH_RATIO;
        // Pass one: the columns still short of a full value, and by how much. A
        // column the reader has placed is not in the list at all: it is either
        // already wide enough, or it is the reader's answer and this pass does not
        // get a vote.
        let mut demand: Vec<(usize, f32)> = self
            .visible_columns
            .iter()
            .zip(self.column_widths.iter())
            .enumerate()
            .filter_map(|(position, (&index, &width))| {
                let column = self.columns.get(index)?;
                if self.reader_widths.contains(&column.column.id) {
                    return None;
                }
                let shortfall = column.content_width(advance)? - width;
                (shortfall > 0.5).then_some((position, shortfall))
            })
            .collect();
        let mut pool = surplus;
        // Equal shares, re-dealt after every round in which a column's own need
        // was the smaller one. Four columns wanting 330 and a pool of 380 does
        // not become "Name takes 380"; it becomes 95 each, then whatever is left
        // goes to the two still short.
        while !demand.is_empty() {
            let share = pool / demand.len() as f32;
            let served: Vec<f32> = demand
                .iter()
                .map(|(_, need)| *need)
                .filter(|need| *need <= share)
                .collect();
            let settled = served.is_empty();
            let increments: Vec<(usize, f32)> = if settled {
                demand.iter().map(|(at, _)| (*at, share)).collect()
            } else {
                demand
                    .iter()
                    .filter(|(_, need)| *need <= share)
                    .map(|(at, need)| (*at, *need))
                    .collect()
            };
            let spent: f32 = increments.iter().map(|(_, add)| *add).sum();
            for (at, add) in increments {
                if let Some(width) = self.column_widths.get_mut(at) {
                    *width += add;
                }
            }
            pool = (pool - spent).max(0.0);
            if settled {
                break;
            }
            demand.retain(|(_, need)| *need > share);
        }
        if pool <= 0.0 {
            return;
        }
        // Pass two: the residue, split between the two prose columns.
        let identity = self
            .visible_columns
            .iter()
            .position(|&index| self.columns[index].class == ColumnClass::Identifier);
        let flex = self
            .visible_columns
            .iter()
            .position(|&index| self.columns[index].is_flex());
        let reader_owns = |at: usize| {
            self.visible_columns
                .get(at)
                .is_some_and(|&index| self.reader_widths.contains(&self.columns[index].column.id))
        };
        // A kind whose identifier is *also* its flex column is one column, not two.
        // No built-in kind declares one, and a kind that did would take the
        // residue whole and uncapped — `MAX_COLUMN_WIDTH` is a guard on a column a
        // pointer drags, and that column is the viewport.
        let shared = matches!((identity, flex), (Some(a), Some(b)) if a == b);
        let identity_room = if shared {
            pool
        } else {
            identity
                .filter(|at| !reader_owns(*at))
                .and_then(|at| self.column_widths.get(at))
                .map_or(0.0, |width| (MAX_COLUMN_WIDTH - *width).max(0.0))
        };
        // Half each, and the flex column is the whole of whatever the identity
        // column could not take — which is all of it for a kind that declares no
        // flex column, and for a reader who has already dragged `Name` as wide as
        // a column is ever drawn.
        let to_identity = if shared || flex.is_none() {
            pool.min(identity_room)
        } else {
            (pool * 0.5).min(identity_room)
        };
        // A flex column the reader has dragged is theirs too, so what it would have
        // taken goes to the identity column instead of being spent on a column this
        // pass may not move. Both branches are the same rule: the policy fills the
        // panel, and it fills it with columns it owns.
        let to_flex = if shared || flex.is_some_and(reader_owns) {
            0.0
        } else {
            pool - to_identity
        };
        if let Some(width) = identity.and_then(|at| self.column_widths.get_mut(at)) {
            *width += to_identity;
        }
        if let Some(width) = flex.and_then(|at| self.column_widths.get_mut(at)) {
            *width += to_flex;
        }
    }

    /// Writes the current columns and widths into the shared table's delegate.
    fn push_columns(&mut self, cx: &mut Context<Self>) {
        let columns = Arc::clone(&self.columns);
        let visible = self.visible_columns.clone();
        let widths: Vec<Pixels> = self.column_widths.iter().map(|w| px(*w)).collect();
        let Some(table) = self.table.clone() else {
            return;
        };
        table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.columns = columns;
            delegate.visible = visible;
            delegate.widths = widths;
            table.refresh(cx);
        });
    }

    /// The reader's row density. Every table in the app reads one value.
    pub fn density(&self, cx: &App) -> Density {
        Density::read(cx)
    }

    /// Sets the row density every table draws at.
    pub fn set_density(&mut self, density: Density, cx: &mut Context<Self>) {
        let setting = cx
            .try_global::<DensitySetting>()
            .cloned()
            .unwrap_or_else(|| DensitySetting(Rc::new(Cell::new(Density::Comfort))));
        if setting.0.get() == density {
            return;
        }
        setting.0.set(density);
        cx.set_global(setting);
        cx.notify();
    }

    /// Records the widths a resize drag produced, once the drag is over.
    ///
    /// The component's own floor is [`COLUMN_MIN_WIDTH`] for every column, which
    /// is narrower than most headers: it let a reader drag `Restarts` to 48 and
    /// get `REST…` back, in a table that says the width is theirs. The floor
    /// that belongs to a column is its own, and it is applied here so the drag
    /// and the row agree on the width in the same frame.
    fn note_resized_widths(&mut self, widths: Vec<Pixels>, cx: &mut Context<Self>) {
        let widths: Vec<Pixels> = widths
            .iter()
            .enumerate()
            .map(|(position, width)| {
                let floor = self
                    .visible_columns
                    .get(position)
                    .map(|&index| self.columns[index].min_width())
                    .unwrap_or(COLUMN_MIN_WIDTH);
                px(f32::from(*width).max(floor))
            })
            .collect();
        self.column_widths = widths.iter().map(|width| f32::from(*width)).collect();
        // A drag reports the whole set, so every column in it is the reader's from
        // here on: the width policy may not re-resolve a column a pointer has
        // already placed, or the drag snaps back on the next window resize.
        self.reader_widths.extend(
            self.visible_columns
                .iter()
                .map(|&index| self.columns[index].column.id.clone()),
        );
        if let Some(table) = self.table.clone() {
            table.update(cx, |table, _| {
                table.delegate_mut().widths = widths;
            });
        }
        self.note_column_widths(cx);
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

    /// Returns each visible column width in pixels, keyed by column ID.
    fn current_widths(&self) -> BTreeMap<String, f32> {
        self.visible_columns
            .iter()
            .copied()
            .zip(self.column_widths.iter().copied())
            .filter_map(|(index, width)| {
                let column = self.columns.get(index)?;
                Some((column.column.id.clone(), width))
            })
            .collect()
    }

    fn reveal_selected_column(&mut self, _window: &Window, cx: &mut Context<Self>) {
        let Some(position) = self
            .visible_columns
            .iter()
            .position(|&index| index == self.selected_column)
        else {
            return;
        };
        let widths = self.column_widths.clone();
        let Some(span) = column_span(&widths, position) else {
            return;
        };
        let Some(table) = self.table_state() else {
            self.pending_column_reveal = true;
            return;
        };
        let (handle, viewport) = {
            let table = table.read(cx);
            (
                table.horizontal_scroll_handle.clone(),
                f32::from(table.horizontal_scroll_handle.bounds().size.width),
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
            handle.set_offset(gpui_kit::point(px(-next), px(0.0)));
        }
        self.pending_column_reveal = false;
    }

    /// The empty state the shared table draws when it has no rows.
    ///
    /// The delegate owns the header, the rows and the loading skeleton, so it
    /// asks the view for this rather than the render path handing over the
    /// view's words, focus handles and recovery buttons once a frame.
    fn empty_state(&mut self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let status = self.host.read(cx).status();
        let snapshot = self.host.read(cx).snapshot();
        let problems_only = self.problems_only;
        let filter = self.filter.read(cx).text().to_owned();
        let spec = self.spec.clone();
        let action_focus = self.empty_action_focus.clone();
        let reason_focus = self.error_reason_focus.clone();
        let active_filters = self.active_filter_count(cx);
        empty_state(
            EmptyStateContext {
                status: &status,
                filter: &filter,
                snapshot: snapshot.as_deref(),
                spec: &spec,
                action_focus: &action_focus,
                reason_focus: &reason_focus,
                problems_only,
                active_filters,
                reason_keyboard_focus: self.keyboard_focus.get(),
            },
            window,
            cx,
        )
    }

    fn context_menu_anchor(
        &self,
        window: &Window,
        cx: &Context<Self>,
        row: Option<usize>,
    ) -> Point<Pixels> {
        let widths = self.column_widths.clone();
        let position = self
            .visible_columns
            .iter()
            .position(|&index| index == self.selected_column)
            .unwrap_or(0);
        let column_start = column_span(&widths, position)
            .map(|span| span.0)
            .unwrap_or(0.0);
        let (horizontal_bounds, vertical_bounds, horizontal_offset, vertical_offset) =
            match self.table_state() {
                Some(table) => {
                    let table = table.read(cx);
                    let horizontal = table.horizontal_scroll_handle.bounds();
                    let vertical = table.vertical_scroll_handle.0.borrow().base_handle.bounds();
                    (
                        horizontal,
                        vertical,
                        table.horizontal_scroll_handle.offset().x,
                        table
                            .vertical_scroll_handle
                            .0
                            .borrow()
                            .base_handle
                            .offset()
                            .y,
                    )
                }
                // Before the first frame there is no geometry to anchor to, so
                // the menu falls back to the toolbar's own position.
                None => (Bounds::default(), Bounds::default(), px(0.), px(0.)),
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
            // Before the shared table has geometry there is nothing to anchor to,
            // and the header menu is a *header* menu: the only band above the
            // header is the summary strip now that the pill toolbar is gone.
            f32::from(design::size::SUMMARY_STRIP) + row_pixels
        };
        let window = window.viewport_size();
        gpui_kit::point(
            px(x.clamp(f32::from(design::space::SM), f32::from(window.width))),
            px(y.clamp(f32::from(design::space::SM), f32::from(window.height))),
        )
    }

    /// Saves changed column widths after the resize debounce.
    fn note_column_widths(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        // Keep the saved width of a hidden column, and the reader's own width for
        // every visible one — and *only* those. This used to write every resolved
        // width, which turned the width policy's own arithmetic into a saved user
        // preference: the next launch read its output as the reader's, and a table
        // nobody had ever dragged stayed pinned to the last window size. What is
        // remembered is what a reader chose.
        let mut next = self.saved_widths.clone();
        for (id, width) in self.current_widths() {
            if self.reader_widths.contains(&id) {
                next.insert(id, width);
            }
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
        menu: Entity<PopupMenu>,
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
        // The rows are read before the menu is built, so the builder only has to
        // lay them out.
        let entries = self.row_menu_entries(RowTarget::Position(index), window, cx);
        let menu = build_menu(window, cx, move |menu, _, _| {
            entries.into_iter().fold(menu, PopupMenu::item)
        });
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
    ) -> Entity<PopupMenu> {
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
        // What the reader asked for and what the width allowed, named once.
        let given_up = self.columns_given_up_for_width();
        // The menu opens on one column, and the width actions act on the
        // selected one. A menu that said only `Wider` left the reader guessing
        // which column it was about, which matters as soon as the list of columns
        // is on screen too.
        let column_title = self
            .columns
            .get(index)
            .map(|column| column.title.to_owned())
            .unwrap_or_default();
        // §4.10 puts `Only problems` in the column's own filter popover, and the
        // menu keeps it reachable without a pointer. The popover is the header's
        // control, so the entry belongs wherever the Status column is at all —
        // which is why this asks about the column and not about its visibility.
        let problems_filter = self.status_column_index().is_some();
        let problems_only = self.problems_only;
        build_menu(window, cx, move |menu, window, cx| {
            // Width stays reachable without a pointer drag.
            let menu = menu
                .item(
                    menu_item(format!("Wider {column_title}"))
                        .icon(IconName::ArrowRight)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |view, cx| {
                                    view.resize_selected_column(COLUMN_WIDTH_STEP, cx)
                                })
                                .ok();
                            }
                        }),
                )
                .item(
                    menu_item(format!("Narrower {column_title}"))
                        .icon(IconName::ArrowLeft)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |view, cx| {
                                    view.resize_selected_column(-COLUMN_WIDTH_STEP, cx)
                                })
                                .ok();
                            }
                        }),
                )
                .item(
                    menu_item("Reset column widths")
                        .icon(IconName::RotateCcw)
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |view, cx| view.reset_column_widths(cx))
                                    .ok();
                            }
                        }),
                )
                // Three unrelated groups in one flat list read as one list.
                .separator();
            let menu = if problems_filter {
                menu.item(
                    menu_item("Show only problems")
                        .checked(problems_only)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                view.update(cx, |view, cx| view.toggle_problems_filter(window, cx))
                                    .ok();
                            }
                        }),
                )
            } else {
                menu
            };
            let columns_view = view.clone();
            menu.separator()
                .submenu("Columns", window, cx, move |menu, _, _| {
                    let mut menu = menu;
                    // A checked box next to a column that is not on screen is the
                    // same lie as a header that reads `…`: the reader is told the
                    // table has a column it does not have. The check keeps saying
                    // what the reader chose — unchecking it is a real choice, not a
                    // dead control — and this line says which of those choices the
                    // current width cannot honour.
                    if let Some(given_up) = &given_up {
                        menu = menu.item(menu_item(given_up.clone()).disabled(true));
                    }
                    for (index, title, id) in &columns {
                        let view = columns_view.clone();
                        let index = *index;
                        let toggled = !hidden.contains(id);
                        menu = menu.item(menu_item(title.clone()).checked(toggled).on_click(
                            move |_, _, cx| {
                                view.update(cx, |view, cx| {
                                    view.toggle_column_visibility(index, cx)
                                })
                                .ok();
                            },
                        ));
                    }
                    menu
                })
        })
    }

    /// The line the column menu shows when the width is holding columns back.
    ///
    /// `None` when the table is showing everything the reader asked for, which is
    /// the only case where the line has nothing to say.
    fn columns_given_up_for_width(&self) -> Option<SharedString> {
        let wanted = visible_indices(&self.columns, &self.hidden_columns);
        let given_up = wanted.len() - self.visible_columns.len();
        (given_up > 0)
            .then(|| SharedString::from(format!("{given_up} hidden until the window is wider")))
    }

    /// Puts every column back to the width it starts at.
    fn reset_column_widths(&mut self, cx: &mut Context<Self>) {
        let widths: Vec<f32> = self
            .visible_columns
            .iter()
            .map(|&index| column_default_width(&self.columns[index]))
            .collect();
        self.column_widths = widths;
        self.reader_widths.clear();
        self.sync_columns(cx);
        self.saved_widths.clear();
        if !cfg!(test) {
            match crate::settings::set_column_widths(cx, self.spec.kind.as_ref(), BTreeMap::new()) {
                Ok(()) => self.clear_column_settings_error(cx),
                Err(_) => self.notify(COLUMN_SETTINGS_ERROR.to_owned(), Severity::Error, cx),
            }
        }
        cx.notify();
    }

    /// Grows or shrinks the selected data column by one step.
    fn resize_selected_column(&mut self, delta: f32, cx: &mut Context<Self>) {
        let index = self.selected_column;
        let Some(column) = self.columns.get(index) else {
            return;
        };
        let title = column.title;
        let id = column.column.id.clone();
        let default = column_default_width(column);
        let Some(position) = self
            .visible_columns
            .iter()
            .position(|&visible| visible == index)
        else {
            return;
        };
        let current = self.current_widths().get(&id).copied().unwrap_or(default);
        // The same ceiling the shared table is handed, so the menu and a drag stop
        // in the same place. A `flex` column's is the table rather than
        // [`MAX_COLUMN_WIDTH`]: its width is the room the other columns leave, and a
        // flat 640 on it is a second, smaller budget — see
        // [`ResourceTableDelegate::column`].
        let ceiling = if column.is_flex() {
            self.drawn_column_width()
        } else {
            MAX_COLUMN_WIDTH
        };
        let next = (current + delta).clamp(column.min_width(), ceiling.max(column.min_width()));
        if (next - current).abs() < f32::EPSILON {
            self.notify(
                format!("{title} is already at its width limit."),
                Severity::Muted,
                cx,
            );
            return;
        }
        if let Some(width) = self.column_widths.get_mut(position) {
            *width = next;
        }
        self.reader_widths.insert(id.clone());
        self.sync_columns(cx);
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

    /// Returns true when the resource kind can start a Port Forward.
    fn supports_port_forward(&self) -> bool {
        FORWARDABLE_KINDS.contains(&self.spec.kind.as_ref())
    }

    /// Reads the Port Forward target for one row.
    ///
    /// `None` when the kind can never be forwarded, so the caller hides the action instead
    /// of offering one that then refuses. `Err` when the kind is forwardable but this row
    /// is not: only a Service can land there, because a Pod that declares no port still
    /// answers a typed one in the dialog, while a Service with no resolvable `targetPort`
    /// has nothing a forward could reach.
    fn port_forward_target_for_object(
        &self,
        object: &DynamicObject,
    ) -> Option<Result<PortForwardTarget, String>> {
        if !self.supports_port_forward() {
            return None;
        }
        let name = object.metadata.name.clone()?;
        let service = self.spec.kind.as_ref() == "Service";
        // Both port lists come from the forwards layer, so the rule for which ports a
        // target offers stays in one place: a Pod offers the `containerPort` values it
        // declares, a Service the `targetPort` behind each of its own ports.
        let ports = if service {
            crate::panels::forwards::service_ports(object)
        } else {
            crate::panels::forwards::container_ports(object)
        };
        if service && ports.is_empty() {
            return Some(Err(format!(
                "Service {name} declares no target port to forward. Start the forward from a Pod behind it instead."
            )));
        }
        Some(Ok(PortForwardTarget {
            namespace: object.metadata.namespace.clone(),
            name: name.into(),
            ports,
        }))
    }

    /// Requests Port Forward for the selected Pod or Service.
    pub fn request_port_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(handler) = self.on_forward_requested.clone() else {
            return;
        };
        if !self.supports_port_forward() {
            self.notify(
                "Start Port Forward is available only for Pods and Services.",
                Severity::Warning,
                cx,
            );
            return;
        }
        let Some(object) = self.selected_object(cx) else {
            // The kind is the noun, so a Service view asks for a Service and a Pod view
            // keeps asking for a Pod.
            let kind = self.spec.kind.clone();
            self.notify(
                format!("Select a {kind} before you start a port forward."),
                Severity::Warning,
                cx,
            );
            return;
        };
        match self.port_forward_target_for_object(&object) {
            Some(Ok(target)) => handler(target, window, cx),
            Some(Err(reason)) => self.notify(reason, Severity::Warning, cx),
            None => {}
        }
    }

    fn request_port_forward_target(
        &mut self,
        target: PortForwardTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_port_forward() {
            self.notify(
                "Start Port Forward is available only for Pods and Services.",
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

    fn toggle_updates_action(&mut self, cx: &mut Context<Self>) {
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

    fn toggle_churn_action(&mut self, cx: &mut Context<Self>) {
        if let Some(churn) = &self.churn {
            churn.toggle();
            cx.notify();
        }
    }

    fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Clears the whole query, which is now the only filter there is.
    ///
    /// It used to have to clear the problems flag as well, because that was a second
    /// piece of state beside the box. There is nothing left to clear: the flag is
    /// read out of the query, so emptying the query empties it.
    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter.read(cx).text().is_empty() {
            return;
        }
        self.apply_query(String::new(), Some(window), cx);
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
            inspector.apply(
                InspectorUpdate::Selection(selection, self.selected_uids.len()),
                cx,
            );
        }
    }

    fn notify_selection(&self, row: Option<Row>, cx: &mut Context<Self>) {
        if let Some(handler) = self.on_selection_changed.clone() {
            handler(row, cx);
        }
    }

    pub(crate) fn table_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.table_focus.clone()
    }

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
            //
            // It was `kept.iter().min()`, which is the LEXICOGRAPHICALLY SMALLEST
            // uid — and a uid is an opaque hash, so "smallest" means nothing a
            // reader could predict. Filtering Pods down to one removed the row
            // they were looking at and silently moved the Inspector to whatever
            // pod happened to hash lowest, which reads as the app deciding to
            // show them a different object.
            //
            // `snapshot.rows` is already the order the reader is looking at, and
            // `by_uid` resolves to a position in it, so "nearest" is a distance
            // along that order and nothing more. The row on the anchor's own side
            // wins the tie, because that is the one the eye was already moving
            // toward.
            let anchor_row = self
                .selection_anchor
                .as_deref()
                .and_then(|uid| snapshot.by_uid.get(uid))
                .copied();
            let next = kept
                .iter()
                .filter_map(|uid| {
                    let row = snapshot.by_uid.get(uid.as_ref()).copied()?;
                    let distance = anchor_row.map_or(0, |anchor| anchor.abs_diff(row));
                    Some((
                        distance,
                        anchor_row.is_some_and(|anchor| row < anchor),
                        uid.clone(),
                    ))
                })
                .min_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
                .map(|(_, _, uid)| uid);
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
        self.reveal_row(index, window, cx);
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
    ///
    /// The scroll is deferred rather than done here. The shared table builds a
    /// row's menu by calling the delegate's `context_menu` from inside its own
    /// render, so opening a row menu re-enters this path while the table entity
    /// is already being updated, and reading its scroll handle there aborts the
    /// app. The position is recorded and [`Self::reveal_pending_row`] applies it
    /// from [`Self::table`], which runs outside the table's render. Focus still
    /// moves now, because that has to land on the interaction that asked for it.
    fn reveal_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let position = self
            .host
            .read(cx)
            .snapshot()
            .map(|snapshot| self.shown_position(&snapshot, index))
            .unwrap_or(index);
        self.pending_row_reveal = Some(position);
        window.focus(&self.table_focus, cx);
        cx.notify();
    }

    /// Scrolls the row [`Self::reveal_row`] recorded into view.
    fn reveal_pending_row(&mut self, cx: &mut Context<Self>) {
        let Some(position) = self.pending_row_reveal.take() else {
            return;
        };
        // The scroll handle only exists once a frame has built the table, so a
        // selection made before the first paint keeps waiting for one.
        let Some(table) = self.table_state() else {
            self.pending_row_reveal = Some(position);
            return;
        };
        table
            .read(cx)
            .vertical_scroll_handle
            .scroll_to_item(position, ScrollStrategy::Nearest);
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
        let index = self.selected_column;
        self.cycle_column_sort(index, cx);
    }

    /// Walks one column through the three orders, from the order the table is in.
    ///
    /// One cycle for the whole table: the header's own control, `Shift+Enter` and
    /// the shared table's backstop all come through here, so a reader who has
    /// learned one route has learned all of them.
    ///
    /// There used to be two. The shared table's header owned a three-state cycle
    /// of its own and the keyboard walked a second one from the opposite end, and
    /// the two were not in phase: "ascending on the default column" reported as
    /// the component's `Ascending`, whose next step is `Default`, and `Default`
    /// means the default order — which *is* ascending on the default column. So
    /// the first click on the sorted column, the click a reader makes to see the
    /// other direction, advanced the arrow and left the rows where they were.
    fn cycle_column_sort(&mut self, index: usize, cx: &mut Context<Self>) {
        self.focus_target = FocusTarget::Column;
        self.selected_column = index;
        // The ranked order is not one of the three, so a cycle started while the
        // query box is ranking starts from the first real order.
        let current = if self.relevance_sort {
            None
        } else {
            self.host.read(cx).sort()
        };
        self.apply_sort(next_sort(current, index, self.default_sort_column()), cx);
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
    fn empty_state_action(&self, cx: &App) -> Box<dyn gpui_kit::Action> {
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
        keystroke: &gpui_kit::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.table_focus.contains_focused(window, cx) {
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
        let height = self.table_state().map_or(0., |table| {
            f32::from(
                table
                    .read(cx)
                    .vertical_scroll_handle
                    .0
                    .borrow()
                    .base_handle
                    .bounds()
                    .size
                    .height,
            )
        });
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
        // A key press is the keyboard arriving. This is the only place the origin
        // can be recorded from, and every focus ring in this file reads it.
        self.keyboard_focus.set(true);
        if self.context_menu.is_some() && self.table_focus.contains_focused(window, cx) {
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
        // `PROMPT` §2.4: Esc always does something. A multi-row selection is the
        // one piece of table state a reader can get stuck in — there is no other
        // way to put it down without clicking each row off — so Esc gives it up
        // before it does anything else. It comes after the pending multi-delete,
        // which owns Esc while it is on screen.
        if keystroke.key.as_str() == "escape" && self.selection_count() > 1 {
            self.select_only(None);
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
        if !self.table_focus.contains_focused(window, cx) {
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
            _ => self.typeahead(keystroke, window, cx),
        }
    }

    /// Jumps the selection to the first row whose name starts with what the
    /// reader is typing.
    ///
    /// `PROMPT` §2.4 and `UI-SPEC` §9.3 both list this, and it is a native list
    /// behaviour rather than a feature: a `File > Open` list, a Finder list and
    /// every mail client jump to a name as you type it, and a table of 10,000
    /// resources that cannot is a table a reader has to scroll. A reader types the
    /// workload — `core`, `api`, `web` — so the match is per name segment; see
    /// [`name_matches_prefix`].
    ///
    /// The prefix expires after [`TYPEAHEAD_RESET`], which is what makes
    /// `c`, `o`, `r` spell `cor` while `c`, pause, `c` starts a new `c`. A
    /// second character matching a *different* row of the first one's matches
    /// extends the search rather than restarting it, which is what native lists
    /// do and what makes a repeated prefix (`coredns-`, `coredns-canary-`)
    /// reachable at all.
    fn typeahead(
        &mut self,
        keystroke: &gpui_kit::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only a plain printable character. A modified keystroke is a shortcut,
        // and a shortcut whose name happens to be a letter is not type-ahead.
        if keystroke.modifiers.control || keystroke.modifiers.platform || keystroke.modifiers.alt {
            return;
        }
        let Some(character) = keystroke.key.as_str().chars().next() else {
            return;
        };
        if !character.is_alphanumeric() && !matches!(character, '-' | '_' | '.') {
            return;
        }
        let now = cx.background_executor().now();
        let prefix = {
            let cell = self.typeahead.clone();
            let (prefix, at) = cell.take();
            match at {
                Some(at) if now.duration_since(at) <= TYPEAHEAD_RESET => prefix,
                _ => String::new(),
            }
        };
        let prefix = prefix + &keystroke.key.as_str().to_lowercase();
        self.typeahead.set((prefix.clone(), Some(now)));
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        // The search wraps and starts from the row after the current one, so
        // typing the same two letters twice walks down a run of matches rather
        // than sitting on the first of them. With nothing selected the search
        // starts at the top: the first keystroke on a fresh table is the common
        // case, and a type-ahead that only works once something is already
        // selected is not a type-ahead.
        let shown = self.shown_rows(&snapshot);
        if shown.is_empty() {
            return;
        }
        let current = self
            .selected_index(&snapshot)
            .and_then(|index| shown.iter().position(|&shown| shown == index))
            .map_or(0, |position| position + 1);
        for step in 0..shown.len() {
            let position = (current + step) % shown.len();
            let Some(row) = snapshot.rows.get(shown[position]) else {
                continue;
            };
            let matches = row
                .obj
                .metadata
                .name
                .as_deref()
                .is_some_and(|name| name_matches_prefix(name, &prefix));
            if matches {
                self.select_index(shown[position], window, cx);
                return;
            }
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

    /// Replaces the query and applies it now, without the typing debounce.
    ///
    /// The popover goes through here rather than through the input's change
    /// handler, so a click filters on the frame it happened. A reader who ticks
    /// two values expects the counts under the second one to be the counts of what
    /// the first one left, and a 300ms wait in between shows them the previous
    /// table instead.
    ///
    /// The value is the *whole* query, not a delta: the box is the one place the
    /// query lives, and a popover that kept its own copy beside it is the two
    /// sources of truth the design warns about.
    fn apply_query(&mut self, text: String, window: Option<&mut Window>, cx: &mut Context<Self>) {
        // A pending debounce would commit the string this write replaces, so it is
        // cancelled rather than left to fight the write.
        self.filter_epoch
            .set(self.filter_epoch.get().wrapping_add(1));
        self.filter_task = None;
        // The field is written *without* its change handler, because this function
        // is the commit: letting the handler fire would schedule a second commit
        // from inside this one, and re-enter the view that is running it.
        let filter = self.filter.clone();
        match window {
            Some(window) => {
                filter.update(cx, |input, cx| {
                    input.set_text_applied(text.clone(), window, cx)
                });
            }
            None => filter.update(cx, |input, cx| {
                input.set_text_applied_pending(text.clone(), cx)
            }),
        }
        self.commit_filter(&text, cx);
        cx.notify();
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

    /// Turns the query box's text into a filter and hands it to the host.
    ///
    /// This is the *only* place a query string becomes a `Filter`, and both entry
    /// points come through it: the reader typing, and the column-header popover
    /// writing the same string back into the box. A parse failure keeps the last
    /// filter that worked and reports the offending token, because a filter built
    /// from half a clause would drop rows the reader can see were not asked for.
    fn commit_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter_pending = false;
        let filter = match Filter::parse(text) {
            Ok(filter) => {
                self.query_error = None;
                filter
            }
            Err(error) => {
                self.query_error = Some(error);
                return;
            }
        };
        // Relevance ranking only means something for a text search. `ns=prod` is
        // an exact set, so ranking its rows by name would be answering a question
        // nobody asked.
        let ranks = filter.search_needle().is_some();
        let was_relevance = self.relevance_sort;
        let active = !filter.is_empty();
        if ranks && !self.filter_active {
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
        self.preds = filter.preds.clone();
        // The problems clause is one clause of the query like any other, so whether
        // it is on is a fact about the query rather than a flag of its own. It is
        // read off the text because the clause's identity *is* its text: teaching
        // `Pred` a spelling for it would be a second way to say the same thing.
        self.problems_only = has_problems_clause(text);
        self.host.update(cx, |host, cx| {
            host.set_relevance(self.relevance_sort);
            if let Some(sort) = restore_sort {
                host.dispatch(TableEvent::SortChanged { sort: Some(sort) }, cx);
            }
            host.dispatch(TableEvent::FilterChanged { filter }, cx)
        });
    }

    /// Walks the reader up the loading ladder, one timer per rung.
    ///
    /// Each task only ever moves the tier *up*: a late task cannot take a reader
    /// back to a spinner they have already been shown a skeleton for, which is
    /// the one way a timer-driven ladder can look worse than a clock comparison.
    ///
    /// The first rung is a timer like the others, not an assignment. `UI-SPEC`
    /// §4.14's first row is "under 200ms: show nothing at all", and a list that
    /// resolved in 40ms was drawing a spinner for the whole of its 40ms — a flash,
    /// which reads as a glitch rather than as speed. The comment here used to claim
    /// the opposite ("the first rung is entered immediately … a spinner that
    /// arrives 200ms after the wait began is a spinner that appeared after the
    /// moment it was for") and the line under it set the tier straight to
    /// `Spinner`, so [`LOADING_INVISIBLE`] had no call site at all. Showing nothing
    /// is not a spinner that arrives late; it is no spinner.
    fn advance_loading_tier(&mut self, cx: &mut Context<Self>) {
        self.loading_tier = LoadingTier::Nothing;
        for (delay, target) in [
            (LOADING_INVISIBLE, LoadingTier::Spinner),
            (SKELETON_AFTER, LoadingTier::Skeleton),
            (LOADING_PROGRESS, LoadingTier::Progress),
        ] {
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(delay).await;
                view.update(cx, |view, cx| {
                    if view.loading_tier < target {
                        view.loading_tier = target;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Counts the listed rows by severity, once per snapshot generation.
    ///
    /// This is the one O(n) pass in a virtualized table, so it is memoised on
    /// `IndexSnapshot::generation` rather than run per frame. A per-frame pass
    /// over 10,000 rows is 10,000 `Severity` matches sixty times a second for a
    /// number that changes when the cluster changes — and if the generation were
    /// not bumped reliably the strip would show a count that does not match the
    /// rows, which is worse than having no strip at all.
    fn refresh_summary(&mut self, snapshot: &Option<Arc<IndexSnapshot>>, shown: &[usize]) {
        let generation = snapshot.as_ref().map(|snapshot| snapshot.generation);
        if self.summary_generation == generation {
            return;
        }
        self.summary_generation = generation;
        let Some(snapshot) = snapshot else {
            self.summary = RowSummary::default();
            return;
        };
        let Some(status_index) = self
            .columns
            .iter()
            .position(|column| column.column.id == "status")
        else {
            // A kind with no status column has no verdict to tally. Saying
            // "10,010 healthy" about a table that never reported a status would
            // be inventing a fact.
            self.summary = RowSummary {
                healthy: shown.len(),
                ..RowSummary::default()
            };
            return;
        };
        let mut summary = RowSummary::default();
        for &index in shown {
            let Some(row) = snapshot.rows.get(index) else {
                continue;
            };
            let Some(cell) = row.cells.get(status_index) else {
                continue;
            };
            let status = cell.text.trim();
            summary.record(
                matches!(status, "Pending" | "ContainerCreating"),
                status_severity(status, row_age(row)),
            );
        }
        self.summary = summary;
    }

    /// The table's own first line: how much of the scope is here, and how much of
    /// it is wrong.
    ///
    /// `UI-REDESIGN` §2 names this one of the product's three signature elements,
    /// and it is the only place in the interface that answers "is this cluster
    /// fine?" without the reader reading 10,000 status cells. A count, a shape,
    /// and a breakdown — and the breakdown is the point, because a proportion of
    /// what is broken is more useful than a proportion of what is whole.
    ///
    /// **It is the header band's first line, not a band of its own.** The two used
    /// to touch without belonging to each other: a 32px strip, a 32px header, and
    /// nothing that said they were one thing — two boxes stacked, which the eye
    /// reads as a list rather than as a header. Four things make them one 64px
    /// block, and all four are shared rather than coincidental:
    ///
    /// * **One plane.** The strip paints the table's own content surface
    ///   explicitly, the same value the header band's per-column fill neutralises
    ///   the component's `table_head` token to, so there is nothing between them
    ///   for the eye to stop on and nothing for a scrollbar to catch on.
    /// * **One inset.** [`TABLE_CONTENT_INSET`], which is the value every body
    ///   cell is padded by. The summary's first word and the `NAME` label and the
    ///   first cell's value are on one leading edge by construction.
    /// * **One centre line.** The strip's content box is
    ///   [`header_cell_height`], not `SUMMARY_STRIP`: the header's box is the
    ///   1px rule short of its band because the rule belongs to the band, and a
    ///   32px box beside a 31px one puts the two lines half a pixel apart for no
    ///   reason a reader could name. Same box, same `items_center`, one line.
    /// * **No rule between them.** The one hairline in this stack is the header's
    ///   own bottom rule, and it is the header's: the strip draws none, and the
    ///   per-column fill rects stop short of that rule so it is the only thing
    ///   between the header and the rows.
    ///
    /// ### One thing may be loud
    ///
    /// The band used to carry two coloured words and a third one that could be
    /// either: `2 rows in error` in red at medium weight, `1 pending` in amber at
    /// medium weight, `17 rows healthy` in grey. Emphasis is a budget and the
    /// band was spending it three ways, so nothing in it was emphatic — a reader
    /// scanning for "is anything broken" had to read all three figures before
    /// either one answered.
    ///
    /// So: **the error count is the fact and the only loud one.** It is the only
    /// figure at `text::MEDIUM`, and it is the only one wearing a status channel.
    /// Healthy is metadata in `fg.tertiary`; *pending* is metadata in
    /// `fg.secondary` and is deliberately **not** graded — the pending bucket's
    /// severity is the one thing the mark already draws, so grading the
    /// word as well says the same thing twice, and says it in the channel the
    /// guide reserves for a signal rather than for a confirmation. Attention is
    /// not spent twice on one fact.
    ///
    /// All four words wear a word role (`role::status_word_for` for the two that
    /// carry a channel, the ink ladder for the two that do not) and all four mark
    /// segments wear a mark role (`role::status_for`). The design system's own
    /// distinction is the whole reason the two sets exist: a 12px word is solved
    /// to clear the text floor, a 4px segment to clear the graphic one, and the
    /// strip reads correctly for a reader who cannot tell the hues apart at all.
    ///
    /// Everything in the band shares one baseline and one gap. The mark is a flex
    /// item of the same row as the two text runs, on the same `items_center` line
    /// and the same `text::LABEL` line box, so it sits on their centre line rather
    /// than on a rule of its own; and the two gaps in the band — strip-level and
    /// between the figures — are `space::SM` and `space::XS` for one stated
    /// reason: the figures are separated by a `·` glyph that *is* the gap, so they
    /// need less space around it than the mark, which is a solid object. One
    /// value per relationship, and the relationship is named.
    ///
    /// It also renders the age grade: the `pending` bucket is split into the pods
    /// that have only just been scheduled and the ones that are stuck, which is
    /// §4.14's stale-data rule applied to a status rather than to a cache — and it
    /// is the mark's own segment that carries the grade, not the word.
    fn summary_strip(
        &self,
        status: &TableStatus,
        shown: usize,
        total: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let plural = self.spec.label_lower();
        // A primed cache paints a thousand rows while the machine is still
        // `Listing`, and a band that says `pods –` over them has decided they are
        // not there.
        let listing = matches!(status, TableStatus::Listing) && shown == 0;
        let filtered = !listing && shown < total;
        let count = shown;
        let count_label = if listing {
            format!("{plural} \u{2013}")
        } else if filtered {
            // The visible half of a sentence the strip only used to give a screen
            // reader. Eight rows out of ten thousand labelled `8 pods` reads as an
            // eight-pod cluster, and the reader who believes it is wrong about the
            // only fact the table was opened for.
            format!(
                "{} of {} {plural}",
                design::format::count(shown),
                design::format::count(total)
            )
        } else {
            format!("{} {plural}", design::format::count(count))
        };
        let mut description = if listing {
            format!("Loading {plural}.")
        } else if filtered {
            format!(
                "{} of {} {plural} match the current filters.",
                design::format::count(shown),
                design::format::count(total)
            )
        } else {
            format!("{} {plural} in this scope.", design::format::count(count))
        };
        let summary = self.summary;
        // The breakdown, one span per fact, each carrying its own ink. The figures
        // are a *partition* of the rows rather than a ranking of them, so they keep
        // the severity order that makes them add up; what says which one is the news
        // is its weight, not its position, and moving the error count to the front
        // would have cost the partition to buy a thing the ink already says.
        //
        // Two rules, and they are the whole emphasis budget:
        //
        // * **A word wears a word role.** The design system's contract is that a
        //   bare status role is a *graphic mark* and the `_word` role is the one
        //   solved to clear the text floor, and these are 12px words on the first
        //   line a reader reads. `role::status_word_for` is the one mapping, so the
        //   two coloured figures cannot be spelled as two channels by hand and
        //   disagree with a third surface that spells them differently.
        // * **Exactly one figure is loud.** The error count is the fact; healthy
        //   and pending are metadata, and pending is metadata *ungraded* — its
        //   grade is the one thing the mark already draws, so a coloured
        //   word beside a coloured segment says the same thing twice and spends
        //   the emphasis budget twice on one population.
        let mut parts: Vec<(SharedString, Hsla, bool)> = Vec::with_capacity(4);
        if !listing && summary.total() > 0 {
            if summary.healthy > 0 {
                parts.push((
                    SharedString::from(design::format::count_with_noun(
                        summary.healthy,
                        "row healthy",
                        "rows healthy",
                    )),
                    // Tertiary, and never a colour: a green "10,000 healthy" is a
                    // second thing shouting on a screen whose whole argument is
                    // that health is the absence of a signal.
                    design::role::fg_tertiary(cx),
                    false,
                ));
            }
            if summary.warning > 0 {
                parts.push((
                    SharedString::from(design::format::count_with_noun(
                        summary.warning,
                        "row needs attention",
                        "rows need attention",
                    )),
                    // Needs attention is a fault, so it wears its channel — at
                    // regular weight, because it is not the news. A reader who has
                    // to ask which of two coloured figures to act on has been
                    // given a question instead of an answer.
                    design::role::status_word_for(Severity::Warning, cx),
                    false,
                ));
            }
            if summary.danger > 0 {
                parts.push((
                    SharedString::from(design::format::count_with_noun(
                        summary.danger,
                        "row in error",
                        "rows in error",
                    )),
                    design::role::status_word_for(Severity::Error, cx),
                    true,
                ));
            }
            // Pending is a bucket, not a note across the three above it, so the
            // four figures are a partition and add up to the row count. Its *word*
            // is metadata: the grade those rows reached is carried by the mark's
            // own pending segment, which is a mark wearing a mark role, so a
            // cluster that just scheduled 9,900 pods and one that has had 9,900
            // stuck for five minutes are still told apart — by the segment, where a
            // distribution belongs, rather than by a second coloured word.
            if summary.pending > 0 {
                parts.push((
                    SharedString::from(format!(
                        "{} pending",
                        design::format::count(summary.pending)
                    )),
                    design::role::fg_secondary(cx),
                    false,
                ));
            }
        }
        // The strip is a `Role::Status`, so this is what a screen reader hears
        // when the counts move. The scope sentence alone says nothing new when
        // the cluster gets worse, which makes a live region that only ever
        // announces its own constancy.
        for (text, _, _) in &parts {
            description.push_str(". ");
            description.push_str(text);
        }
        let rail = rail_widths(&summary);
        // Four mark inks for four buckets, and the healthy one is `role::success`
        // rather than grey.
        //
        // This is the one place in the file where a *mark* wears a success colour,
        // and it is the exception that makes the mark a distribution. §0 铁律三
        // says a healthy resource is grey and only a problem takes a colour, and
        // that rule is about the reader's attention: a green "10,000 healthy" is
        // shouting, and a green dot beside every `Running` cell in a 10,000-row
        // table is worse. Inside a proportion it is the opposite problem. A
        // distribution whose healthy segment is grey beside three coloured ones
        // has one unlabelled part, so "18 of 20 rows are fine" and "the other 2
        // are the red I can see" both failed to say anything and the reader was
        // left guessing at the two pixels of grey. A mark inside a proportion
        // carries a category; a number carries news. That is why the healthy
        // *figure* beside it stays `fg.tertiary` and only the healthy *segment* is
        // a colour — and why the segment is four pixels rather than six, because
        // a category is not a claim on the reader's attention.
        let inks = [
            ("table-summary-rail-healthy", design::role::success(cx)),
            ("table-summary-rail-warning", design::role::warning(cx)),
            ("table-summary-rail-danger", design::role::danger(cx)),
            (
                "table-summary-rail-pending",
                design::role::status_for(summary.pending_severity.unwrap_or(Severity::Success), cx),
            ),
        ];
        let stale = matches!(status, TableStatus::Stale(_));
        // Whether the rows on screen came off the disk, how long ago, and how long
        // the watch has been silent. None of this used to reach the screen at all:
        // `TableStatus::Stale` carried a reason and no clock, and the cache
        // carried a save time and nothing else, so rows written to disk at startup
        // were indistinguishable from live ones and a watch that had been dead for
        // an hour was indistinguishable from one that had just hiccuped. A reader
        // about to scale or restart something needs to know which one they are
        // looking at, and the strip is the one line they read either way.
        let (cache_age, cache_past_ttl, stale_age) = {
            let host = self.host.read(cx);
            (
                host.cache_age(),
                host.cached().is_some_and(|cached| cached.stale),
                host.stale_age(),
            )
        };
        // The live region has to say it too: the words on the band are the ones a
        // screen reader is never handed, and "8 pods" is exactly the claim that
        // needs correcting when the rows are twenty minutes old.
        if let Some(age) = cache_age {
            description.push_str(&format!(
                ". These rows were last read from disk {} ago.",
                design::format::age(age.as_secs())
            ));
        }
        if let Some(age) = stale_age {
            description.push_str(&format!(
                ". Live updates stopped {} ago.",
                design::format::age(age.as_secs())
            ));
        }
        // The band and the header band below it are one 64px block, so the strip
        // owns two things the rest of the stack already owns: the table's own
        // surface (stated, not inherited — the header band neutralises the
        // component's `table_head` token to this same value one line down, and
        // "the same value" has to be true of something) and the content inset.
        h_flex()
            .id("table-summary")
            .debug_selector(|| "table-summary".to_owned())
            .role(Role::Status)
            .aria_label(description)
            .w_full()
            .flex_none()
            .h(design::size::SUMMARY_STRIP)
            .bg(table_row_surface(cx))
            // The content box hangs from the band's top edge rather than filling
            // it, and that is the only way the two lines can share a centre. The
            // header's own box is `TABLE_HEADER - 1px` because the hairline under
            // it belongs to the band; this box is the same height, so `20 pods`
            // and `NAME ⌃` are on one line instead of half a pixel apart.
            .items_start()
            .child(
                h_flex()
                    .w_full()
                    .h(header_cell_height())
                    .px(TABLE_CONTENT_INSET)
                    .gap(design::space::SM)
                    .items_center()
                    .font(ui_font(cx))
                    .text_size(design::text::LABEL)
                    .line_height(design::text::LABEL_LINE_HEIGHT)
                    // The scope, and nothing else. There is no status mark on it any more
                    // because a mark on a *count* is a claim about the rows behind it: the
                    // old strip put a red dot next to `10,010 pods`, and a red dot next
                    // to a number reads as "10,010 broken", which is a different and wrong
                    // claim about a table that has 110 healthy rows in it. The mark now
                    // appears only on a figure that *is* a fault, and a cluster with
                    // nothing wrong has no mark anywhere in the band.
                    .child(
                        div()
                            .debug_selector(|| "table-summary-count".to_owned())
                            .flex_none()
                            .text_color(design::role::fg_secondary(cx))
                            .child(SharedString::from(count_label)),
                    )
                    .when(!listing && summary.total() > 0, |strip| {
                        strip.child(
                            // A proportion mark, not a bar and not a rule with ticks on it.
                            //
                            // It has been three things. A 3px rule on a `border.subtle`
                            // track, which is a border; a 6px bar on a `surface.inset`
                            // track, which is worse — the track is four steps below the
                            // strip it sits on, so every gap between two segments read as
                            // a black slot cut through the line rather than as air, and
                            // eighty pixels of solid `status.success` made the mark the
                            // second loudest thing in the band's own row. It is a mark
                            // now: four pixels of [`SUMMARY_MICROBAR_HEIGHT`], no track
                            // at all, and a real [`design::space::XXS`] of the strip's
                            // own surface between every pair of segments.
                            //
                            // The track is gone rather than repainted because the
                            // Overview's own meters rejected `surface.inset` for the same
                            // reason — a meter drawn on it reads as a hole in the surface
                            // rather than as the whole a share is a share of — and the
                            // lane this mark needs is not a container but a separation,
                            // which a gap already is. Every bucket that is present still
                            // wears its own mark ink, so all four parts of the
                            // distribution stay decodable, and the figures keep the
                            // quieter inks: a *number* is not an alert, and a mark is not
                            // a word.
                            h_flex()
                                .id("table-summary-rail")
                                .debug_selector(|| "table-summary-rail".to_owned())
                                .flex_none()
                                .w(SUMMARY_MICROBAR_WIDTH)
                                .h(SUMMARY_MICROBAR_HEIGHT)
                                .gap(design::space::XXS)
                                // The radius belongs to the lane, and the lane clips, so
                                // the first and last segments get it for free — including
                                // the case where one segment fills 88% of the width. The
                                // clip stays with nothing behind it for the same reason
                                // the radius stays: the segment widths are a budget
                                // against this lane, and a bucket the arithmetic
                                // over-runs is a bucket that must not push the figures
                                // beside it along.
                                .rounded(SUMMARY_MICROBAR_RADIUS)
                                .overflow_hidden()
                                .children(
                                    inks.into_iter()
                                        .zip(rail)
                                        .filter(|&((_, _), width)| width > 0.0)
                                        .map(|((name, ink), width)| {
                                            div()
                                                .debug_selector(move || name.to_owned())
                                                .h_full()
                                                .w(px(width))
                                                .bg(ink)
                                        }),
                                ),
                        )
                    })
                    .when(!parts.is_empty(), |strip| {
                        // One line at every width, always: the spans never wrap, the
                        // group is allowed to shrink to nothing, and only the last figure
                        // gives way — the scope, the rail and the leading failure stay
                        // readable down to the narrowest table the layout supports.
                        let last = parts.len() - 1;
                        let mut row = h_flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap(design::space::XS)
                            .whitespace_nowrap()
                            .overflow_hidden();
                        for (index, (text, ink, emphasised)) in parts.into_iter().enumerate() {
                            if index > 0 {
                                row = row.child(
                                    div()
                                        .flex_none()
                                        // `fg.tertiary`, not `fg.disabled`. `fg.disabled` is
                                        // the ink for text a control cannot act on, and a
                                        // `·` between two figures is not that — so it drew
                                        // in the faintest ink on screen and the four
                                        // figures read as four separate sentences with a
                                        // smudge between them rather than as one line
                                        // about one population.
                                        .text_color(design::role::fg_tertiary(cx))
                                        .child(SharedString::from("\u{b7}")),
                                );
                            }
                            let mut part = div().text_color(ink).child(text).whitespace_nowrap();
                            if emphasised {
                                part = part.font_weight(design::text::MEDIUM);
                            }
                            row = row.child(if index == last {
                                part.min_w(px(0.0)).text_ellipsis().flex_shrink(1.0)
                            } else {
                                part.flex_none()
                            });
                        }
                        strip.child(row)
                    })
                    // §4.14's "保留旧数据 + 4% warning 底 wash + 表头标 Stale", and the
                    // age the rule asks for. The comment that used to sit here said the
                    // age "is not available here" and sent it to the title bar; that was
                    // true of `TableStatus`, which carries a reason and not a clock, and
                    // it was used as a reason not to know. The host times the failure
                    // itself, so the strip can say how long the rows have been frozen —
                    // and a reader deciding whether to trust a row is better served by
                    // the band under their cursor than by a dot two hundred pixels away.
                    //
                    // A *word*, so it wears the word role: the same 12px sentence as the
                    // figures beside it, solved to the text floor with them. The bar
                    // keeps the mark role for the same reason it does everywhere else.
                    .when(stale, |strip| {
                        strip.child(
                            div()
                                .flex_none()
                                .text_color(design::role::status_word_for(Severity::Warning, cx))
                                .child(freshness_word("Stale data", stale_age)),
                        )
                    })
                    // Rows read off the disk at startup wear the same band. A cache
                    // older than the TTL says so in the warning channel, because past
                    // that point it is not a fast start — it is a list of objects that
                    // may no longer exist, and it has to be graded like one.
                    .when(cache_age.is_some(), |strip| {
                        let ink = if cache_past_ttl {
                            design::role::status_word_for(Severity::Warning, cx)
                        } else {
                            design::role::fg_secondary(cx)
                        };
                        strip.child(
                            div()
                                .flex_none()
                                .text_color(ink)
                                .child(freshness_word("From cache", cache_age)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Records that the keyboard, not the pointer, is driving the table.
    ///
    /// The arrow keys and every other table action are *key bindings*, not
    /// `on_key_down`, so a listener that only set the origin flag there would
    /// leave it false for exactly the keystrokes a reader uses most. Every
    /// `on_action` on the grid calls this first, which is the one place a bound
    /// key is observable.
    fn note_keyboard(&mut self) {
        self.keyboard_focus.set(true);
    }

    /// The query input the resource header mounts.
    ///
    /// `UI-REDESIGN` D16 deleted the five-pill toolbar, and `UI-SPEC` §4.2 puts
    /// the query box on the *right* of the [`design::size::RESOURCE_HEADER`] band —
    /// which belongs
    /// to `shell/panels.rs` and is Wave 2's. The input therefore has no button
    /// in this file any more, and deleting the entity would have deleted the
    /// filter with it: every debounced predicate, the problems count, and the
    /// `FocusFilter` action all read it.
    ///
    /// What Wave 2 needs is a 240×28 box it can mount in the header's right
    /// group, plus the two actions it already has: `FocusFilter` focuses it and
    /// `ClearFilter` empties it. [`Self::filter_input`] is the element and
    /// [`Self::has_active_filter`] is what the header's badge should read, so
    /// neither the caller nor this file has to know how the input is built.
    /// The 240x28 query box, for the resource header to mount.
    ///
    /// A read-only builder: the box is a view of a field the view already owns,
    /// and the header is not the view. Typing goes to the field, not here.
    ///
    /// The box and its error are one element, because the error belongs *under* the
    /// box and a caller that mounted them separately would have to know the box's
    /// height to place the second one. It is positioned rather than stacked so the
    /// header's own [`design::size::RESOURCE_HEADER`] band does not grow: the
    /// header is a fixed-height row, and a filter that made the whole app's title
    /// area taller on a typo would be a worse mistake than the one being reported.
    pub fn filter_input(&self, cx: &App) -> AnyElement {
        let focus_filter = action_tooltip("Focus resource filter", &FocusFilter, cx);
        let mut action = div()
            .id("table-filter-action")
            .relative()
            .flex_none()
            .w(FILTER_INPUT_WIDTH)
            .h(design::size::CONTROL)
            .child(self.filter.clone());
        if let Some(error) = &self.query_error {
            action = action.child(query_error_line(error, cx));
        }
        action.interactivity().tooltip(text_tooltip(focus_filter));
        action.into_any_element()
    }

    /// What this view is looking at: the kind, its label, and whether it is
    /// namespaced.
    ///
    /// The resource header reads it to title itself. Without it the header has
    /// to take the kind from the tab, which is a second source for one fact.
    pub fn resource_spec(&self) -> crate::session::ResourceSpec {
        self.spec.clone()
    }

    /// Whether live updates are paused.
    ///
    /// The header's `⋯` menu says `Resume` or `Pause` from this, and the two
    /// have to agree: a menu that offers "Resume" while the table is streaming
    /// is a menu that lies about the one thing it exists to change.
    pub fn updates_paused(&self, cx: &App) -> bool {
        matches!(self.host.read(cx).status(), TableStatus::Paused)
    }

    /// Whether the test-update generator is running.
    ///
    /// It is a development tool with no place in the chrome, and `UI-REDESIGN`
    /// D16 puts it behind the overflow menu rather than on a button beside the
    /// reader's data.
    pub fn churn_enabled(&self) -> bool {
        self.churn.as_ref().is_some_and(|churn| churn.is_enabled())
    }

    /// Pause or resume live updates, whichever the table is not doing.
    pub fn toggle_updates(&mut self, cx: &mut Context<Self>) {
        self.toggle_updates_action(cx)
    }

    /// Turn the test-update generator on or off.
    pub fn toggle_churn(&mut self, cx: &mut Context<Self>) {
        self.toggle_churn_action(cx)
    }

    /// Reports whether a query or the status filter is hiding rows.
    ///
    /// The resource header's filter box needs it to show its own pending state
    /// and the empty state needs it to say how many filters are active, and both
    /// have to agree: a filter that is on and a filter the interface claims is
    /// on are the same filter, and only one function can be right about it.
    pub fn has_active_filter(&self, cx: &App) -> bool {
        self.active_filter_count(cx) > 0
    }

    /// How many filters are hiding rows right now.
    ///
    /// `UI-SPEC` §4.13 requires the "被筛掉了" empty state to say the *number*,
    /// because "no results" without it reads as an empty cluster and sends a
    /// reader to the wrong place to fix it. The number is the number of *clauses*,
    /// which is what a reader can count in the box — `ns=prod restarts>3` is two,
    /// and calling it one would not match what they see. A namespace the caller
    /// scoped the view to is a scope rather than a filter, so it is not counted: a
    /// reader who chose a namespace is not confused by it.
    fn active_filter_count(&self, _cx: &App) -> usize {
        self.preds.len()
    }

    /// `shown` is the list of row indices the body actually renders, so the
    /// summary strip's count and the body cannot be derived from two different
    /// filters.
    fn table(
        &mut self,
        snapshot: Option<Arc<IndexSnapshot>>,
        shown: Arc<Vec<usize>>,
        status: &TableStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Built first, so every width, scroll offset and reveal below measures
        // against a table that exists.
        let table = self.ensure_table(window, cx);
        // The columns are a function of the viewport, and the viewport changes
        // when the reader resizes the window — which is the one thing that
        // decides whether the table can hold §10.2's row or has to give columns
        // up. `ensure_table` resolved against the *previous* frame's width, so
        // this is where a resize is answered; it is a no-op when nothing moved.
        if self.viewport_moved(cx) && self.resolve_columns(cx) {
            self.push_columns(cx);
        }
        let progress = self.host.read(cx).total_count();
        let table_focus = self.table_focus_handle(cx);
        let table_focused = table_focus.contains_focused(window, cx);
        // The problems filter can hide rows, so the list maps positions to rows.
        let row_count = shown.len();
        if self.pending_column_reveal {
            self.reveal_selected_column(window, cx);
        }
        self.reveal_pending_row(cx);
        self.note_column_widths(cx);
        let (sort, relevance) = {
            let host = self.host.read(cx);
            (host.sort(), host.relevance())
        };
        // Freshness is a property of the table, so every row it still draws
        // inherits it rather than each row re-deciding.
        let stale = matches!(status, TableStatus::Stale(_));
        // Everything the body draws comes from one snapshot of the view's state,
        // so a row, its cells, the empty state and the toolbar's count cannot
        // disagree about which row is where. The view is only read, so the table
        // can be written without the two borrows overlapping. The delegate holds
        // the status too, because the shared table asks it whether to stand in
        // with a skeleton and which empty state to draw.
        let selected = Arc::clone(&self.selected_uids);
        let anchor = self.selected_uid.clone();
        let has_status_column = self
            .visible_columns
            .iter()
            .any(|&index| self.columns[index].column.id == "status");
        let typography = DataTypography::from_theme_settings(cx);
        let pending = self
            .host
            .read(cx)
            .pending_states()
            .into_iter()
            .map(|(uid, (op, remaining))| (SharedString::from(uid), PendingState { op, remaining }))
            .collect();
        table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            delegate.status = status.clone();
            delegate.snapshot = snapshot;
            delegate.shown = shown;
            delegate.selected = selected;
            delegate.anchor = anchor;
            delegate.table_focused = table_focused;
            delegate.keyboard_focus = self.keyboard_focus.get();
            let order_changed = delegate.sort != sort || delegate.relevance != relevance;
            delegate.sort = sort;
            delegate.relevance = relevance;
            delegate.stale = stale;
            delegate.has_status_column = has_status_column;
            delegate.typography = typography;
            delegate.pending = pending;
            delegate.tier = self.loading_tier;
            delegate.waited = self
                .loading_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
            delegate.progress = progress;
            // The header's sort control is drawn from the shared table's *own*
            // copy of each column's state, and that copy is only rebuilt from the
            // delegate when the table is refreshed. Writing `delegate.sort` on
            // every frame changed the glyph this table draws beside the label and
            // nothing else: the control kept whatever order the table was
            // constructed with, which is `Default` for every column because the
            // delegate is built before the first list exists. So a table sorted by
            // name showed an up arrow on the left of `NAME` and "unsorted" in the
            // control on the right, and stayed that way until something unrelated
            // — a column shown, a column resized — happened to refresh the table.
            if order_changed {
                state.refresh(cx);
            }
        });
        let row_height = row_height(cx);
        // The shared table owns the header band, the empty states and the
        // loading skeleton, so one element covers all three: it sizes the
        // skeleton to its own viewport and swaps the rows out for the state the
        // view hands the delegate.
        let body: AnyElement = DataTable::new(&table)
            .bordered(false)
            .stripe(false)
            .with_size(Size::Size(row_height))
            .into_any_element();

        div()
            .id("resource-grid")
            .debug_selector(|| "resource-grid".to_owned())
            .role(Role::Grid)
            .aria_label(format!("{} Table", self.spec.label))
            .aria_description(TABLE_ACCESSIBILITY_DESCRIPTION)
            .font(ui_font(cx))
            .text_size(design::text::BODY)
            .line_height(design::text::BODY_LINE_HEIGHT)
            // The header is a row of the grid, so the count includes it.
            .aria_row_count(grid_row_count(row_count))
            .aria_column_count(self.visible_columns.len())
            .relative()
            .size_full()
            .min_h_0()
            .bg(table_row_surface(cx))
            // Keep the table after the toolbar in the Tab order.
            .tab_group()
            .tab_index(3)
            .track_focus(&table_focus)
            .key_context(TABLE_CONTEXT)
            .on_action(cx.listener(|view, _: &SelectPrevious, window, cx| {
                view.note_keyboard();
                view.move_selection(Move::Up, window, cx)
            }))
            .on_action(cx.listener(|view, _: &SelectNext, window, cx| {
                view.note_keyboard();
                view.move_selection(Move::Down, window, cx)
            }))
            .on_action(cx.listener(|view, _: &SelectNextColumn, window, cx| {
                view.note_keyboard();
                view.move_column(1, window, cx)
            }))
            .on_action(cx.listener(|view, _: &SelectPreviousColumn, window, cx| {
                view.note_keyboard();
                view.move_column(-1, window, cx)
            }))
            .on_action(cx.listener(|view, _: &SortSelectedColumn, _window, cx| {
                view.note_keyboard();
                view.sort_selected_column(cx)
            }))
            .on_action(cx.listener(|view, _: &OpenDetails, window, cx| {
                view.note_keyboard();
                view.open_details(window, cx)
            }))
            .on_action(cx.listener(|view, _: &OpenRowActions, window, cx| {
                view.note_keyboard();
                view.open_row_context_menu(None, window, cx)
            }))
            .on_action(cx.listener(|view, _: &DeleteSelection, window, cx| {
                view.note_keyboard();
                view.request_delete_confirmation(window, cx)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _event, window, cx| {
                    // A click is not the keyboard arriving. Without this the row
                    // under the pointer painted the same rail a `\u{2193}` does,
                    // which is the single most reliable way to make a focus ring
                    // look decorative.
                    view.keyboard_focus.set(false);
                    let handle = view.table_focus_handle(cx);
                    window.focus(&handle, cx);
                    cx.notify();
                }),
            )
            // The shared table draws its own resize bands on every column edge
            // and its own horizontal scrollbar, so the grid below is only the
            // frame: the named region, the toolbar's Tab order and the rows.
            .child(body)
            // The rest of the header band, once the columns have run out.
            //
            // `UI-SPEC` §10.1 makes `flex` a per-*column* declaration, so only the
            // kinds that end in one fill their panel: `Pod` does, and `Node`,
            // `Namespace` and `ConfigMap` do not. Their rows are 768px of columns
            // in a 1624px panel, and the 856px that remain belong to no column — so
            // the shared table's own header fill showed through there as a
            // rectangle of `surface.raised` with nothing in it, sitting on the same
            // band the columns' own backgrounds had just cleared. §4.4 asks for the
            // band to be transparent, and `docs/mockup` measures one colour from
            // above the rule to below it.
            //
            // It cannot be painted from inside a header cell: the component clips
            // every one of them to its own column, which is also why the per-column
            // background reaches out by the cell padding rather than by its own
            // width. So it is one rect over the grid, laid down after the table so
            // it is on top, starting at the columns' own total so it covers the
            // leftover and nothing else. It overlaps the last column's background
            // by a pixel because the columns' widths are a sum of floats and the
            // panel's edge is not, and the two rects are the same colour, so the
            // overlap is free.
            .when(self.drawn_column_width() > 0.0, |grid| {
                grid.child(
                    div()
                        .debug_selector(|| "resource-header-tail".to_owned())
                        .absolute()
                        .left(px((self.drawn_column_width() - 1.0).max(0.0)))
                        .right_0()
                        .top_0()
                        // 31, not 32: the band's last pixel is the 1px
                        // `border.subtle` rule the component draws, and this rect
                        // is on top of it.
                        .h(design::size::TABLE_HEADER - design::border::LINE)
                        .bg(table_header_surface(cx)),
                )
            })
            .into_any_element()
    }

    /// The width the visible columns add up to, in the grid's own coordinates.
    ///
    /// Zero before the first resolution, which is also when there is no band to
    /// cover: the columns are still at the widths the kind declares and the shared
    /// table has not been handed a viewport.
    fn drawn_column_width(&self) -> f32 {
        self.column_widths.iter().sum()
    }

    /// The header cell of one visible column.
    ///
    /// The shared table owns the column's geometry and its resize handle; this is
    /// the app's own header: its words, its emphasis, the problems filter on the
    /// Status column, the column menu, and the keyboard's focus treatment.
    ///
    /// `UI-SPEC` §4.4, in full: 32px tall, no fill of its own, a 1px
    /// `border.subtle` rule under the band, a `caption` semibold uppercase label
    /// at `fg.tertiary`, `fg.secondary` under the pointer with the rule
    /// strengthened to `border.base`, and `fg.primary` for the sorted column. What
    /// is *not* there is as load-bearing as what is: no column rail, no second
    /// underline floating above the band's own rule, and no arrow on the seven
    /// columns that are not sorted.
    ///
    /// ### Three states, and one of them is a line the header already owns
    ///
    /// `keyboard_focused` is the origin question, exactly as it is for a row's
    /// rail: a `Tab` onto a column and a click on the same column both put the
    /// column in `selected_column`, and a ring that appeared for either is a ring
    /// that means "you are here" and "you clicked here" at once. So the ring is
    /// gated on the same flag the row rail is, and the click below clears it.
    ///
    /// The ring *is* the header band's own bottom edge. The alternative — a 4-sided
    /// outline round a 31px cell — is the loudest thing a 32px band can carry, and
    /// the guide's own rule is that a hairline belongs to the boundary that owns
    /// it: the bottom edge of a column header is exactly that boundary, and a
    /// reader reads it the way they read a selected column in every native table.
    /// So the focused column spends 2px of [`design::border::FOCUS_RAIL`] of
    /// `role::accent` on that edge and the header's own 1px rule shows through
    /// everywhere else. Nothing is added; one boundary changes weight.
    fn header_cell(
        &self,
        position: usize,
        sort: Option<Sort>,
        relevance: bool,
        keyboard_focused: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let index = self.visible_columns[position];
        let column = &self.columns[index];
        let header_height = header_cell_height();
        let group: SharedString = format!("pod-header-{index}").into();
        let header_name: SharedString = column.column.id.as_str().into();
        // The header cell and the wash inside it are both named after the column,
        // and each name is moved into its own closure.
        let hover_name = header_name.clone();
        let affordance = sort_affordance_with_relevance(sort, index, relevance);
        let sorted = affordance_marks_the_column(affordance);
        let title_text = header_label(column.title);
        let description = header_accessibility_description(column.title, affordance);
        let label = header_accessibility_label(column.title, affordance);
        // The label holds one ink for the whole header group: hover is shape, not
        // recolour (see [`header_label_color`]), so it does not join the wash's
        // hover group. It used to shadow that group's ink, which is how a header
        // ended up with a bright label under a faint wash.
        //
        // Weight carries "which column is this table ordered by", not the ink:
        // see [`header_label_weight`].
        let title = div()
            .when(sorted, |title| {
                title.debug_selector(|| "pod-header-sorted-label".to_owned())
            })
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .font(ui_font(cx))
            .text_size(design::text::CAPTION)
            .line_height(design::text::CAPTION_LINE_HEIGHT)
            .font_weight(header_label_weight(affordance))
            .text_color(header_label_color(affordance, cx))
            .child(title_text);
        let cell = div()
            .id(("pod-column", index))
            .debug_selector(move || format!("pod-header-{header_name}"))
            .role(Role::ColumnHeader)
            .aria_label(label)
            .aria_description(description.clone())
            .aria_keyshortcuts(COLUMN_KEYS)
            .aria_column_index(position + 1)
            .group(group.clone())
            // The cell fills the band the shared header already padded and takes
            // the width the sort control leaves, so the label sits immediately
            // beside the control whether it is left or right aligned. It must
            // not claim the whole column as well: the shared table lays this
            // label and the control out as siblings, and a full-width label
            // would push the control out of the cell.
            .flex_1()
            .h(header_height)
            .min_w_0()
            .relative()
            .flex()
            .items_center()
            .gap(design::space::XS)
            // §4.4 puts the band's own fill at `surface.raised`, which is the
            // step above the `content` surface the rows sit on and the one the
            // role ladder names for a table header. The shared table paints its
            // header band with its own `table_head` token, so the band is stated
            // here rather than inherited: one rect per column, reaching out by
            // the cell padding the same way the hover wash below does, and
            // stopping one pixel short of the bottom so the 1px `border.subtle`
            // rule underneath stays the only thing between the header and the
            // rows. It was `table_row_surface` — the rows' own value — which is
            // what made the band read as another row.
            .child(
                div()
                    .debug_selector({
                        let hover_name = hover_name.clone();
                        move || format!("pod-header-fill-{hover_name}")
                    })
                    .absolute()
                    .left(px(-f32::from(design::space::LG)))
                    .right(px(-f32::from(design::space::LG)))
                    .top_0()
                    // `bottom_0`, not one pixel up: this cell is
                    // `TABLE_HEADER - LINE` tall, so the 1px `border.subtle` rule
                    // is already *outside* it, on the band the component draws, and
                    // stopping short of the cell's own edge left a 1px sliver of
                    // that fill between the rect and the rule.
                    .bottom_0()
                    .bg(table_header_surface(cx)),
            )
            // §4.4's second half of the header hover, the part the shared table
            // cannot do for us: `hover → 底线 → border.base` needs the hairline,
            // and the hairline is the header band's own bottom border, drawn by
            // the component out of one style token it owns end to end — nothing
            // this file sets reaches it. So the second half of the hover state
            // is the *column* rather than the band: the one thing the reader is
            // actually pointing at, since a click lands on the column and not on
            // the strip. It matters most on the sorted column, where the label is
            // already `fg.primary` and the indicator a rung quieter, so the label
            // half of the hover has nothing left to say.
            //
            // The wash is a child rather than this cell's own background because
            // the cell sits inside the component's 16px cell padding, and a
            // background here would paint a 32px-narrower rectangle floating
            // inside the column. Reaching back out by exactly the padding the
            // table declares — the same [`cell_paddings`] the width arithmetic
            // charges for — makes the wash meet its neighbour's with no seam.
            //
            // It is `row_hover_bg`, the same solved wash a body row takes, so a
            // header and a row under the pointer are one gesture and the wash is
            // already a step on the row ladder rather than a value picked here.
            .child(
                div()
                    .debug_selector(move || format!("pod-header-hover-{hover_name}"))
                    .absolute()
                    .left(px(-f32::from(design::space::LG)))
                    .right(px(-f32::from(design::space::LG)))
                    .top_0()
                    .bottom_0()
                    .group_hover(group.clone(), |wash| wash.bg(design::row_hover_bg(cx))),
            )
            // No rule of its own along the bottom. §4.4's 1px `border.subtle`
            // hairline is already drawn once, across the whole band, by the shared
            // table's header row — and this cell drew a second one, one scanline
            // above it, inset by the cell's own 16px of padding on each side. Two
            // one-pixel lines a pixel apart is a two-pixel band, and the upper one
            // stopped 32px short at every column boundary, so the header's bottom
            // edge read as a dashed rule with a notch over every divider. Measured
            // on a 1440px table: nine dashes totalling 1195px at logical y=174 over
            // a continuous 1629px rule at y=175.
            .tooltip(text_tooltip(description))
            // Clicking the label picks the column for the column menu, the row
            // actions and `Shift+Enter`. The shared table's own sort control
            // sits beside it and owns the order.
            //
            // A click is not the keyboard arriving. It selects the same column a
            // `Tab` does, so without this the header's focus ring appeared for a
            // pointer exactly as it does for a key — the same mistake the row
            // rail made, in the same place.
            .on_click(cx.listener(move |view, _event: &ClickEvent, window, cx| {
                view.keyboard_focus.set(false);
                view.set_selected_column(index, window, cx);
            }));
        // Header alignment follows the data: numbers right, text left, and the
        // label sits on the *same edge as its values* in both — which the control
        // is in the way of, because a control that always follows the label pushes
        // a right-aligned label 20px in from the numbers it names. `Restarts`
        // ended at a different x from every `0` under it, and a header that does
        // not line up with its column is the one misalignment a reader sees
        // without knowing to look for it.
        //
        // So the control goes on the far side of the label from the values, which
        // is the same rule both ways: the arrow points away from the data. Left
        // of the numbers, `[AGE ^]`; right of them, `[NAME ^]`. One rule, and the
        // label's data edge is the column's data edge either way.
        let right_aligned = column.is_right_aligned();
        let cell = cell.when(right_aligned, |cell| cell.justify_end());

        // §4.4's one mark: the sorted column carries a direction and nothing else
        // does until the pointer arrives. The shared table drew this control
        // itself, and it draws a `ChevronsUpDown` on every column that carries a
        // sort state — a permanent glyph on every header says "sortable" once per
        // column and reads as noise, and the seven glyphs are what turned a
        // 32px header band into a row of icons. The label's `fg.primary` is the
        // state mark; the control is the affordance; neither is duplicated.
        //
        // The label's slot is the column's, so a numeric column's label sits on
        // the same edge as its values in both alignments.
        // A column's controls are visible when the pointer is on the column or
        // when the keyboard has selected it, and only the sorted column's arrow is
        // there at rest. `docs/mockup` draws exactly this band — `Name ^` and
        // nothing else — and the alternative (a glyph on every column that says
        // "sortable" once per column) is the noise §4.4 rules out.
        //
        // Keyboard counts as "on the column": without it, Tab would put a focus
        // ring around a control with no visible glyph in it, which is a control
        // the keyboard can reach and not see.
        let keyboard_on_column = self.selected_column == index;
        // The keyboard's ring on this column's own bottom edge, and only when the
        // keyboard is what put it there. It sits inside the 31px box and reaches
        // back out by the cell padding, exactly as the wash above does, so it
        // meets its neighbour's with no seam and overlaps the shared table's own
        // 1px rule below it — which is the point: the rule under a column header
        // is the header's own edge, and on the focused column that edge is the
        // ring. Nothing is added to the band; one boundary changes weight.
        let focus_name = self.columns[index].column.id.as_str().to_owned();
        let focus_rail = div()
            .debug_selector(move || format!("pod-header-focus-{focus_name}"))
            .absolute()
            .left(px(-f32::from(design::space::LG)))
            .right(px(-f32::from(design::space::LG)))
            .bottom_0()
            .h(design::border::FOCUS_RAIL)
            .when(keyboard_on_column && keyboard_focused, |rail| {
                rail.bg(design::role::accent(cx))
            });
        let cell = cell.child(focus_rail);
        // One hover group for the whole header cell, handed to the controls that
        // live in it. The sort control and the value filter each minted their own
        // group name (`pod-sort-<n>`, `pod-filter-<n>`) and asked for that group,
        // while the cell registered `pod-header-<n>` — so neither reveal ever
        // fired and both controls stayed at `opacity(0)` under the pointer that
        // exists to show them. A group name is a join between two places in the
        // file, so it is made once here and passed down.
        let sort_control = self.column_sort_control(
            position,
            index,
            affordance,
            keyboard_on_column,
            group.clone(),
            cx,
        );
        // One filter control per filterable column, and it is the popover. There
        // used to be a second one here, on the Status column only: a one-click
        // toggle for "rows that need attention", sitting beside the popover that
        // already offers the same filter as its first row. Two controls for one
        // filter is one too many on a band that is 32px tall, and it was worse
        // than redundant — the two drew *the same glyph*. `problems_filter_icon`
        // answers `Funnel` for the toggle's on state and the popover's trigger is
        // a `Funnel` too, so turning the filter on put two identical funnels on
        // the Status header 20px apart, one in accent inside a wash and one in
        // `fg.tertiary`, meaning the same thing twice and looking like two
        // different things.
        //
        // §4.10 is where the row lives and where the shortcut is printed:
        // "底部一条 Only problems ⌥⇧P". One place to reach it, one control in the
        // band, and the same trigger on `Namespace` and `Node` as on `Status` —
        // which is what makes a reader's second column filter take no learning.
        let filter_control = column_filter_field(column).map(|field| {
            self.column_filter_popover(position, field, keyboard_on_column, group.clone(), cx)
        });
        let no_filter = div();
        let filter_control = filter_control.unwrap_or_else(|| no_filter.into_any_element());
        // The header's half of the row's trailing lane, and only the half that can
        // be seen.
        //
        // The body reserves [`TRAILING_LANE`] at the trailing edge of its last
        // cell, so that lane is a spine the band above it has to agree with. A
        // right-aligned column is the case where the disagreement would show: its
        // values end one lane in from the panel edge and its label would end on the
        // panel edge, which is exactly the header-does-not-line-up-with-its-column
        // defect the alignment above exists to prevent. A left-aligned column is
        // unaffected — its label starts at the column's leading edge and a
        // trailing spacer moves nothing — so the spacer is drawn only where it has
        // a job, and the lane is `trailing_lane`'s answer rather than a value
        // invented here, which is what keeps the two halves from drifting.
        let lane = trailing_lane(
            Some(column),
            self.column_widths
                .get(position)
                .copied()
                .map_or_else(|| px(column_default_width(column)), px),
            position + 1 == self.visible_columns.len(),
        );
        // The cell's own name has already moved into the first `debug_selector`
        // above, so the lane gets a copy rather than a second borrow of it.
        let lane_name = self.columns[index].column.id.as_str().to_owned();
        let lane_spacer = div()
            .debug_selector(move || format!("pod-header-lane-{lane_name}"))
            .flex_none()
            .w(lane + TRAILING_LANE_GAP);
        let cell = if right_aligned {
            cell.child(filter_control)
                .child(sort_control)
                .child(title)
                .when(lane > px(0.0), |cell| cell.child(lane_spacer))
        } else {
            cell.child(title).child(sort_control).child(filter_control)
        };
        // Right-click opens visibility and width actions.
        let view = cx.entity().downgrade();
        cell.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |_view, _event, window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.open_header_context_menu(window, cx));
                }
            }),
        )
        .into_any_element()
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

    /// Returns the snapshot rows the table shows, in display order.
    ///
    /// The query's `status!=` clause already left the host's snapshot without the
    /// settled rows, so what is left here is the grade. A row stays only when the
    /// ink in its own status cell calls it a fault.
    ///
    /// It used to keep every row that was *not* `Success`, which is the same
    /// sentence stated as its complement and is not the same set: a status word
    /// this app does not know grades `Neutral` and an absent one grades `Muted`,
    /// and both are "not Success". So on a Service, a ConfigMap or anything else
    /// the cluster reports no status for, a filter named `Only problems` left
    /// every row on screen — while the strip directly above it counted those same
    /// rows as healthy. A filter whose membership the reader cannot predict is
    /// worse than no filter, because they will trust it. `Only problems` is now
    /// exactly the two populations the strip names as faults, and the pending
    /// queue stays out of it, which is what the age grade has always said a
    /// queue is.
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
                let row = &snapshot.rows[index];
                row.cells.get(status).is_some_and(|cell| {
                    matches!(
                        status_severity(cell.text.trim(), row_age(row)),
                        Severity::Warning | Severity::Error
                    )
                })
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
    /// detail of the Status header.
    ///
    /// It is a *predicate* now rather than a flag: the clause is
    /// `status!=Running` plus the severities `design::pod_severity` grades as
    /// healthy, written into the same query string the reader types into. That is
    /// what makes the header's popover the general mechanism instead of a
    /// hard-coded special case — the box shows the clause, a reader can edit it,
    /// and `Clear Filter` clears it with everything else.
    pub fn set_problems_only(&mut self, problems_only: bool, cx: &mut Context<Self>) {
        if self.problems_only == problems_only {
            return;
        }
        self.apply_problems_only(problems_only, None, cx);
    }

    /// The one place `problems_only` changes: by writing the clause into the query.
    ///
    /// A flag the reader cannot see and cannot edit is a second source of truth
    /// next to the box, and the two disagree the moment a reader clears the query
    /// and leaves the filter on. So the toggle is a query edit like any other, and
    /// the boolean below is *derived* from the parsed predicates in
    /// [`Self::commit_filter`] rather than written here.
    fn apply_problems_only(
        &mut self,
        problems_only: bool,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let query = self.filter.read(cx).text().to_owned();
        let next = with_problems_clause(&query, problems_only);
        if next == query {
            return;
        }
        self.apply_query(next, window, cx);
    }

    /// Hides or shows the rows whose status is healthy.
    fn toggle_problems_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_problems_only(!self.problems_only, Some(window), cx);
    }

    /// One column's value popover: the trigger and everything it holds.
    ///
    /// It hangs off the header control rather than being positioned by this file.
    /// Two reasons, and the second is the one that matters: a position computed
    /// here is a second copy of the table's layout arithmetic, and the header band
    /// scales its columns to the viewport, so a copy of the arithmetic is a copy
    /// that is wrong the moment the window is a size the copy was not tested at.
    /// Anchoring to the element that opened it cannot be either.
    ///
    /// The content is re-read every frame, so the counts and the ticks are the ones
    /// the *current* query implies. A popover holding a snapshot of the counts is a
    /// popover whose numbers disagree with the table under it after one click, which
    /// is worse than showing no numbers.
    fn column_filter_popover(
        &self,
        position: usize,
        field: Field,
        revealed: bool,
        group: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let open = self.column_filter_column == Some(position);
        let Some(&column) = self.visible_columns.get(position) else {
            return div().into_any_element();
        };
        let column_id = self.columns[column].column.id.to_string();
        // The counts are over the rows the *other* clauses left, so a tick narrows
        // the table without also narrowing the numbers beside the values still
        // unticked — which is what makes a count usable as a fact.
        let other = self
            .preds
            .iter()
            .filter(|pred| !is_field_clause(pred, field))
            .cloned()
            .collect::<Vec<_>>();
        let values = column_values(self.host.read(cx).snapshot().as_deref(), column, &other);
        let allowed = allowed_values(&self.preds, field);
        // A filter that is doing something keeps its trigger on screen. A hidden
        // control that is already filtering is a control the reader has to find by
        // hovering seven headers to turn off, and that is the one case where
        // "quiet until pointed at" costs more than it buys.
        let active = allowed.as_ref().is_some_and(|values| !values.is_empty());
        let problems_only = self.problems_only;
        let view = cx.weak_entity();
        let view_for_rows = view.clone();
        let view_for_open = view.clone();
        let trigger = self.column_filter_trigger(
            position,
            &column_id,
            open,
            active,
            revealed,
            group.clone(),
            cx,
        );
        Popover::new(("column-filter-popover", position))
            .anchor(Anchor::TopLeft)
            .open(open)
            // `on_open_change` runs while the popover renders, which is inside this
            // view's own update lease, so the write is deferred rather than leasing
            // the view twice.
            .on_open_change(move |open, _, cx| {
                let open = *open;
                let view = view_for_open.clone();
                cx.defer(move |cx| {
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            view.column_filter_column = open.then_some(position);
                            cx.notify();
                        });
                    }
                });
            })
            .trigger(trigger)
            .content(move |_, _window, cx| {
                column_filter_body(
                    field,
                    &values,
                    allowed.as_deref(),
                    problems_only,
                    &view_for_rows,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The header control that opens one column's value popover.
    ///
    /// It is a [`Button`] rather than a bare `div` because the popover hangs off it:
    /// `Popover::trigger` needs something that can be *selected*, and a button is
    /// the thing in this codebase that already has the focus ring, the Enter and
    /// Space handling and the pressed state. A hand-rolled `div` with a click
    /// handler would be reachable by the pointer only, which is the mistake the
    /// Status header's own filter control was written to stop repeating.
    ///
    /// Its own click and its own place in the Tab order: the header's click picks
    /// the column and the shared table's control owns the order, so a control that
    /// shared either would mean a click that filtered sometimes and sorted
    /// sometimes.
    ///
    /// The element is named after the *column* rather than after its position in
    /// the band, so a test can find the `Status` filter without counting columns
    /// and so a reordering cannot silently move the assertion onto another one.
    ///
    /// The ink is declared, because the button owns its glyph and the variant is
    /// the only place an ink can be stated for it. The band has exactly one
    /// resting ink for its controls and one active ink, and both come from
    /// `design::icon` rather than from the variant's own defaults: `Ghost` paints
    /// its icon in `secondary_foreground`, which is the brightest ink in the
    /// component's palette, so a per-value filter — a convenience next to the
    /// query box that can already do it — used to come out louder than the sorted
    /// column's own label. The tier it retreated to was no better: `fg.tertiary`
    /// beside the `fg_secondary` column label is what a *disabled* control looks
    /// like, and this one is never disabled.
    ///
    /// The active tier is spent on the one state the reader has to be able to see
    /// without pointing at anything: a column that is filtering. Every other
    /// column's funnel is at rest opacity until the header lights up, so an ink
    /// difference is the only thing that separates "this column is filtering" from
    /// "this column is hovered".
    ///
    /// Hover stays off the glyph. gpui-kit's `hovered` keeps a custom variant's
    /// `foreground`, so the wash on the control is the whole hover and the funnel
    /// does not change colour under the pointer — a glyph that changes ink on hover
    /// reads as a different glyph.
    #[allow(clippy::too_many_arguments)]
    fn column_filter_trigger(
        &self,
        position: usize,
        column_id: &str,
        open: bool,
        active: bool,
        revealed: bool,
        group: SharedString,
        cx: &App,
    ) -> Button {
        let description = if open {
            "Close the value filter"
        } else {
            "Filter by value"
        };
        let debug_selector = format!("column-filter-trigger-{column_id}");
        // The reveal is the header cell's own hover group, handed in by
        // `header_cell`: the same band a reader is pointing at. It used to mint a
        // name of its own that no cell ever registered, so the trigger was
        // `opacity(0)` on the one column that has one.
        Button::new(format!("column-filter-{position}"))
            .debug_selector(move || debug_selector.clone())
            .icon(IconName::Funnel)
            .custom(
                ButtonCustomVariant::new(cx)
                    .foreground(if active {
                        design::icon::active(cx)
                    } else {
                        design::icon::resting(cx)
                    })
                    .hover(design::role::fg_secondary(cx))
                    .active(design::role::fg_primary(cx)),
            )
            .compact()
            .with_size(Size::Size(design::size::HIT_MIN))
            .w(design::size::HIT_MIN)
            .h(design::size::HIT_MIN)
            .selected(open)
            .group(group.clone())
            .when(!active && !revealed, |button| button.opacity(0.0))
            .group_hover(group, |button| button.opacity(1.0))
            .accessibility_label(description)
            .tooltip(description)
            .tab_index(0)
    }

    /// The header's own sort control: a direction on the sorted column, and the
    /// same mark held back on every other column until the pointer arrives.
    ///
    /// One control per column, in the same [`design::size::HIT_MIN`] box the value
    /// filter beside it uses, because two controls of different sizes in one
    /// header read as two designers. 20px is under §4.6's 24px icon button
    /// because two of them have to sit inside a 32px band beside a word; the
    /// keyboard reaches all three orders with `Shift+Enter`, and the header's own
    /// tooltip says so.
    ///
    /// The box is also the glyph. gpui-kit derives a `Button`'s icon from its own
    /// size at 0.75, and the two controls in this band share a box on purpose, so
    /// the funnel and the chevron come out at one weight without either of them
    /// having to name a size — and a box that differed would have silently made
    /// the two differ too.
    fn column_sort_control(
        &self,
        position: usize,
        index: usize,
        affordance: SortAffordance,
        revealed: bool,
        group: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let sorted = affordance_marks_the_column(affordance);
        // One glyph, three states: a single chevron is a direction, a pair of them
        // is an invitation, and the sorted column has already been invited.
        let icon = match affordance {
            SortAffordance::Ascending => IconName::ChevronUp,
            SortAffordance::Descending => IconName::ChevronDown,
            _ => IconName::ChevronsUpDown,
        };
        let description = sort_description(affordance);
        // One ink for the lane, not one per state: the chevron is a mark beside
        // the column label, and it is drawn one rung below the label so the two
        // are never read as one word. `NAME ^` used to share `fg.primary` on the
        // sorted column, which made it a single eleven-character label as far as
        // the eye was concerned; a chevron a rung quieter than its label is
        // unmistakably a mark beside it.
        //
        // Which column is ordered is already carried twice over by shapes that
        // cost no ink — the label's weight, and the fact that this control stays
        // at rest opacity on the sorted column while every other column's arrow
        // appears only under the pointer. The tier below the resting one would
        // have said neither, and it is the tier that reads as disabled.
        let ink = design::icon::resting(cx);
        let debug_selector = format!("pod-sort-control-{index}");
        let control = Button::new(("column-sort", position))
            .debug_selector(move || debug_selector.clone())
            .icon(icon)
            .custom(
                ButtonCustomVariant::new(cx)
                    .foreground(ink)
                    .hover(design::role::fg_secondary(cx))
                    .active(design::role::fg_primary(cx)),
            )
            .compact()
            .with_size(Size::Size(design::size::HIT_MIN))
            .w(design::size::HIT_MIN)
            .h(design::size::HIT_MIN)
            .accessibility_label(format!("Sort by {}", self.columns[index].title))
            .tooltip(description)
            .tab_index(0)
            .group(group.clone())
            .when(!sorted && !revealed, |control| control.opacity(0.0))
            .group_hover(group, |control| control.opacity(1.0))
            .on_click(cx.listener(move |view, _event: &ClickEvent, _window, cx| {
                view.cycle_column_sort(index, cx);
            }));
        control.into_any_element()
    }

    /// Ticks or unticks one value of one column, and filters on the frame it
    /// happened.
    ///
    /// No debounce: a reader comparing two values wants the second one's tick
    /// without finding the header again, and the counts under the values do not move
    /// — they are a fact about the cluster, not about whether it is ticked.
    fn toggle_column_value(
        &mut self,
        field: Field,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No clause yet means every value is allowed, so the first untick starts from
        // the values actually present rather than from an empty list. That is what
        // makes it narrow rather than empty the table.
        let mut next =
            allowed_values(&self.preds, field).unwrap_or_else(|| self.all_values_of(field, cx));
        if next.iter().any(|allowed| allowed == value) {
            next.retain(|allowed| allowed != value);
        } else {
            next.push(value.to_owned());
        }
        next.sort();
        let query = self.filter.read(cx).text().to_owned();
        self.apply_query(set_field_values(&query, field, &next), Some(window), cx);
    }

    /// Every value a column holds, in popover order.
    fn all_values_of(&mut self, field: Field, cx: &Context<Self>) -> Vec<String> {
        let Some(column) = self
            .columns
            .iter()
            .position(|column| Field::parse(&column.column.id) == Some(field))
        else {
            return Vec::new();
        };
        column_values(self.host.read(cx).snapshot().as_deref(), column, &[])
            .into_iter()
            .map(|(value, _)| value)
            .collect()
    }
}

/// The parse error, drawn under the query box.
///
/// `UI-REDESIGN` L5 A and `UI-SPEC` §4.15 both say inline, and they mean it: a
/// toast for a bad token is a message about the box that appears somewhere the
/// reader is not looking, at a size that cannot hold a token and an expectation
/// side by side, and it disappears before a slow reader has read it.
///
/// The red hairline is the one place red is right. `§0` 铁律三 is that colour is
/// for exceptions, and an unparseable query is the only thing on this screen that
/// is an exception rather than a reading — everything else here is a value, and a
/// value is `fg.tertiary`.
fn query_error_line(error: &QueryError, cx: &App) -> AnyElement {
    let danger = design::role::danger(cx);
    div()
        .id("query-error")
        .debug_selector(|| "query-error".to_owned())
        .absolute()
        .top(design::size::CONTROL)
        .left_0()
        .right_0()
        .mt(design::space::XXS)
        .flex()
        .flex_col()
        .gap(design::space::XXS)
        .rounded(design::radius::SM)
        // The line is a hairline under the box, so it reads as the box's own
        // bottom edge being wrong rather than as a panel that appeared.
        .border_t_1()
        .border_color(danger)
        .pt(design::space::XXS)
        .child(
            div()
                .font(ui_font(cx))
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .text_color(design::role::fg_secondary(cx))
                .child(error.message()),
        )
        .into_any_element()
}

/// The columns a value popover is offered on, and why only these.
///
/// A checkbox per *distinct value* is only meaningful where the values are few
/// and enumerable. `status`, `namespace` and `node` are; `restarts`, `ready` and
/// `age` are ranges, where a list of the numbers present is a list of the numbers
/// that happened to occur, and `name` is a search box's job rather than a
/// checkbox list's. `UI-SPEC` D13 puts the same line at the other end: the popover
/// is for enumerated values, and anything else goes in the query box.
///
/// Asks of a column id, so the answer is `None` for everything the grammar can name
/// but the design will not enumerate — a column that is not a field at all cannot
/// have a clause written for it, so offering a filter on it would be a control that
/// could not do anything.
///
/// The list of filterable columns lives with the width arithmetic in `columns.rs`,
/// which has to budget room for this trigger; asking the column rather than
/// re-deriving the answer here is what keeps the control and the width that
/// reserves space for it from disagreeing.
fn column_filter_field(column: &ResourceColumn) -> Option<Field> {
    if !column.has_value_filter() {
        return None;
    }
    Field::parse(column.column.id.as_str())
}

/// One column's popover body: a row per value, then the problems row.
///
/// D13: no custom input here. A field for writing `restarts>3` inside a popover
/// whose whole job is ticking a value would be a second query box in a place the
/// reader did not go to type one, and the two would be free to disagree. Complex
/// expressions go in the box above the table, which is one control with one grammar.
///
/// A row that is a *button* rather than a checkbox input, because the popover draws
/// the box itself: gpui's own checkbox is a 16px component with its own keyboard
/// model, and a menu row that contains a focusable control is two tab stops deep for
/// one action.
fn column_filter_body(
    field: Field,
    values: &[(String, usize)],
    allowed: Option<&[String]>,
    problems_only: bool,
    view: &WeakEntity<PodsView>,
    cx: &mut App,
) -> AnyElement {
    let mut body = v_flex()
        .id("column-filter-body")
        .debug_selector(|| "column-filter-body".to_owned())
        .w(COLUMN_FILTER_WIDTH)
        // §4.10: 4px of padding, 2px between items. The items are the rows, and the
        // gap is what makes a list of them read as a list.
        .p_1()
        .gap(design::space::XXS)
        .max_h(COLUMN_FILTER_MAX_HEIGHT)
        .overflow_y_scroll();
    for (value, count) in values {
        let checked = allowed.is_none_or(|allowed| allowed.iter().any(|one| one == value));
        let view = view.clone();
        let toggled = value.clone();
        let count = *count;
        body = body.child(
            Button::new(format!("column-filter-value-{value}"))
                .debug_selector({
                    let value = value.clone();
                    move || format!("column-filter-value-{value}")
                })
                .ghost()
                .compact()
                .w_full()
                .h(design::size::ROW_NORMAL)
                .selected(checked)
                .accessibility_label(format!(
                    "{value}, {count} rows. {}",
                    if checked { "Shown" } else { "Hidden" }
                ))
                .on_click(move |_event, window, cx| {
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            view.toggle_column_value(field, &toggled, window, cx)
                        });
                    }
                })
                .child(filter_row_body(value, count, checked, cx)),
        );
    }
    // The separator and the problems row only make sense on a status column. On a
    // namespace list "only problems" is not a thing the app can grade, and a row
    // that does nothing is worse than no row.
    if field == Field::Status {
        let view = view.clone();
        body = body
            .child(
                div()
                    .id("column-filter-separator")
                    .debug_selector(|| "column-filter-separator".to_owned())
                    .h(design::border::LINE)
                    .my(design::space::XXS)
                    .bg(design::role::border_subtle(cx)),
            )
            .child(
                Button::new("column-filter-problems")
                    .debug_selector(|| "column-filter-problems".to_owned())
                    .ghost()
                    .compact()
                    .w_full()
                    .h(design::size::ROW_NORMAL)
                    .selected(problems_only)
                    .accessibility_label(if problems_only {
                        "Only problems. On. Show every row"
                    } else {
                        "Only problems. Off. Show rows that need attention"
                    })
                    .on_click(move |_event, window, cx| {
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |view, cx| view.toggle_problems_filter(window, cx));
                        }
                    })
                    .child(problems_row(problems_only, cx)),
            );
    }
    body.into_any_element()
}

/// The values a column holds, with the count for each, biggest first.
///
/// The count is the reason this popover exists: a reader can act on "9,900 are
/// `Pending`" and cannot act on a list of values with no sizes. It is counted over
/// the rows the *other* clauses left, so ticking a value narrows the numbers under
/// the ones still unticked rather than reporting the pre-filter set forever.
fn column_values(
    snapshot: Option<&IndexSnapshot>,
    column: usize,
    predicates: &[Pred],
) -> Vec<(String, usize)> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for row in &snapshot.rows {
        if !predicates.iter().all(|pred| pred.matches(&row.obj)) {
            continue;
        }
        let Some(cell) = row.cells.get(column) else {
            continue;
        };
        let text = cell.text.trim();
        if text.is_empty() {
            continue;
        }
        *counts.entry(text.to_owned()).or_default() += 1;
    }
    let mut values = counts.into_iter().collect::<Vec<_>>();
    // Biggest first, then alphabetical: a stable order means the list does not
    // reshuffle under the pointer between two clicks, and a list that reorders
    // itself when a count changes is a list a reader mis-clicks.
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    values
}

/// The values a column's clause allows, or `None` when the query says nothing
/// about it.
///
/// `None` and `Some(vec![])` are different answers and both are reachable: a
/// reader who unticks every value has asked for no rows, and a reader who has not
/// touched the column has asked for all of them. Conflating them would make the
/// last untick jump from nothing to everything.
fn allowed_values(preds: &[Pred], field: Field) -> Option<Vec<String>> {
    let mut values = preds.iter().find_map(|pred| match pred {
        Pred::Field {
            field: this,
            compare: Compare::Equal,
            value: Right::Text(values),
            negated: false,
        } if *this == field => Some(values.clone()),
        _ => None,
    })?;
    // Sorted, because the popover compares membership and the query's own order is
    // the reader's, not a set's. Two queries naming the same values are one filter
    // and have to render as one filter.
    values.sort();
    Some(values.iter().map(|value| value.to_string()).collect())
}

/// Sets or clears a field's value list, leaving every other clause as written.
///
/// A string edit, for the same reason the problems toggle is one: the reader may
/// have typed `ns=prod restarts>3` and then clicked a header, and rebuilding the
/// query from parsed predicates would rewrite their words in the parser's
/// spelling underneath the pointer.
///
/// An empty list clears the clause rather than writing `status=`, which the
/// grammar rejects. That makes the last untick the same thing as `Clear`, which is
/// one behaviour rather than two.
fn set_field_values(query: &str, field: Field, values: &[String]) -> String {
    let name = field_query_name(field);
    let mut kept = query
        .split_whitespace()
        .filter(|token| !token_is_field_clause(token, field))
        .map(str::to_owned)
        .collect::<Vec<String>>();
    if !values.is_empty() {
        kept.push(format!("{name}={}", values.join(",")));
    }
    kept.join(" ")
}

/// Whether a query token is a value clause on `field`.
///
/// Read off the parsed clause rather than by string prefix, so `status!=Running`
/// is recognised and a token that merely starts with the same letters is not.
fn token_is_field_clause(token: &str, field: Field) -> bool {
    let Ok(filter) = Filter::parse(token) else {
        return false;
    };
    filter.preds.len() == 1
        && filter.preds.iter().any(|pred| {
            matches!(
                pred,
                Pred::Field { field: this, .. } if *this == field
            )
        })
}

/// The query's spelling of a field, which is not always the column id: a
/// namespace is `ns` in the grammar and `Namespace` in the header.
fn field_query_name(field: Field) -> &'static str {
    match field {
        Field::Namespace => "ns",
        other => other.as_str(),
    }
}

/// One row of a value popover: a checkbox, the value, and how many rows have it.
///
/// `UI-SPEC` §4.10, in full: 14px check, 13px text, and the count right-aligned at
/// `11/500 fg.tertiary`. The count is the whole reason the row exists — a reader can
/// act on "9,900 are `Pending`" and cannot act on a list of values with no sizes —
/// and it is `tertiary` because a count is a reading, not a verdict.
fn filter_row_body(value: &str, count: usize, checked: bool, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .gap(design::space::SM)
        .items_center()
        .child(filter_checkbox(checked, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font(ui_font(cx))
                .text_size(design::text::SUBTITLE)
                .line_height(design::text::SUBTITLE_LINE_HEIGHT)
                .text_color(design::role::fg_primary(cx))
                .child(value.to_owned()),
        )
        .child(
            div()
                .flex_none()
                .font(ui_font(cx))
                .text_size(design::text::CAPTION)
                .line_height(design::text::CAPTION_LINE_HEIGHT)
                .font_weight(design::text::MEDIUM)
                .text_color(design::role::fg_tertiary(cx))
                .child(design::format::count(count)),
        )
        .into_any_element()
}

/// The `Only problems` row, with its shortcut.
///
/// The label says what it does and the chip says how, which is the whole point of
/// a shortcut hint: a reader who has not learned the chord still knows the row
/// works. The chip is gpui-kit's `Kbd` rather than a hand-drawn box, because a
/// keycap is a component with one appearance and a second rendering of it is how
/// two keycaps end up meaning two chords.
fn problems_row(problems_only: bool, cx: &App) -> AnyElement {
    let stroke = Keystroke::parse(PROBLEMS_KEYS)
        .ok()
        .map(Kbd::new)
        .map(|kbd| kbd.into_any_element());
    h_flex()
        .w_full()
        .gap(design::space::SM)
        .items_center()
        .child(filter_checkbox(problems_only, cx))
        .child(
            div()
                .flex_1()
                .font(ui_font(cx))
                .text_size(design::text::SUBTITLE)
                .line_height(design::text::SUBTITLE_LINE_HEIGHT)
                .text_color(design::role::fg_primary(cx))
                .child("Only problems"),
        )
        .when_some(stroke, |row, chip| row.child(chip))
        .into_any_element()
}

/// A 14px check box, drawn rather than borrowed.
///
/// `IconName::Check` in a menu slot is 12px and its box is the menu's, so a popover
/// that used the menu's own check would have a control 2px narrower than §4.10 says and
/// no box at all on the unticked rows — a check that appears is a check, and a
/// missing one reads as "no value here" rather than "not chosen".
///
/// The geometry is the control's own, not an icon lane's: [`design::size::KIND_ICON`]
/// fixes fourteen as the smallest mark lane the product sanctions, and its own note
/// files a check inside its box as control geometry rather than an icon — stated in
/// the box's terms, four pixels of inset so the tick's stroke clears the box's 1px
/// hairline with air to spare where the corner of the check turns.
fn filter_checkbox(checked: bool, cx: &App) -> AnyElement {
    let box_size = design::size::KIND_ICON;
    let tick_size = box_size - design::space::XS;
    let ink = if checked {
        design::role::accent_fg(cx)
    } else {
        design::role::fg_tertiary(cx)
    };
    div()
        .flex_none()
        .w(box_size)
        .h(box_size)
        .rounded(design::radius::XS)
        .border_1()
        .border_color(if checked {
            design::role::accent(cx)
        } else {
            design::role::border_base(cx)
        })
        .bg(if checked {
            design::role::accent(cx)
        } else {
            design::role::surface_overlay(cx).opacity(0.)
        })
        .flex()
        .items_center()
        .justify_center()
        .when(checked, |box_mark| {
            box_mark.child(
                Icon::new(IconName::Check)
                    .with_size(Size::Size(tick_size))
                    .text_color(ink),
            )
        })
        .into_any_element()
}

/// The query clause the Status popover's `Only problems` row writes.
///
/// It is the *settled* healthy states, and deliberately not `Pending`: the age
/// grading that decides whether a pending pod is a problem is a property of one
/// row rather than of the value `Pending`, so it cannot be written as a clause over
/// that value. `shown_rows` applies that half, from the same severities the ink
/// uses, which is why the header control and the popover row agree even though only
/// one of them is a string the reader can edit.
const PROBLEMS_CLAUSE: &str = "status!=Running,Succeeded,Active,Ready,Bound";

/// The chord the `Only problems` row advertises.
///
/// `UI-SPEC` §4.10 puts `⌥⇧P` beside the row, and the binding itself is the
/// keymap's business — a file this change does not own. The row shows the chord
/// because the design specifies it there, and the keymap has to bind the same one.
const PROBLEMS_KEYS: &str = "alt-shift-p";

/// Whether a query already carries the problems clause.
///
/// Read off the parsed clause rather than the raw text, so `status!=Running` typed
/// by hand is the same filter as the row that wrote it — which is the entire point
/// of the two entry points sharing a string. A clause that names *any* exclusion on
/// `status` counts: a reader who typed `status!=CrashLoopBackOff` has asked for rows
/// that are not that, and the header control claiming to be off while rows are hidden
/// is the exact lie the flag-instead-of-a-clause arrangement allowed.
fn has_problems_clause(query: &str) -> bool {
    let Ok(filter) = Filter::parse(query) else {
        return false;
    };
    filter.preds.iter().any(|pred| {
        matches!(
            pred,
            Pred::Field {
                field: Field::Status,
                compare: Compare::NotEqual,
                ..
            }
        )
    })
}

/// Whether a predicate is a value clause on `field`, whatever its values.
///
/// The predicate-level twin of [`token_is_field_clause`]: one reads a parsed clause,
/// the other a query token, and both answer the same question so the popover's
/// counts and its ticks describe the same filter.
fn is_field_clause(pred: &Pred, field: Field) -> bool {
    matches!(pred, Pred::Field { field: this, .. } if *this == field)
}

/// Adds or removes the problems clause, leaving every other clause the reader
/// wrote exactly as it is.
///
/// This is a string edit rather than a predicate edit on purpose. The reader may
/// have typed `ns=prod restarts>3` and then clicked the header; rebuilding the
/// query from parsed predicates would re-render their words in the parser's
/// spelling, and the box would rewrite what they wrote under the pointer.
fn with_problems_clause(query: &str, on: bool) -> String {
    let mut kept = query
        .split_whitespace()
        .filter(|token| !is_problems_clause(token))
        .map(str::to_owned)
        .collect::<Vec<String>>();
    if on {
        kept.push(PROBLEMS_CLAUSE.to_owned());
    }
    kept.join(" ")
}

fn is_problems_clause(token: &str) -> bool {
    let Ok(filter) = Filter::parse(token) else {
        return false;
    };
    filter.preds.len() == 1 && has_problems_clause(token)
}

impl PodsView {
    /// Builds the row menu with only the available actions. The menu binds to
    /// the row behind the target, never to a position that a rebuild can reuse.
    ///
    /// The rows come back as a list so the keyboard path and the shared table's
    /// own right-click popup describe exactly the same menu.
    fn row_menu_entries(
        &mut self,
        target: RowTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PopupMenuItem> {
        // The row left the snapshot, so the menu must not fall back to a neighbour.
        let Some(index) = self.resolve_row_target(Some(target), cx) else {
            return Vec::new();
        };
        self.select_index(index, window, cx);
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return Vec::new();
        };
        let Some(row) = snapshot.rows.get(index).cloned() else {
            return Vec::new();
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
        let forward_target = self
            .port_forward_target_for_object(&object)
            .and_then(Result::ok);
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
        // The one destructive row carries `danger` on both its glyph and its
        // label. `role::danger` rather than a solved marker: the menu's surface is
        // an overlay, `danger` is the channel, and solving it per-background is how
        // one role ends up meaning two colours.
        let delete_marker = design::role::danger(cx);

        let mut entries = vec![
            menu_item("Copy Name").icon(IconName::Copy).on_click({
                let name = name.clone();
                move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
                }
            }),
            // Single click only selects now, and the trailing ellipsis button is
            // gone, so the row menu is the one place a pointer reaches the
            // preview. It sits first because it is what most readers want from a
            // row they just right-clicked. A second `Describe` entry used to
            // carry a byte-identical handler: two labels, two icons, one
            // behaviour.
            menu_item("Open details")
                .icon(IconName::ChevronRight)
                .on_click({
                    let view = ops_view.clone();
                    let row = describe_row.clone();
                    move |_event, window, cx| {
                        view.update(cx, |view, cx| view.activate_row(row.clone(), window, cx))
                            .ok();
                    }
                }),
        ];
        if let (Some(handler), Some(target)) = (&service_account_handler, service_account_target) {
            let handler = handler.clone();
            entries.push(
                menu_item("Open Service Account")
                    .icon(IconName::UserCheck)
                    .on_click(move |_event, window, cx| {
                        handler(target.clone(), window, cx);
                    }),
            );
        }
        if let (Some(handler), Some(request)) = (&logs_handler, &log_request) {
            let handler = handler.clone();
            let request = request.clone();
            entries.push(
                menu_item("Logs")
                    .icon(IconName::BookOpen)
                    .on_click(move |_event, window, cx| handler(&request, window, cx)),
            );
        }
        if let (Some(_), Some(target)) = (&exec_handler, exec_target) {
            let view = ops_view.clone();
            entries.push(menu_item("Exec").icon(IconName::Terminal).on_click(
                move |_event, window, cx| {
                    view.update(cx, |view, cx| {
                        view.request_exec_target(target.clone(), window, cx)
                    })
                    .ok();
                },
            ));
        }
        if let (Some(_), Some(target)) = (&forward_handler, forward_target) {
            let view = ops_view.clone();
            entries.push(
                menu_item(START_PORT_FORWARD)
                    .icon(IconName::ArrowRight)
                    .on_click(move |_event, window, cx| {
                        view.update(cx, |view, cx| {
                            view.request_port_forward_target(target.clone(), window, cx)
                        })
                        .ok();
                    }),
            );
        }
        if !ops_available {
            return entries;
        }
        if can_restart {
            let view = ops_view.clone();
            let target = object_ref.clone();
            entries.push(menu_item("Restart").icon(IconName::RotateCw).on_click(
                move |_event, _, cx| {
                    if let Some(target) = target.clone() {
                        view.update(cx, |view, cx| view.request_restart_target(target, cx))
                            .ok();
                    }
                },
            ));
        }
        if can_scale {
            let view = ops_view.clone();
            entries.push(menu_item("Scale").icon(IconName::ArrowRightLeft).on_click(
                move |_event, window, cx| {
                    if let Some(target) = scale_target.clone() {
                        view.update(cx, |view, cx| {
                            view.request_scale_target_dialog(target.clone(), window, cx)
                        })
                        .ok();
                    }
                },
            ));
        }
        if delete_available {
            let view = ops_view.clone();
            entries.push(
                // The one destructive row, so it is the one that carries the
                // error channel on both its glyph and its label.
                //
                // The glyph goes in the menu's own icon slot rather than beside
                // the label. The slot is the lane this menu's other glyphs are
                // drawn in — the component sizes it — so a hand-drawn twin beside
                // the label put two bins on one row at two weights, one of them in
                // the row's default ink because the slot is what the other rows
                // use, and the danger one was the second rather than the only.
                PopupMenuItem::element(move |_, _| {
                    h_flex()
                        .id("menu-item-delete")
                        .flex_1()
                        .min_w_0()
                        .child(Label::new("Delete").text_color(delete_marker).truncate())
                })
                .icon(Icon::new(IconName::Trash).text_color(delete_marker))
                .on_click(move |_event, window, cx| {
                    if let Some(target) = delete_target.clone() {
                        view.update(cx, |view, cx| {
                            view.request_delete_target_confirmation(target.clone(), window, cx)
                        })
                        .ok();
                    }
                }),
            );
        }
        entries
    }

    /// The rows the shared table offers for one snapshot index.
    ///
    /// The index arrives already resolved from the table's list position, so the
    /// menu binds to the row and not to a slot in the list.
    fn row_menu_entries_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PopupMenuItem> {
        let target = self
            .host
            .read(cx)
            .snapshot()
            .as_deref()
            .and_then(|snapshot| snapshot.rows.get(index))
            .and_then(RowTarget::for_row)
            .unwrap_or(RowTarget::Position(index));
        self.row_menu_entries(target, window, cx)
    }

    /// The inline error bar `UI-SPEC` §4.15 puts under the table body.
    ///
    /// The bar replaced a full-area panel: a 40px coloured disc, a title, a
    /// 480px explanation, a guidance line and a button, in the middle of the
    /// table, with `Role::Alert`. That is a dialog's apology, and it has three
    /// costs. It hid rows the reader could still read — a watch that stopped
    /// leaves the last snapshot on screen and the panel threw it away. It stole
    /// focus on entry, so every later render pulled focus back to Retry and the
    /// reader could not Tab past it. And it replaced the *region*, so the table's
    /// own scroll position, its column widths and its selection went with it.
    ///
    /// §4.15 wants the error where it happened: 32px, a band of its own, a 3px
    /// `danger` bar, a `danger` wash behind it, 12px `danger` copy, and a verb
    /// phrase as the action. The rows stay.
    ///
    /// "Where it happened" is the half of §4.15 that did not survive. The bar sat
    /// above the body, so a watch stopping moved the header and every row down
    /// 32px and starting again moved them back: the window rearranged itself
    /// under a reader who was looking at it. Under the body it is the same band
    /// with the same ink, and the table loses 32px of height instead of gaining
    /// it, which is a change the reader's eye is already used to from the
    /// selection bar.
    fn error_bar(
        &self,
        status: &TableStatus,
        has_rows: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let (reason, action, guidance) = match status {
            TableStatus::Failed(reason) => {
                // A permission failure and a connection failure need different
                // words and the same retry, because retrying is the only thing
                // that can change either of them.
                let permission = is_permission_failure(reason);
                let plural = self.spec.label_lower();
                let copy = if permission {
                    format!(
                        "This identity has no permission to list {plural} in this scope. It can \
                         still list namespaces."
                    )
                } else {
                    user_reason(reason, &plural)
                };
                (reason.clone(), Recovery::RetryList, copy)
            }
            // §4.14: with rows on screen a stale watch is a warning about the
            // data, not a reason to replace it. The bar is the "keep it and mark
            // it stale" half of that rule.
            TableStatus::Stale(reason) if has_rows => (
                reason.clone(),
                Recovery::RetryLive,
                RETRY_LIVE_UPDATES_GUIDANCE.to_owned(),
            ),
            _ => return None,
        };
        if self.dismissed_error.as_deref() == Some(reason.as_str()) {
            return None;
        }
        let (id, label, emphasis) = action.button();
        let focus = match action {
            Recovery::RetryLive => self.stale_retry_focus.clone(),
            _ => self.retry_focus.clone(),
        };
        let tooltip = action.tooltip(cx);
        let tooltip = text_tooltip(format!(
            "{tooltip} \u{2014} {}",
            table_status_detail(status)
        ));
        Some(
            h_flex()
                .id("table-error-bar")
                .debug_selector(|| "table-error-bar".to_owned())
                .role(Role::Alert)
                .aria_label(reason.clone())
                .aria_description(guidance.clone())
                .w_full()
                .flex_none()
                .h(INLINE_ERROR_HEIGHT)
                .gap(design::space::SM)
                .items_center()
                // The text starts where the summary strip's and the first column's
                // text start: 16, the table's own padding-x. `UI-SPEC` §7 asks for
                // one left edge across everything in a region, and the bar was the
                // one thing in the table's own bands that broke it.
                //
                // It could not be fixed by padding alone. The rule is 3px of
                // `danger` on the band's own top edge, so the content's left edge
                // is the region's inset and nothing else — no rule inset to add
                // and no 5 to subtract, which is a value in no step of §2.1's ten.
                // The content pads to the region's own inset and the rule is laid
                // across the band above it.
                .pl(TABLE_CONTENT_INSET)
                .relative()
                .bg(design::role::danger_wash(cx))
                .font(ui_font(cx))
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .child(
                    // The 3px bar is the error's one piece of geometry, and it is
                    // the only thing in the table that is a solid block of `danger`,
                    // so it reads as a boundary rather than as content.
                    //
                    // It is on the band's **top** edge because that is the boundary
                    // it owns: the edge between the rows the reader is reading and
                    // the statement that those rows may be behind. It used to be a
                    // 3px stub down the band's leading edge, which was right while
                    // the band sat at the region's left — it marked the whole band
                    // by marking its corner. Under the body there is no left edge
                    // to mark, and a full-width rule across the top is both the
                    // stronger claim and the same 3px of ink: the fault is the one
                    // thing on this screen that draws a line under the data.
                    div()
                        .debug_selector(|| "table-error-bar-rule".to_owned())
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .h(design::space::XXS + px(1.))
                        .bg(design::role::danger(cx)),
                )
                .child(
                    div()
                        .debug_selector(|| "table-error-guidance".to_owned())
                        .flex_1()
                        .min_w(px(0.0))
                        // The `_word` role, because this is a sentence and the
                        // design system's word roles are the ones solved to clear
                        // the text floor. The bar's own 3px rule keeps the bare
                        // `danger`: it is a graphic, and it is the one thing in the
                        // table that is a solid block of it.
                        .text_color(design::role::danger_word(cx))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(SharedString::from(guidance)),
                )
                .child(
                    recovery_button(id, label, &focus, emphasis, 0isize, cx)
                        .tooltip(tooltip)
                        .on_click(move |_event, window, cx| action.dispatch(window, cx)),
                )
                .child(
                    recovery_icon_button(
                        "table-error-dismiss",
                        IconName::X,
                        &self.notice_focus,
                        1isize,
                        "Dismiss error",
                        cx,
                    )
                    .tooltip(text_tooltip("Dismiss error"))
                    .on_click({
                        let view = cx.weak_entity();
                        let reason = reason.clone();
                        move |_event, _window, cx| {
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |view, cx| view.dismiss_error(reason.clone(), cx));
                            }
                        }
                    }),
                )
                .into_any_element(),
        )
    }

    /// Hides the inline error bar without touching the data underneath it.
    ///
    /// The bar's state comes from the host, so dismissing it cannot clear the
    /// host's reason — it records *which* reason the reader dismissed and the bar
    /// comes back the moment the host reports a different one. Losing the error
    /// for good because the reader looked away would be worse than the bar; so
    /// would a bar that cannot be dismissed at all.
    fn dismiss_error(&mut self, reason: String, cx: &mut Context<Self>) {
        self.dismissed_error = Some(reason);
        cx.notify();
    }

    /// The bar docked at the bottom of the table while more than one row is
    /// selected, and the confirmation step for deleting them.
    ///
    /// `UI-SPEC` §4.4 places it at the **bottom** and says why: "浮层会盖住行, 而遮住
    /// 内容正是 popover 最不该做的事". It used to be at the top, full-bleed in a danger
    /// wash, and it shared a slot with a selection counter that lived in the pill
    /// toolbar. So the count was at the top of the window, the confirmation was at the
    /// top of the window, and the rows they were about — or about to be destroyed —
    /// were underneath all of it.
    ///
    /// The bar has two states and one slot. Idle it is `5 selected  [Restart]
    /// [Delete 5 pods]  Esc`; pressed it becomes the confirmation step. That is
    /// deliberate: escalating the same bar is a smaller step than opening a dialog,
    /// and the rows it covers are still the rows it is about.
    ///
    /// Idle, every control on the bar is the *same* ghost. It used to end in a
    /// filled accent `Delete 5 pods`, which made the bar's one uncommitted command
    /// look like a committed one and spent the screen's scarcest colour — the one
    /// the row selection's rail and the one real primary action in the window
    /// compete for — on a button whose whole job is to open the confirmation step
    /// one row below it. `primary` means "the default commit", and this is not it:
    /// the commit is the `Delete` in the confirmation state, and that one is
    /// `danger` because it is the destructive one. One treatment at rest, one
    /// commit when pressed.
    ///
    /// No shadow. It sits *on* the content rather than above it, and a shadow on a
    /// bar that is not floating is the §8 restraint checklist's first entry.
    ///
    /// It is left-aligned on [`TABLE_CONTENT_INSET`], not centred. Centred, the bar
    /// and the six bands above it were the only things in the region with no
    /// leading edge: the count read `3 selected` at whatever x its own width put
    /// it, and a reader who scans the column that starts at 16px found nothing
    /// there. One action band for the whole region, on the region's own spine,
    /// is what makes it a bar rather than a caption.
    fn selection_bar(
        &self,
        request: Option<&MultiDeleteRequest>,
        spec: &ResourceSpec,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let selection = self.selection_count();
        let confirming = request.is_some();
        if selection <= 1 && !confirming {
            return None;
        }
        let noun = spec.label_lower();
        let confirm_focus = self.multi_delete_confirm_focus.clone();
        let cancel_focus = self.multi_delete_cancel_focus.clone();
        let view = cx.weak_entity();
        let bar: AnyElement = if let Some(request) = request {
            let count = request.objects.len();
            let message = format!("Delete {} {noun}?", design::format::count(count));
            h_flex()
                .id("multi-delete-bar")
                .debug_selector(|| "multi-delete-bar".to_owned())
                .role(Role::AlertDialog)
                .aria_label(message.clone())
                .aria_description(request.label.clone())
                .w_full()
                .flex_none()
                .h(design::size::SELECTION_BAR)
                .gap(design::space::SM)
                .items_center()
                .px(TABLE_CONTENT_INSET)
                .bg(design::role::danger_wash(cx))
                .font(ui_font(cx))
                .text_size(design::text::BODY)
                .line_height(design::text::BODY_LINE_HEIGHT)
                .child(
                    // The one severity-coloured mark in the bar, at the status
                    // marker size every other status mark in the table uses, so the
                    // bar's height is decided by the 28px controls and the padding
                    // rather than by the mark.
                    div()
                        .id("multi-delete-marker")
                        .debug_selector(|| "multi-delete-marker".to_owned())
                        .flex_none()
                        .w(design::size::STATUS_MARKER)
                        .child(
                            Icon::new(IconName::Trash)
                                .with_size(Size::Size(design::size::STATUS_MARKER))
                                .text_color(design::role::danger(cx)),
                        ),
                )
                .child(
                    div()
                        .text_color(design::role::fg_primary(cx))
                        .whitespace_nowrap()
                        .child(message),
                )
                .child(
                    recovery_button(
                        "multi-delete-cancel",
                        "Cancel",
                        &cancel_focus,
                        ButtonEmphasis::Default,
                        1isize,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_event, window, cx| {
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |view, cx| view.cancel_multi_delete(window, cx));
                            }
                        }
                    }),
                )
                .child(
                    recovery_button(
                        "multi-delete-confirm",
                        "Delete",
                        &confirm_focus,
                        ButtonEmphasis::Danger,
                        0isize,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_event, window, cx| {
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |view, cx| view.confirm_multi_delete(window, cx));
                            }
                        }
                    }),
                )
                .into_any_element()
        } else {
            // The count is the live region. A range selection used to be invisible:
            // the member rows shared the active row's fill with a measured 1.018:1
            // against the row beside them, and nothing anywhere counted them.
            let count_label = design::format::count_with_noun(selection, "selected", "selected");
            let description =
                design::format::count_with_noun(selection, "row selected", "rows selected");
            let can_restart = matches!(
                spec.kind.as_ref(),
                "Deployment" | "StatefulSet" | "DaemonSet"
            );
            let can_scale = SCALABLE_KINDS.contains(&spec.kind.as_ref());
            h_flex()
                .id("selection-bar")
                .debug_selector(|| "selection-bar".to_owned())
                .role(Role::Status)
                .aria_label(description)
                .w_full()
                .flex_none()
                // §4.4: the one stroke the bar has, and it is on top because the bar
                // is below the content. The bottom edge needs nothing: it is the
                // window's.
                .border_t_1()
                .border_color(design::role::border_subtle(cx))
                .h(design::size::SELECTION_BAR)
                .gap(design::space::SM)
                .items_center()
                // `surface.chrome`, not the table's own `surface.content`. The bar
                // is chrome: it is about the table rather than part of it, it sits
                // between the rows and the status bar, and on the content surface
                // it was indistinguishable from the last row above it — a band of
                // the same colour with a hairline on top, which is the shape of a
                // row divider and not the shape of a toolbar.
                //
                // The count is `text::LABEL` because it is metadata about the rows
                // rather than a cell in them, and every button on the bar is the
                // same ghost: the commit is the `danger` `Delete` in the
                // confirmation state, so an accent button here would claim a
                // decision the bar has not taken. The trailing `Esc` is the way
                // out rather than an action, so it is `fg.tertiary`.
                .px(TABLE_CONTENT_INSET)
                .bg(design::role::surface_chrome(cx))
                .font(ui_font(cx))
                .text_size(design::text::LABEL)
                .line_height(design::text::LABEL_LINE_HEIGHT)
                .child(
                    div()
                        .id("selection-count")
                        .debug_selector(|| "selection-count".to_owned())
                        .flex_none()
                        .text_color(design::role::fg_secondary(cx))
                        .child(SharedString::from(count_label)),
                )
                .when(can_restart, |bar| {
                    bar.child(
                        recovery_button(
                            "selection-restart",
                            "Restart",
                            &self.multi_delete_cancel_focus,
                            ButtonEmphasis::Default,
                            0isize,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_event, _window, cx| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |view, cx| view.restart_selection(cx));
                                }
                            }
                        }),
                    )
                })
                .when(can_scale, |bar| {
                    bar.child(
                        recovery_button(
                            "selection-scale",
                            "Scale",
                            &self.multi_delete_cancel_focus,
                            ButtonEmphasis::Default,
                            0isize,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_event, window, cx| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |view, cx| view.scale_selection(window, cx));
                                }
                            }
                        }),
                    )
                })
                .child(
                    recovery_button(
                        "selection-delete",
                        format!("Delete {} {noun}", design::format::count(selection)),
                        &self.multi_delete_confirm_focus,
                        // Ghost, like its two neighbours. See the doc comment: the
                        // commit is the `Delete` in the confirmation state, and
                        // painting this one accent made the bar claim a decision it
                        // has not taken.
                        ButtonEmphasis::Default,
                        1isize,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_event, window, cx| {
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |view, cx| {
                                    view.request_delete_confirmation(window, cx)
                                });
                            }
                        }
                    }),
                )
                .child(
                    // `Esc` is drawn rather than assumed: §4.4 lists it in the bar's
                    // contents, and a shortcut the interface does not show is a
                    // shortcut most readers will not find. It is `fg.tertiary`
                    // because it is the way *out*, not an action.
                    div()
                        .flex_none()
                        .text_color(design::role::fg_tertiary(cx))
                        .child(SharedString::from("Esc")),
                )
                .into_any_element()
        };
        Some(bar)
    }

    /// Restarts every selected row that the operations layer can restart.
    ///
    /// The bar's `Restart` used to be a `\u{2026}` on a row, which meant a reader who
    /// had selected five pods to delete three of them had no way to restart the other
    /// two without selecting them again. Restart is reversible and delete is not, so
    /// the bar is also the place where "cancel" is an action.
    fn restart_selection(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let uids: Vec<SharedString> = self.selected_uids.iter().cloned().collect();
        let mut restarted = 0;
        for uid in uids {
            let Some(row) = snapshot.row_by_uid(uid.as_ref()) else {
                continue;
            };
            let object = Arc::clone(&row.obj);
            let Some(target) = self.object_ref(&object) else {
                continue;
            };
            if !matches!(
                self.spec.kind.as_ref(),
                "Deployment" | "StatefulSet" | "DaemonSet"
            ) {
                continue;
            }
            self.request_restart_target(target, cx);
            restarted += 1;
        }
        if restarted == 0 {
            self.notify(
                "No selected row can be restarted from here.",
                Severity::Muted,
                cx,
            );
        }
    }

    /// Opens the scale dialog for the selected row, if there is exactly one.
    fn scale_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.host.read(cx).snapshot() else {
            return;
        };
        let Some(uid) = self.selected_uid.clone() else {
            return;
        };
        let Some(row) = snapshot.row_by_uid(uid.as_ref()) else {
            return;
        };
        let object = Arc::clone(&row.obj);
        let Some(target) = self.scale_target_for_object(&object) else {
            self.notify("This resource cannot be scaled.", Severity::Muted, cx);
            return;
        };
        self.request_scale_target_dialog(target, window, cx);
    }
}

impl Render for PodsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, snapshot, total) = {
            let host = self.host.read(cx);
            (host.status(), host.snapshot(), host.total_count())
        };
        // A session that is still loading kubeconfig must not steal focus for a
        // Retry button that a later failure may replace.
        //
        // The error bar does not steal focus at all. §4.15's rule is that the
        // error appears where it happened and leaves the reader where they were,
        // and the old panel focused Retry on entry and then re-focused it on every
        // render — so a reader who reached for a row could not Tab past it, and a
        // reader who clicked a *different* focusable had the focus yanked back
        // under them.
        if self.restore_table_focus {
            self.restore_table_focus = false;
            let focus = self.table_focus_handle(cx);
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        }
        // The strip counts what the table *lists*, not what the snapshot holds.
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
        // `UI-SPEC` §4.14: how long the reader has actually been waiting picks the
        // tier, and the timestamp is stamped when the list actually starts rather
        // than when the view is built, so a table opened onto a warm cache does
        // not spend its first two seconds in a loading state it never needed.
        // The ladder restarts with every list, and each rung is a timer rather
        // than a comparison: a late frame must not be able to skip a tier, and a
        // tier that only a wall clock can reach is a tier nobody can test.
        if matches!(status, TableStatus::Listing) {
            if self.loading_since.is_none() {
                self.loading_since = Some(Instant::now());
                self.advance_loading_tier(cx);
            }
        } else {
            self.loading_since = None;
            self.loading_tier = LoadingTier::Nothing;
        }
        self.refresh_summary(&snapshot, &shown);
        let body = self.table(snapshot, shown.clone(), &status, window, cx);

        let notice_view = cx.weak_entity();
        let notice = self.notice.clone().map(|notice| {
            let epoch = notice.epoch;
            let notice_focus = self.notice_focus.clone();
            let view = notice_view.clone();
            notice_banner(&notice, cx)
                .child(
                    recovery_icon_button(
                        "resource-notice-dismiss",
                        IconName::X,
                        &notice_focus,
                        0isize,
                        "Dismiss message",
                        cx,
                    )
                    .tooltip(text_tooltip("Dismiss message"))
                    .on_click(move |_, window, cx| {
                        view.update(cx, |view, cx| view.dismiss_notice(epoch, window, cx))
                            .ok();
                    }),
                )
                .into_any_element()
        });
        let summary = self.summary_strip(&status, shown.len(), total, cx);
        let error_bar = self.error_bar(&status, !shown.is_empty(), cx);
        let selection_bar =
            self.selection_bar(self.multi_delete.as_ref(), &self.spec.clone(), window, cx);
        let context_menu = self.context_menu.as_ref().map(|menu| {
            let position = self.context_menu_position;
            let menu = menu.clone();
            gpui_kit::deferred(
                gpui_kit::anchored()
                    .position(position)
                    .snap_to_window_with_margin(design::space::SM)
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
            .bg(table_row_surface(cx))
            .text_color(design::role::fg_primary(cx))
            .track_focus(&self.focus_handle)
            .key_context(TABLE_CONTEXT)
            .on_action(cx.listener(|view, _: &ToggleUpdates, window, cx| {
                view.note_keyboard();
                let from_recovery = view.empty_action_focus.is_focused(window)
                    || view.focus_restore_for_recovery(window);
                view.restore_table_focus |= from_recovery;
                view.toggle_updates(cx)
            }))
            .on_action(cx.listener(|view, _: &ToggleChurn, _window, cx| view.toggle_churn(cx)))
            .on_action(cx.listener(|view, _: &ToggleProblemsOnly, window, cx| {
                view.toggle_problems_filter(window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &FocusFilter, window, cx| view.focus_filter(window, cx)),
            )
            .on_action(cx.listener(|view, _: &ClearFilter, window, cx| {
                view.note_keyboard();
                // Clearing the filter fills the table again, so the table takes
                // the focus back instead of the filter that held the button.
                view.restore_table_focus |= view.focus_restore_for_recovery(window);
                view.clear_filter(window, cx)
            }))
            .on_action(cx.listener(|view, _: &Refresh, window, cx| {
                view.note_keyboard();
                view.restore_table_focus |= view.focus_restore_for_recovery(window);
                view.dismissed_error = None;
                view.loading_since = Some(Instant::now());
                view.loading_tier = LoadingTier::Nothing;
                view.advance_loading_tier(cx);
                view.refresh(cx)
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(summary)
            .child(div().flex_grow_1().min_h_0().child(body))
            // §4.15 wanted the error at the top of the table body so it would not
            // be interleaved with the summary strip — the strip reports what is on
            // screen, and the bar reports why the screen might be behind the
            // cluster. Those are two claims and only the first survives: a band
            // above the body is a band that pushes the body down, so the header
            // and every row moved 32px when a watch stopped and 32px back when it
            // came, and the notice above it did the same on its own eight-second
            // timer. The top was the only place from which a transient band could
            // displace rows, so the transient bands moved to the only place from
            // which they cannot.
            //
            // Here the table shrinks from the bottom, which is what the selection
            // bar has always done and what every reader has already accepted as
            // how this table behaves when something about the rows is currently
            // true. Nothing above the body moves, so the header holds its y, the
            // reader's rows hold their y, and the scroll position keeps meaning
            // something.
            //
            // The order is fault, then message, then the reader's own actions:
            // the fault is about the data that is on screen and belongs against
            // it, the message is about something the reader just did, and the
            // selection bar is a committed state with buttons on it and stays
            // nearest the window's own edge. All three are `flex_none` and all
            // three are below the body, so they stack upward from it and nothing
            // in the header stack is ever conditional.
            .when_some(error_bar, |this, bar| this.child(bar))
            .when_some(notice, |this, notice| this.child(notice))
            .when_some(selection_bar, |this, bar| this.child(bar))
            .when_some(context_menu, |this, menu| this.child(menu))
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
        let table_context = gpui_kit::KeyContext::parse("Table").ok();
        let key_taken = |spec: &str| {
            let (Ok(key), Some(context)) =
                (gpui_kit::Keystroke::parse(spec), table_context.as_ref())
            else {
                return false;
            };
            let (matches, pending) =
                bindings.bindings_for_input(&[key], std::slice::from_ref(context));
            !matches.is_empty() || pending
        };
        let missing = |action: &dyn gpui_kit::Action, spec: &str| {
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

// ---------------------------------------------------------------------------
// Menus, tooltips, and the focus-owned buttons
// ---------------------------------------------------------------------------

/// A plain-text tooltip for a `div`. gpui-kit's `Tooltip` owns the popup, its
/// placement and its dismissal; the app only hands it the words.
fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

/// A context menu built from `PopupMenu` rows. gpui-kit owns the popup surface,
/// its focus ring and its dismissal, so the app only describes the rows.
fn build_menu(
    window: &mut Window,
    cx: &mut App,
    build: impl FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, build)
}

/// One row of a context menu.
///
/// The label is rendered here rather than left to the default row so each entry
/// keeps a `MENU_ITEM-<label>` debug selector. Several menu behaviours are
/// asserted by name — a width entry that has to name its column, a row that must
/// not offer `Describe` twice — and an index-derived element id would make those
/// assertions depend on the order the rows happen to be in.
fn menu_item(label: impl Into<SharedString>) -> PopupMenuItem {
    let label: SharedString = label.into();
    let id: SharedString = format!("menu-item-{label}").into();
    let selector: SharedString = format!("MENU_ITEM-{label}").into();
    PopupMenuItem::element(move |_, _| {
        let selector = selector.clone();
        div()
            .id(id.clone())
            .debug_selector(move || selector.to_string())
            .flex_1()
            .min_w_0()
            .truncate()
            .child(label.clone())
    })
}

/// How loud a recovery control looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ButtonEmphasis {
    /// The resting surface, for a control that offers a way forward.
    Default,
    /// The accent surface, for the one control that repairs the table itself.
    Accent,
    /// The error surface, for the one destructive confirmation.
    Danger,
}

/// A button the view keeps a `FocusHandle` for.
///
/// The themed `Button` owns its focus handle and gives no way to hand it one,
/// but the table view has a focus contract that needs an external handle: a
/// recovery control (Retry, Refresh, the multi-delete confirmation) must be
/// reachable without a pointer, and the table has to take the focus back when
/// that control disappears together with the state it repaired.
///
/// `base::Button` is the same button without the theme's surface. It still owns
/// the pointer, the keyboard, the focus ring and the accessible name, so the app
/// supplies only the surface the rest of the toolbar shares.
fn recovery_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    focus: &FocusHandle,
    emphasis: ButtonEmphasis,
    tab_index: isize,
    cx: &App,
) -> BaseButton {
    let (background, foreground) = match emphasis {
        // §4.6's ghost: no fill at rest, and a hover fill because a ghost is a
        // toolbar-shaped control rather than a pushed one.
        ButtonEmphasis::Default => (None, design::role::fg_secondary(cx)),
        // The screen's one `primary`. §4.6: a pushed button has no hover, only a
        // pressed state, which is why this arm carries no `.hover`.
        ButtonEmphasis::Accent => (Some(design::role::accent(cx)), design::role::accent_fg(cx)),
        // A wash of the channel rather than a solid fill: the one destructive
        // control in the table has to read as destructive without spending a
        // second status colour on screen, and `PROMPT` §2.1 #8 caps the accent at
        // two uses per screen. The label on that wash is the channel's *word*
        // ink, because a label on a wash of its own channel is what the word
        // role exists for and the button's label is 12px body text.
        ButtonEmphasis::Danger => (
            Some(design::role::danger_wash(cx)),
            design::role::danger_word(cx),
        ),
    };
    let button = BaseButton::new(id)
        .track_focus(focus)
        .tab_stop(true)
        .tab_index(tab_index)
        .h(design::size::CONTROL)
        .px(design::space::MD)
        // §4.6: a button is `radius::MD`. It was `SM`, which is the chip's value,
        // so a button and a chip at the same height had different corners — the
        // single most reliable way to make one set of components look like
        // several people drew them.
        .rounded(design::radius::MD)
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT)
        .font_weight(design::text::MEDIUM)
        .text_color(foreground)
        .child(Label::new(label));
    let button = button.when_some(background, |button, background| button.bg(background));
    if matches!(emphasis, ButtonEmphasis::Default) {
        // Only a ghost gets a hover fill. §4.6 and `PROMPT` §2.1 #9: a pushed
        // button has a pressed state and no hover, because a hover on a push
        // button is a web convention no native tool has.
        button.hover(|style| style.bg(design::role::accent_wash(cx)))
    } else {
        button
    }
}

/// The icon-only half of [`recovery_button`], on the same target.
///
/// The box and the glyph are stated separately, which is the whole point: gpui-kit
/// derives a component button's icon from its own box, so a 28px box would have
/// drawn a 21px glyph and this bar's two dismiss controls would not have matched
/// the icon controls in any other bar.
fn recovery_icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    focus: &FocusHandle,
    tab_index: isize,
    label: &'static str,
    cx: &App,
) -> BaseButton {
    BaseButton::new(id)
        .track_focus(focus)
        .tab_stop(true)
        .tab_index(tab_index)
        .accessibility_label(label)
        .size(design::size::CONTROL)
        .rounded(design::radius::MD)
        // The resting ink of a glyph in a control, not `fg.tertiary`. This is an
        // icon-only button with no label to read, so the glyph is the whole
        // control — drawn at the placeholder tier beside the bar's `fg_secondary`
        // text it read as a disabled button, which is the one thing an active
        // dismiss control must not look like.
        .text_color(design::icon::resting(cx))
        // Hover is the wash, never a second ink on the glyph.
        .hover(|style| style.bg(design::role::accent_wash(cx)))
        .child(Icon::new(icon).with_size(Size::Size(design::icon::IN_TOOLBAR)))
}

/// The UI text face a subtree renders in.
fn ui_font(cx: &App) -> Font {
    font(cx.theme().font_family.clone())
}

/// The body-size label an empty state's own sentence is drawn in.
///
/// `body 13/400`, and not `caption`, for the reason `UI-SPEC` §2.3 gives: its four
/// levels separate by **2px and by weight**, and the example it names as failing is
/// `body 13` against `metadata 11`. An empty state's description is also the one
/// place §2.3 budgets a *paragraph* ("40ch（空状态说明）"), and `caption` is the
/// token for a short uppercase section label — carrying `+0.06em` tracking, which
/// is letterspacing for `RESTARTS` and not for a sentence.
///
/// This is `R1h`: the Dock's empty state made the same move for the same reason
/// (`panels/dock.rs`, `label_body(hint)`), and leaving the table on `caption` is
/// the failure R1 exists to catch — two empty states of the same shape, side by
/// side, at two different sizes for no reason either can name.
fn description_label(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::BODY)
        .line_height(design::text::BODY_LINE_HEIGHT)
}

/// The label a failure's own reason is drawn in.
///
/// `label 12/400`, because this is the same sentence the inline error bar shows
/// (`table-error-bar`, also `design::text::LABEL`) and §4.15 fixes both at 12/400.
/// It was `caption` 11, so the same words were 11px in the empty state and 12px in
/// the bar beside it — a difference too small to name and large enough to see as the
/// error moving when the table loses its rows.
fn reason_label(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT)
}

fn action_tooltip(label: &str, action: &dyn gpui_kit::Action, cx: &App) -> String {
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

/// F10, Shift+F10, and the Menu key open the row or column menu.
fn is_row_actions_key(keystroke: &gpui_kit::Keystroke) -> bool {
    if keystroke.modifiers.alt || keystroke.modifiers.platform || keystroke.modifiers.control {
        return false;
    }
    match keystroke.key.as_str() {
        "f10" => true,
        "menu" => !keystroke.modifiers.shift,
        _ => false,
    }
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

/// Renders a non-blocking notice: a write operation's outcome, in place.
///
/// This is the same shape as the error bar and it is not the same thing. A
/// notice says "the request I sent has not been confirmed yet, or failed", it
/// has a `\u{2713}`-shaped history of its own — a delete that is still waiting
/// for the server, a scale that came back with an error — and it carries no
/// control beyond dismissal. §4.15 governs the *state of the table*, which is
/// what the error bar is for; a notice about one row's write is a different
/// event and putting it in the error bar's slot would have made the bar mean
/// two things.
///
/// It stacks against the table's own bottom edge, with the error bar above it,
/// because that is the only slot a transient band can occupy without moving the
/// rows. It used to sit between the summary strip and the table body, which put
/// 28px between the header and the reader's reading position for eight seconds —
/// the length of [`NOTICE_DURATION`] — and then took it back. A band that
/// arrives on a timer must not be able to move anything above it.
///
/// A 6px dot rather than the 16px health glyph, for §4.4's reason: the dot
/// carries the severity, and a glyph per severity in a bar that can appear above
/// a table of a thousand rows is a second status vocabulary in the same screen.
fn notice_banner(notice: &Notice, cx: &App) -> gpui_kit::Stateful<Div> {
    // The dot is a mark and the sentence is a word, and the design system keeps
    // two inks per channel for exactly that split. One value for both held the
    // banner's message to the graphic floor while the error bar a band below it —
    // the same failure, in the same words, at the same size — wore the text floor.
    //
    // The dot's own ink is named by what it carries rather than picked from the
    // role ladder beside it: a severity is a mark's channel, and a dot that
    // repeats the word beside it is decoration.
    let (dot, ink) = match notice.severity {
        Severity::Error => (
            design::icon::status(cx, Severity::Error),
            design::role::danger_word(cx),
        ),
        Severity::Warning => (
            design::icon::status(cx, Severity::Warning),
            design::role::warning_word(cx),
        ),
        // §0 铁律三: an operation that merely succeeded is not a problem, so it
        // takes the quiet ink. The old code handed `Success` and `Info` the same
        // green wash, so "it worked" and "here is something to know" were one
        // colour at the one place a reader is being told them apart.
        _ => (design::icon::incidental(cx), design::role::fg_secondary(cx)),
    };
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
        // A named integer has no selector a test can look up, and this band is
        // one of the two that stack against the table's own bottom edge, so the
        // invariant that they never move the rows is only testable if this band
        // answers to a name.
        .debug_selector(|| "table-notice".to_owned())
        .role(role)
        .aria_label(notice.message.clone())
        .w_full()
        .flex_none()
        .px(TABLE_CONTENT_INSET)
        .h(design::size::OPEN_VIEWS)
        .gap(design::space::SM)
        .items_center()
        .font(ui_font(cx))
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT);
    if let Some(detail) = notice.detail.clone() {
        banner = banner.aria_description(detail);
    }
    let tooltip = notice
        .detail
        .clone()
        .unwrap_or_else(|| SharedString::from("Dismiss message"));
    banner.interactivity().tooltip(text_tooltip(tooltip));
    banner
        .child(
            div()
                .size(design::size::STATUS_DOT)
                .flex_none()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(ink)
                .whitespace_nowrap()
                .text_ellipsis()
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
    // A pending delete is a warning — the row is about to stop existing — and a
    // pending scale or restart is not a problem at all, so it takes the quiet
    // role. The old code gave both an icon glyph, which put two more shapes in
    // the same cell as the 6px status dot the spec asks for.
    //
    // The dot keeps the mark ink and the label takes the word ink: a chip is the
    // one place in the table that prints a status word on a wash of its own
    // channel, which is the case the design system's second ink per channel
    // exists for.
    let (background, dot, ink) = match op {
        PendingOp::Delete => (
            design::role::warning_wash(cx),
            design::icon::status(cx, Severity::Warning),
            design::role::warning_word(cx),
        ),
        PendingOp::Scale { .. } | PendingOp::Restart => (
            design::role::accent_wash(cx),
            design::icon::resting(cx),
            design::role::fg_secondary(cx),
        ),
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
        .rounded(design::radius::SM)
        .bg(background)
        .text_color(ink)
        .font(ui_font(cx))
        .text_size(design::text::CAPTION)
        .line_height(design::text::CAPTION_LINE_HEIGHT)
        .whitespace_nowrap();
    badge.interactivity().tooltip(text_tooltip(detail));
    badge
        .child(
            div()
                .size(design::size::STATUS_DOT)
                .flex_none()
                .rounded_full()
                .bg(dot),
        )
        .child(SharedString::from(op.label()))
        .when(seconds > 0, |badge| {
            badge.child(
                div()
                    .text_color(design::role::fg_tertiary(cx))
                    .child(SharedString::from(format!("{seconds}s"))),
            )
        })
        .into_any_element()
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

/// Reports whether a resource name answers to a type-ahead prefix.
///
/// A plain `starts_with` is almost useless on a Kubernetes list: every name in a
/// kind shares a prefix, so `coredns-7d9f4b-abcde` and `coredns-canary-5f6g7h`
/// are both found by `coredns` and by nothing a reader would actually type. What
/// a reader types is the *workload* — `core`, `api`, `web` — and a Pod's name is
/// `<workload>-<suffix>-<hash>`, so the workload is the first segment and the
/// distinguishing part is whichever segment they mean.
///
/// A match is therefore the whole name, or any `-`-separated segment of it, and
/// not the hash: a reader types names, and typing a hash is what the clipboard is
/// for.
fn name_matches_prefix(name: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let name = name.to_lowercase();
    name.starts_with(prefix) || name.split('-').any(|segment| segment.starts_with(prefix))
}

/// Maps one status cell to the health channel, graded by how long it has said so.
///
/// `Unknown` states no verdict at all: a pod the kubelet lost track of is not a
/// pod that is waiting, and painting the two identically made the app lie in the
/// one case where the reader most needs the difference.
///
/// `Pending` is the one status whose text alone is not a verdict, and this is
/// the most load-bearing function in the file. A cluster mid-rollout has
/// thousands of `Pending` pods, and `UI-SPEC` §0 铁律三 grades them by age: a pod
/// scheduled four seconds ago is doing what a scheduled pod does and reads grey
/// like any healthy row, one that has been waiting a minute is worth a second
/// look, and one that has been waiting five minutes is stuck and has to be
/// findable among the thousands that are not. A single `Pending → warning` rule
/// paints all of them the same colour, which is a table where the reader has
/// nothing to look at.
///
/// `design::pod_severity` has no age input and is a shared contract this file
/// does not own, so the grade lives here where the table resolves severity and
/// `design` keeps owning the plain text mapping.
fn status_severity(status: &str, age: Option<Duration>) -> Severity {
    match status {
        "NotReady" | "Degraded" | "Unavailable" => Severity::Warning,
        "Pending" | "ContainerCreating" => match age {
            // No timestamp means the cluster has not told us when it started, so
            // the honest reading is the quiet one: claiming a pod is stuck on the
            // strength of a missing field is the same class of lie as reading
            // "Unknown" as healthy.
            None => Severity::Success,
            Some(age) if age > PENDING_DANGER_AFTER => Severity::Error,
            Some(age) if age > PENDING_WARNING_AFTER => Severity::Warning,
            Some(_) => Severity::Success,
        },
        // `Unknown` is handled by `design::pod_severity`, which owns the split and
        // has a test for it. A local copy of the same rule is a rule that can drift.
        _ => design::pod_severity(status),
    }
}

/// The age of a row, which is the input the `Pending` grade is a function of.
fn row_age(row: &Row) -> Option<Duration> {
    age_seconds(&row.obj).map(Duration::from_secs)
}

/// The channel a status cell draws, and *whether* it needs a row re-solve.
///
/// `UI-SPEC` §0 铁律三 inverts the usual reading: a healthy resource is grey, and
/// only a problem — `Warning`/`Error` (`Info` counts as a claim) — gets a colour.
/// That is the split [`Severity::marker_on`] / [`Severity::word`] make once, in
/// the product's single resolver, so the cell draws its channel from there rather
/// than re-deriving it from the *mark* role and re-lifting. But the *solve* the
/// resolver applies is only honest for a coloured claim: a grey mark or word on
/// the neutral ladder (`fg_tertiary`/`fg_secondary`) is already at its text floor
/// and must stay exactly on its rung — a 3:1 graphic solve would lift `fg_tertiary`
/// brighter than the word beside it. So only the loud channels take the row
/// re-solve (the wash under a selection can smudge them); the quiet ink returns
/// untouched, as it always has.
fn status_channel(severity: Severity, row_background: Hsla, graphic: bool, cx: &App) -> Hsla {
    match severity {
        // A coloured claim: the single resolver gives the channel ink, and the
        // row re-solve lifts it off the selection wash if it has to.
        Severity::Warning | Severity::Error | Severity::Info => {
            let ink = if graphic {
                severity.marker_on(cx, row_background)
            } else {
                severity.word(cx)
            };
            status_ink_on_row(ink, row_background, graphic, cx)
        }
        // A quiet state (healthy, `<30s` Pending, neutral): the exact ink-ladder
        // rung, never a graphic-solved lift — see the doc above.
        _ if graphic => design::role::fg_tertiary(cx),
        _ => design::role::fg_secondary(cx),
    }
}

/// The dot a status cell draws: the channel's mark, solved only when it is a claim.
fn status_dot_ink(severity: Severity, row_background: Hsla, cx: &App) -> Hsla {
    status_channel(severity, row_background, true, cx)
}

/// The word a status cell draws: the same channel's word role, one step above its
/// dot, solved only when it is a claim.
fn status_word_ink(severity: Severity, row_background: Hsla, cx: &App) -> Hsla {
    status_channel(severity, row_background, false, cx)
}

/// Re-solves one status ink against the surface the row is actually painted on.
///
/// The channel comes from [`Severity::marker_on`] / [`Severity::word`] — the
/// product's single severity resolver — so a status dot and word are never
/// derived from the *mark* role and re-lifted here. This wrapper then applies
/// the one row-specific adjustment the resolver does not know about: three of
/// the five row states are a wash over the content surface (selection, muted
/// selection, keyboard cursor), and a word solved against the bare surface can
/// smudge against that 20% wash. This walks the ink away from the row until it
/// clears the same floor the resolver already promises — 3:1 for the dot
/// (graphic), 4.5:1 for the word (text), raised under Increase Contrast.
fn status_ink_on_row(ink: Hsla, row_background: Hsla, graphic: bool, cx: &App) -> Hsla {
    let increased = crate::settings::increase_contrast_enabled(cx);
    let minimum = match (graphic, increased) {
        (true, false) => design::MARKER_MIN_CONTRAST,
        (true, true) => design::INCREASED_CONTRAST_GRAPHIC_MIN,
        (false, false) => design::TEXT_MIN_CONTRAST,
        (false, true) => design::INCREASED_CONTRAST_TEXT_MIN,
    };
    design::graphic_on_with_minimum(row_background, ink, minimum)
}

/// What a screen reader is told about a status, including the age grade.
///
/// The verdict used to exist only as a glyph, so a reader heard "Status:
/// Pending" and never learned that Pending is a warning — and with the grade,
/// never learned that one Pending pod is stuck and nine thousand are not. The
/// words are the only channel this has.
fn status_accessible_label(status: &str, age: Option<Duration>) -> String {
    let severity = status_severity(status, age);
    let verdict = design::health_label(severity);
    if !matches!(status, "Pending" | "ContainerCreating") {
        return verdict.to_owned();
    }
    let Some(age) = age else {
        return format!("{verdict} \u{2014} the cluster has not reported when it started");
    };
    let grade = match severity {
        Severity::Error => "stuck",
        Severity::Warning => "waiting",
        _ => "scheduled",
    };
    format!("{verdict}, {grade} for {}", format_age(age))
}

fn row_accessible_label(
    columns: &[ResourceColumn],
    visible: &[usize],
    cells: &[CellValue],
    age: Option<Duration>,
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
        .map(|cell| status_accessible_label(cell.text.trim(), age));
    match (label.is_empty(), verdict) {
        (false, Some(verdict)) => format!("{label}, Health: {verdict}"),
        (true, Some(verdict)) => format!("Health: {verdict}"),
        (false, None) => label,
        (true, None) => "Resource row".to_owned(),
    }
}

/// Describes how one table row is painted.
///
/// There is no `Stripe`. `UI-SPEC` §4.4 and `PROMPT` §2.1 #6 both say 无, and a
/// zebra is not a decoration: it is a second thing the eye has to read on every
/// row, competing with the one thing the row is *for*. The row height carries
/// the rhythm instead.
///
/// ### The six states, and what separates each from its neighbour
///
/// The guide asks a data table to "distinguish focus, hover, active row, and
/// multi-selection". Every one of them has to be told apart without being told
/// apart *twice*, so the ladder spends one channel at a time and stops:
///
/// | state | wash | rail | focus edge | ink |
/// |---|---|---|---|---|
/// | rest | the content surface | — | — | the column's own declaration |
/// | hover | the solved hover wash, *composited over* whatever the row already is | — | — | unchanged |
/// | keyboard cursor | `row_focus_bg` | accent rail | hairline around the row |
/// | selected, focused, and the row the inspector is showing | `row_selection_bg` | accent rail | hairline around the row |
/// | selected, table unfocused | `row_selection_bg`, unchanged | accent rail | — |
/// | range member | `row_selection_bg`, unchanged | — | — |
///
/// Four things follow from that table and all four are load-bearing:
///
/// * **Hover is one wash, composited.** `selected + hover` is the *same* signal at
///   a stronger strength, so the hover is laid over the row's own state rather
///   than swapped in for it. Swapping it replaced the accent wash with the plain
///   hover wash at the exact moment the reader was about to act.
/// * **A member is quieter than the row the inspector is showing.** The two used
///   to take the same fill and the difference was 1.018:1 against the unselected
///   row beside them — invisible — while every selected row took the *full*
///   selection wash, so a 200-row range painted 200 slabs at the strength meant
///   for one. The active row keeps the strong wash and the rail, because it is
///   the one row a command acts on; every other selected row takes the quiet
///   wash, which is a step above hover and a step below the cursor. A range
///   therefore reads as *one* marked row among several tinted ones, and a
///   single selection reads as a marked row alone.
/// * **Focus is a shape, not a tint.** Keyboard focus used to be told from
///   selection by four points of accent alpha, which is a difference a reader
///   cannot see and a greyscale screenshot cannot show at all. The focused row
///   now carries a `border.strong` hairline around it as well as its wash, so the
///   cue is a frame rather than a slightly bluer band, and hiding the colour still
///   leaves the keyboard's position readable.
/// * **The inspector's row is the active row.** The selection is what drives the
///   Inspector, so there is no seventh state to paint and no way for the two to
///   disagree about which row they mean. The cell inks are not stepped up on a
///   washed row: [`columns::CellInk::color`] already solves every level against
///   the surface the cell is drawn on, so a `fg.secondary` on a 20% accent wash
///   is that wash's `fg.secondary` rather than the plain surface's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowVisual {
    /// Not selected, no cursor.
    Plain,
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

    /// Reports whether the row is the one the keyboard is on.
    ///
    /// This is the cue that has to survive greyscale, so it is drawn as a shape —
    /// a `border.strong` frame round the whole row — and not as a fourth
    /// alpha of accent. Focus used to be the same 2px accent rail the selection uses, and on a
    /// selected row the rail, the wash and the focus were three readings of one
    /// hue: a reader who cannot separate the hues (or a screenshot reduced to
    /// luminance) saw one selected row and could not say which one the next
    /// keystroke would act on.
    fn is_focused(self) -> bool {
        matches!(self, Self::SelectedFocused | Self::FocusRing)
    }
}

/// Resolves one row's visual state.
///
/// A selected row keeps its fill when the table loses focus, but the fill steps
/// down so the active row stays findable. Only the active row of a multi-row
/// selection keeps the rail, so the rail always means "this is the row a command
/// acts on".
///
/// `keyboard_focus` and `table_focused` are different questions and the function
/// needs both. `contains_focused` is true after a click, so gating the ring on it
/// painted the same rail for a `\u{2193}` and for a click on row one — which is
/// exactly the difference `PROMPT` §2.1 #10 calls the line between a considered
/// interface and an amateur one. The ring is the keyboard's; the fill is
/// everybody's.
fn row_visual(
    selected: bool,
    anchor: bool,
    table_focused: bool,
    keyboard_focus: bool,
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
    // The cursor sits on the first row before anything is selected, and only
    // when the keyboard got here. A click leaves no cursor behind, so the row
    // under the pointer is not left wearing a keyboard affordance.
    if table_focused && keyboard_focus && !has_selection && index == 0 {
        return RowVisual::FocusRing;
    }
    RowVisual::Plain
}

/// The wash a selected row takes, whenever it was selected.
///
/// One value for all three selected states on purpose. Selection is a fact about
/// the resource, not about where the keyboard is: a reader who alt-tabs away and
/// comes back has to find the same rows selected, so a row that lost focus keeps
/// its fill exactly. It used to step *down* (`design::row_selected_bg` at 20%
/// accent focused, a hand-scaled 11% unfocused), and the weaker step is what made
/// a selection look like it had evaporated.
///
/// It is `role::accent_wash` — the theme's own accent wash at its own alpha —
/// rather than a strength of accent picked here, for two reasons. It is a quarter
/// lighter than the wash it replaces, which is the whole of the "muddy slab that
/// reads heavier than the data" defect: at 20% the band was darker and bluer than
/// the name it was supposed to be sitting behind. And `Roles::text_surfaces`
/// already solves every ink against exactly this plane
/// (`composite_surface(surface_content, accent_wash)`), so the quiet selection is
/// a surface the type ladder was designed on rather than one invented to fill a
/// gap.
fn row_selection_bg(cx: &App) -> Hsla {
    design::composite_surface(table_row_surface(cx), design::role::accent_wash(cx))
}

/// Paints one row's visual state.
///
/// Three selected states, one fill: the selection is persistent, so the state the
/// reader can *see* does not answer a question they did not ask. What separates
/// the three is the rail and the focus edge, and both are shapes rather than
/// tints — which is what lets them survive greyscale, and what lets a
/// hundred-row range read as a range (many tinted rows, one of them marked)
/// rather than as a hundred equally loud slabs.
fn row_visual_background(visual: RowVisual, selected: Hsla, focused: Hsla, plain: Hsla) -> Hsla {
    match visual {
        RowVisual::SelectedFocused | RowVisual::SelectedUnfocused | RowVisual::SelectedMember => {
            selected
        }
        RowVisual::FocusRing => focused,
        RowVisual::Plain => plain,
    }
}

/// The background a row takes while the pointer is over it.
///
/// `UI-SPEC` §4.4 reads `hover` and `selected+hover` as the *same* signal at
/// two strengths: "accent.wash 再 +4%". So a plain row takes the solved hover
/// wash, and every row that already carries a state takes the same translucent
/// hover wash laid *over* that state. Nothing here invents a colour — the
/// overlay is the theme's own `element_hover`, the same 3.5% the plain row's
/// wash is solved from, so the two cannot drift apart.
///
/// `row_hover_bg` cannot be the overlay: it is an opaque colour already solved
/// against the table, so compositing it over a selection would land on the
/// hover colour and lose the selection again — which is the bug this exists to
/// remove.
fn row_hover_background(visual: RowVisual, background: Hsla, cx: &App) -> Hsla {
    if visual == RowVisual::Plain {
        return design::row_hover_bg(cx);
    }
    design::composite_surface(background, design::colors(cx).element_hover)
}

/// The surface every row of this table composites onto.
///
/// `DESIGN.md §3.4` files `surface` under tables and inputs. The table used to
/// paint on `canvas`, and the loading skeleton on the canvas too, so the one
/// level of the ramp a reader stares at for eight hours was never on screen. One
/// function for both keeps the two states from drifting apart.
fn table_row_surface(cx: &App) -> Hsla {
    design::role::surface_content(cx)
}

/// The surface the column header band paints on.
///
/// `surface_raised`, one step above the content plane the rows sit on, and the
/// step the role ladder names for a table header. The header and the body were
/// the same value, so the band read as a fifth row rather than as the chrome
/// above them — and the fix is not a hairline, because a hairline cannot say
/// "this band is a different kind of thing", only "these two things touch".
fn table_header_surface(cx: &App) -> Hsla {
    design::role::surface_raised(cx)
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

/// The face and line every cell in the body renders in.
///
/// `D25` measured the mixed table and rejected it: JetBrains Mono at 13px is
/// heavier than Inter at 13px, and a row with the status in sans and the numbers
/// in mono reads as two different products. The whole body is therefore the UI
/// face at the body token, and mono survives only in the long-text class — an
/// image tag, an IP, a port, a cron expression — where the value genuinely *is*
/// code and the monospace advance is the point.
///
/// The `tnum` features come from the data typography, because tabular figures
/// are the only reason a number column aligns: without them `1` is narrower than
/// `8` and a right-aligned count column staircases. A number column in a
/// proportional face without `tnum` is a number column a reader cannot scan.
///
/// The line is the reader's configured data line when they have raised it, and
/// the body token otherwise. A reader who enlarges text to read it gets a taller
/// row rather than a clipped one, which is the whole point of the setting.
fn table_typography(cx: &App) -> DataTypography {
    let configured = DataTypography::from_theme_settings(cx);
    DataTypography {
        features: configured.features,
        font: ui_font(cx),
        size: design::text::BODY.max(configured.size),
        line_height: design::text::BODY_LINE_HEIGHT.max(configured.line_height),
    }
}

/// Returns the row height: the density floor, the line, and one pixel of slack.
///
/// This looks like [`design::row_height`] and is deliberately **not** it. That
/// function floors at [`design::size::ROW`], which is the product's *comfortable*
/// default, so calling it here made `Normal` and `Dense` both 32px and the
/// density setting stopped moving the row at all. The density is the row token for
/// this table, so the three clauses are the design's with the first one swapped:
/// at least the density's floor, at least the configured line, and a pixel more
/// than the line.
///
/// The slack is the design's and is not decoration: the shared table hands a cell a
/// box one pixel shorter than the row, so a row sized to exactly its line crops
/// the line and the crop shows as text sitting a hair high in its row.
///
/// A `design::row_height` that took the floor as an argument would say this once
/// instead of twice. It does not, so the substitution is written here and the
/// reason is written next to it.
fn row_height(cx: &App) -> Pixels {
    let line = table_typography(cx).line_height;
    let floor = Density::read(cx).floor();
    line.max(floor).max(line + design::border::LINE)
}

/// Returns how many rows fit in a viewport at the current text size.
fn rows_in_viewport(height: f32, row_height: Pixels) -> usize {
    (height / f32::from(row_height)).floor().max(1.0) as usize
}

/// The three orders one column can be in, and the step between them.
///
/// The third state is a real sort on the default column rather than a silent "no
/// sorting": an unsorted table still falls back to that order, so claiming "not
/// sorted" while the rows run by Name would be a false affordance.
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

/// The measured geometry of one cell value, used to size its tooltip.
#[derive(Clone, Debug)]
struct CellShape {
    width: Pixels,
    font: Font,
    features: FontFeatures,
    size: Pixels,
}

/// Returns the text area inside a cell: the column width minus both paddings.
///
/// The identifier column is charged one more hairline, and the reason is measured
/// rather than theoretical. `UI-SPEC` §4.4 gives the first column "右侧 1px
/// `border.subtle` 作分界", that rule sits *inside* the column's 252px, and the
/// table lays its cells out at the fractional widths `fit_columns` resolved — on
/// the shipping 1920px window the Name column's text box is 219px where this
/// function said 220. A name shortened to fit 220 then lost its last glyph to a
/// 1px overflow, and the cell's own `text_ellipsis` answered by adding a second
/// ellipsis: `nightly-reindex-warehouse-…-dpmz9` shipped as
/// `nightly-reindex-warehouse-…-dp…`. Charging the rule here fixes all three
/// readers of the width at once — the middle ellipsis, the overflow test and the
/// tooltip — because they all ask this function.
///
/// This is also the one truncation policy, in one place. Two rules, and every
/// column obeys both:
///
/// * **A name is cut in the middle** (`UI-SPEC` §10.1), because a Kubernetes
///   name's hash is at its tail and a tail ellipsis deletes the half that tells
///   one pod from another.
/// * **Everything else is cut at the tail**, because a namespace, a node or an
///   image tag is identified by its prefix — and a value that does not fit in
///   either carries its full text in a tooltip.
///
/// What is *not* here is a per-column truncation width, and that is the point: a
/// column's resolved width is a consequence of the reader's window and their
/// drags, so a policy stated in widths is a policy that changes under them. This
/// function takes the resolved width and asks the same question of every column,
/// which is why dragging one wider cannot silently give it a different
/// truncation rule from its neighbour.
fn cell_text_width(column: Option<&ResourceColumn>, width: Pixels) -> f32 {
    let width = if f32::from(width) > 0.0 {
        f32::from(width)
    } else {
        column.map_or(0.0, ResourceColumn::default_width)
    };
    // Both sides of the cell are padded, and `UI-SPEC` §10.1's padding is 16.
    let padding = 2.0 * f32::from(design::space::LG);
    let divider = if column.is_some_and(|column| column.class == ColumnClass::Identifier) {
        1.0
    } else {
        0.0
    };
    (width - padding - divider).max(0.0)
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

/// The strip's word for rows that are not live, carrying how old they are.
///
/// The duration is part of the word rather than a figure beside it because the
/// reader is not comparing it to the counts next to it — they are deciding
/// whether the row under the cursor is safe to act on, and `Stale data` on its
/// own does not answer that.
fn freshness_word(label: &str, age: Option<Duration>) -> SharedString {
    match age {
        Some(age) => SharedString::from(format!(
            "{label} \u{b7} {} old",
            design::format::age(age.as_secs())
        )),
        None => SharedString::from(label),
    }
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
///
/// `UI-SPEC` §4.13 requires four *kinds* to be told apart, and the distinction
/// that matters is not cosmetic: "there is nothing here" and "your filters hid
/// it" send a reader to opposite ends of the interface, and a state that cannot
/// say which one it is sends them to the wrong one first. The recovery states
/// below the four are a table-specific vocabulary on top of the same shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptyState {
    /// Under 200ms: §4.14 says show nothing, and the delegate never asks.
    Loading,
    /// The reader paused the watch and there is nothing cached.
    Paused,
    /// The watch stopped and there is nothing cached to fall back on.
    Stale,
    /// The list itself failed and there is nothing cached.
    Failed,
    /// A kind the design declined a table for: an Event is a timeline.
    NoTimeline,
    /// A kind the app has no columns for. A gap, and the state says so.
    UnknownKind,
    /// Rows exist and a filter hid all of them.
    NoFilterMatch,
    /// Rows exist and the status filter hid every healthy one.
    NoProblems,
    /// Rows exist and the scope is genuinely empty.
    NoRows,
    /// The list failed on a permission boundary, which is a different problem
    /// with a different way forward than a network one.
    Forbidden,
}

/// Resolves the empty state for the current table conditions.
fn empty_state_kind(
    status: &TableStatus,
    filter: &str,
    kind: &str,
    snapshot: Option<&IndexSnapshot>,
    problems_only: bool,
) -> EmptyState {
    // A permission failure is its own state, not a generic failure: it is the one
    // failure where retrying changes nothing, and §4.13 asks for copy that says
    // what the identity *can* do rather than only what it cannot.
    if let TableStatus::Failed(reason) = status
        && is_permission_failure(reason)
    {
        return EmptyState::Forbidden;
    }
    match status {
        TableStatus::Listing => EmptyState::Loading,
        TableStatus::Paused if snapshot.is_none_or(|snapshot| snapshot.rows.is_empty()) => {
            EmptyState::Paused
        }
        TableStatus::Stale(_) => EmptyState::Stale,
        TableStatus::Failed(_) => EmptyState::Failed,
        _ if declines_table(kind) => EmptyState::NoTimeline,
        _ if !is_known_kind(kind) => EmptyState::UnknownKind,
        // The status filter comes first. With a status filter on and a name
        // that happens not to match, the empty state blamed the name for a
        // filter the reader never typed.
        _ if problems_only => EmptyState::NoProblems,
        _ if !filter.is_empty() => EmptyState::NoFilterMatch,
        _ => EmptyState::NoRows,
    }
}

/// Reports whether a source reason is an RBAC refusal rather than a broken
/// cluster.
///
/// Two phrases, because the API server has two: a `Forbidden` status and an
/// `Unauthorized` one. Both mean the same thing to a reader — this identity is
/// not allowed to do this — and both mean something completely different from a
/// connection failure, so they cannot share a state.
fn is_permission_failure(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("forbidden") || reason.contains("unauthorized")
}

/// Says how many filters are hiding rows, which §4.13 makes the reader's only
/// clue that the scope is not empty.
///
/// The grammar follows the number rather than the noun, because "1 filter are
/// active" is the kind of detail that makes an interface feel machine-written.
fn active_filters_copy(count: usize) -> String {
    if count == 1 {
        "1 filter is active.".to_owned()
    } else {
        format!("{count} filters are active.")
    }
}

struct EmptyStateContext<'a> {
    status: &'a TableStatus,
    filter: &'a str,
    snapshot: Option<&'a IndexSnapshot>,
    spec: &'a ResourceSpec,
    action_focus: &'a FocusHandle,
    /// Makes the failure reason reachable by keyboard and screen readers.
    reason_focus: &'a FocusHandle,
    /// Hides rows that are healthy.
    problems_only: bool,
    /// How many filters are active, so the "filtered out" state can say the
    /// number rather than implying it.
    active_filters: usize,
    /// Whether the focus the reason line holds came from a key.
    reason_keyboard_focus: bool,
}

/// The one way forward an empty state offers.
///
/// Each variant carries its control, its tooltip and the action it runs, so an
/// empty state's arm is one line and the four attributes that make a recovery
/// control a recovery control live in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    /// Resumes the watch the reader paused.
    Resume,
    /// Restarts a watch that stopped.
    RetryLive,
    /// Retries the initial list. The one control that repairs the table.
    RetryList,
    /// Restarts the source, for a list that arrived empty or cannot be read.
    Refresh,
    /// Clears every filter at once, which is what the "filtered out" state
    /// offers. One button that undoes the whole narrowing beats two buttons that
    /// each undo a part of it and leave the reader guessing which is which.
    ClearFilters,
    /// Turns off the status filter the reader never typed.
    ShowEveryRow,
    /// Clears a filter that is hiding every row behind a stopped watch.
    ClearHidingFilter,
}

impl Recovery {
    /// The control's element id, its label and how loud it looks.
    fn button(self) -> (&'static str, &'static str, ButtonEmphasis) {
        match self {
            Self::Resume => ("empty-resume", "Resume", ButtonEmphasis::Default),
            Self::RetryLive => (
                "empty-stale-action",
                RETRY_LIVE_UPDATES,
                ButtonEmphasis::Default,
            ),
            Self::RetryList => (
                "empty-error-retry",
                RETRY_LOADING_RESOURCES,
                ButtonEmphasis::Accent,
            ),
            Self::Refresh => ("empty-refresh", REFRESH_RESOURCES, ButtonEmphasis::Default),
            // §4.13 asks for this one to be the screen's `primary`: it is the
            // state where the reader came to do something and a single click undoes
            // whatever stopped them, and the accent is spent on exactly that.
            Self::ClearFilters => (
                "empty-clear-filter",
                "Clear filters",
                ButtonEmphasis::Accent,
            ),
            Self::ShowEveryRow => (
                "empty-show-all",
                NO_PROBLEMS_ACTION,
                ButtonEmphasis::Default,
            ),
            Self::ClearHidingFilter => (
                "empty-stale-action",
                "Clear filter",
                ButtonEmphasis::Default,
            ),
        }
    }

    /// The control's own words plus the key that runs it.
    fn tooltip(self, cx: &App) -> String {
        match self {
            Self::Resume => action_tooltip("Resume live updates", &ToggleUpdates, cx),
            Self::RetryLive => action_tooltip(RETRY_LIVE_UPDATES, &Refresh, cx),
            Self::RetryList => action_tooltip(RETRY_LOADING_RESOURCES, &Refresh, cx),
            Self::Refresh => action_tooltip(REFRESH_RESOURCES, &Refresh, cx),
            // A filter nobody typed has no shortcut of its own: the route to it
            // is the Status column's own control.
            Self::ClearHidingFilter => action_tooltip("Clear Resource Filter", &ClearFilter, cx),
            Self::ClearFilters => "Clear every active filter".to_owned(),
            Self::ShowEveryRow => NO_PROBLEMS_ACTION.to_owned(),
        }
    }

    /// Runs the command from whichever control dispatched it.
    fn dispatch(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Resume => window.dispatch_action(Box::new(ToggleUpdates), cx),
            Self::RetryLive | Self::RetryList | Self::Refresh => {
                window.dispatch_action(Box::new(Refresh), cx)
            }
            Self::ClearFilters | Self::ShowEveryRow | Self::ClearHidingFilter => {
                window.dispatch_action(Box::new(ClearFilter), cx)
            }
        }
    }
}

/// Draws one empty state.
///
/// `UI-SPEC` §4.13, in full: a 24px `fg.tertiary` icon, one line of `title`
/// 15/600 at `fg.primary`, **no description by default**, and at most one action.
/// Web applications over-explain an empty state; a native one states the fact and
/// offers the way forward, and anything more is a sentence the reader has to read
/// to learn what a one-line title already said.
///
/// Three of the four kinds deliberately have *no* description, and each of the
/// three omits it for a different reason: "真的没有" has nothing to add, "被筛掉
/// 了" is told by its own action, and an empty load is not an empty scope. The
/// two that keep one — filtered out and forbidden — are the two where the reason
/// is the whole message, and `§4.13` names a line for exactly those.
///
/// The title is `design::text::TITLE` and not `BODY`: it was 13px, the same size
/// as a cell value, which made the one piece of chrome on the screen
/// indistinguishable from the thing it was standing in for.
fn empty_state(context: EmptyStateContext<'_>, window: &Window, cx: &App) -> AnyElement {
    let EmptyStateContext {
        status,
        filter,
        snapshot,
        spec,
        action_focus,
        reason_focus,
        problems_only,
        active_filters,
        reason_keyboard_focus,
    } = context;
    let plural = spec.label_lower();
    let failed_reason = match status {
        TableStatus::Failed(reason) => Some(reason.clone()),
        _ => None,
    };
    let (icon, title, detail, action, label) = match empty_state_kind(
        status,
        filter,
        spec.kind.as_ref(),
        snapshot,
        problems_only,
    ) {
        EmptyState::Loading => (
            IconName::LoaderCircle,
            format!("Loading {plural}"),
            // §4.14: under 200ms the delegate never asks for this at all, so
            // reaching here means the wait is real and a line saying so is the
            // information rather than the explanation.
            None,
            None,
            format!("Loading {plural}"),
        ),
        EmptyState::Paused => (
            IconName::Pause,
            "Updates are paused".to_owned(),
            None,
            Some(Recovery::Resume),
            format!("Live updates are paused. The {plural} list is not loading."),
        ),
        EmptyState::Stale => (
            IconName::TriangleAlert,
            "Live updates stopped".to_owned(),
            // A filter that hides every row is not the reason updates stopped,
            // so that state offers the one control that clears it.
            if filter.is_empty() {
                Some(RETRY_LIVE_UPDATES_GUIDANCE.to_owned())
            } else {
                Some("Clear the filter to view the last available rows.".to_owned())
            },
            Some(if filter.is_empty() {
                Recovery::RetryLive
            } else {
                Recovery::ClearHidingFilter
            }),
            "Live updates stopped. Showing the last available data.".to_owned(),
        ),
        EmptyState::Failed => (
            IconName::TriangleAlert,
            format!("Loading {plural} failed"),
            Some(RETRY_LOADING_RESOURCES_GUIDANCE.to_owned()),
            Some(Recovery::RetryList),
            format!("Loading {plural} failed"),
        ),
        // `UI-SPEC` §10.3: an event is a timeline. Saying so here is the point —
        // without it, a reader who opens Events and finds no table has been told
        // the cluster has no events, which is a lie about a cluster that is full
        // of them.
        EmptyState::NoTimeline => (
            IconName::Info,
            "Events are a timeline".to_owned(),
            Some("Events are shown on an object's page and in Activity.".to_owned()),
            None,
            "Events are not a table. They are shown on an object's page and in Activity."
                .to_owned(),
        ),
        // An unknown kind has no column layout, so an empty list is expected.
        // The list can still arrive late, so the state offers a way forward.
        EmptyState::UnknownKind => (
            IconName::Info,
            UNKNOWN_KIND_TITLE.to_owned(),
            Some(UNKNOWN_KIND_GUIDANCE.to_owned()),
            Some(Recovery::Refresh),
            format!("{UNKNOWN_KIND_TITLE} for {plural}"),
        ),
        // §4.13's "被筛掉了": the *number* is the whole content of this state, and
        // the title names the *filter* rather than leaving "no results" to imply an
        // empty cluster. It read `No pods match`, which is a fragment rather than a
        // sentence: it said what did not happen without saying what did, so the
        // action underneath it was the only thing on screen that named the cause.
        EmptyState::NoFilterMatch => (
            IconName::Search,
            format!("No {plural} match this filter"),
            Some(active_filters_copy(active_filters)),
            Some(Recovery::ClearFilters),
            format!(
                "No {plural} match the current filters. {}",
                active_filters_copy(active_filters)
            ),
        ),
        // The reader never typed a filter, so "Clear the filter" pointed at a
        // control that had nothing to do with the state on screen. The filter
        // that is actually on is the status one.
        EmptyState::NoProblems => (
            IconName::Check,
            format!("No {plural} need attention"),
            Some(NO_PROBLEMS_GUIDANCE.to_owned()),
            Some(Recovery::ShowEveryRow),
            format!("No {plural} need attention"),
        ),
        // A live table with no rows still needs one way forward. §4.13's table
        // asks for `Create pod`; there is no create path behind this table
        // (`ObjectOps` is delete, scale and restart), so the one action is the
        // one that can actually do something. Reported rather than faked with a
        // button that opens nothing.
        //
        // No description, and that is the design's decision rather than an
        // omission: four of the nine states carry a sentence and this one does
        // not, because `No pods in this scope` *is* the sentence and a second line
        // under it would repeat the title's own words at a lower emphasis. The
        // block's vertical centring is also measured on the title, so a state that
        // has a description is centred on the group and one that does not is
        // centred on the title; a state that has both is centred twice.
        EmptyState::NoRows => (
            design::kind_icon(spec.kind.as_ref()),
            format!("No {plural} in this scope"),
            None,
            Some(Recovery::Refresh),
            format!("No {plural} in this scope"),
        ),
        // §4.13: a permission error says what the identity *can* do. "Forbidden"
        // on its own is the API server's word for a problem, not an explanation
        // of one, and the raw RBAC JSON is worse: it is unreadable, it is long,
        // and it does not answer the only question the reader has.
        EmptyState::Forbidden => (
            IconName::TriangleAlert,
            format!("Cannot list {plural}"),
            Some(format!(
                "This identity has no permission to list {plural} in this scope. It can still list \
                 namespaces, and the ClusterRole that grants it is named in the error below."
            )),
            Some(Recovery::RetryList),
            format!("Cannot list {plural}: this identity is not permitted to."),
        ),
    };
    // One icon size for all table empty states, and the lead lane's own
    // number, so the same empty state is not a different size here than in
    // every sibling panel.
    let icon_size = Size::Size(design::icon::LEAD);
    let spinning = icon == IconName::LoaderCircle;
    // The lead mark takes the resting ink: one rung under its own title, which is
    // the relationship every other mark/word pair in the product has, and quiet
    // enough that the title is still the thing being read. §4.13 is explicit
    // that it must not be a *coloured* large icon — a 40px red disc in the middle
    // of a table is a web dialog's apology, and a red one reads as a second thing
    // that went wrong on top of the thing that did — and that rule is about hue,
    // which left the tier below unclaimed. It was sitting there, and
    // `fg.tertiary` is the placeholder and count rung: a mark the reader has to
    // identify, drawn at the ink the product uses for text nobody must read.
    let lead_ink = design::icon::resting(cx);
    let glyph: AnyElement = if spinning {
        // The shared spinner stops when the reader asked for less motion.
        crate::panels::common::spinner(icon, lead_ink, icon_size)
    } else {
        div()
            .id("empty-icon")
            .debug_selector(|| "empty-icon".to_owned())
            .child(Icon::new(icon).with_size(icon_size).text_color(lead_ink))
            .into_any_element()
    };
    // The reason is readable on screen; the raw text stays in the tooltip, so a
    // long server message cannot push the state off.
    let mut lines = v_flex()
        .w_full()
        .min_w(px(0.0))
        .items_center()
        .gap(design::space::XS);
    if let Some(reason) = failed_reason.as_deref() {
        // Gated on the keyboard's origin like every other ring in this file: a
        // click into a reason line used to stroke it exactly as a `Tab` did.
        let focused = reason_focus.is_focused(window) && reason_keyboard_focus;
        lines = lines.child(
            div()
                .id("empty-error-reason")
                .debug_selector(|| "empty-error-reason".to_owned())
                .track_focus(reason_focus)
                .tab_index(0isize)
                .role(Role::Note)
                .aria_label(user_reason(reason, &plural))
                .aria_description(reason)
                .text_center()
                .child(
                    // A sentence in a word role. The `danger` mark role is solved
                    // for a 6px dot and a 3px rule, and a 12px reason line read at
                    // arm's length is the one place the reader is *reading* the
                    // failure rather than spotting it.
                    reason_label(user_reason(reason, &plural))
                        .text_color(design::role::danger_word(cx)),
                )
                .when(focused, |note| {
                    note.border_1().border_color(design::role::accent(cx))
                }),
        );
    }
    if let Some(detail) = detail {
        lines = lines.child(
            div()
                .debug_selector(|| "empty-detail".to_owned())
                // `fg.secondary`, the ink the Dock's empty state gives the identical
                // slot. `fg.tertiary` is the placeholder and count rung: a 13px
                // paragraph the reader is meant to read once, drawn in it, reads
                // as a second thing the app is not going to tell them about, and
                // it left two same-shaped empty states a tier apart.
                .child(description_label(detail).text_color(design::role::fg_secondary(cx))),
        );
    }
    let state_element = Empty::new()
        // The dashed frame an `Empty` draws is a page-level cue. Inside a table
        // that already has its own frame it reads as a hole cut in the grid.
        .border_0()
        .p_0()
        // `Empty` grows (`flex_1`) so a page can centre itself in whatever is
        // left. Inside the table the action is a *sibling* of this block, not a
        // child, so growing pushed it to the bottom of the viewport: the title
        // sat in the middle of the table and `Clear filters` sat 450px below it
        // against the status bar. §4.13 asks for icon + one line + action as one
        // centred group with 48px above and below, and the wrapper's own
        // `justify_center` is what puts the group there.
        .flex_none()
        .gap(design::space::SM)
        .header(
            EmptyHeader::new()
                .gap(design::space::SM)
                .max_w(design::size::EMPTY_MEASURE)
                .media(
                    // A turning glyph gets the unframed slot: the frame is sized
                    // for a static icon and would crop the sweep.
                    EmptyMedia::new()
                        .with_variant(if spinning {
                            EmptyMediaVariant::Default
                        } else {
                            EmptyMediaVariant::Icon
                        })
                        .child(glyph),
                )
                .title(
                    EmptyTitle::new()
                        // §4.13's `title 15/600`. It was `BODY` — 13px, the same size
                        // as a cell value — which made the one piece of chrome in
                        // the table area indistinguishable from the data it stood in
                        // for.
                        .text_size(design::text::TITLE)
                        .line_height(design::text::TITLE_LINE_HEIGHT)
                        .font_weight(design::text::SEMIBOLD)
                        .text_color(design::role::fg_primary(cx))
                        .child(
                            div()
                                .id("empty-title")
                                .debug_selector(|| "empty-title".to_owned())
                                .child(SharedString::from(title)),
                        ),
                )
                .description(EmptyDescription::new().child(lines)),
        );
    v_flex()
        .id("resource-empty")
        .debug_selector(|| "resource-empty".to_owned())
        .size_full()
        .items_center()
        .justify_center()
        .gap(design::space::SM)
        .role(Role::Region)
        .aria_label(label)
        .when(failed_reason.is_some(), |mut state| {
            state
                .interactivity()
                .tooltip(text_tooltip(failed_reason.clone().unwrap_or_default()));
            state.aria_description(RETRY_LOADING_RESOURCES_GUIDANCE)
        })
        .child(state_element)
        // §4.13: at most one action. The state used to add a `Switch Namespace`
        // button on top of whatever the state itself offered, so a namespaced
        // empty table had two buttons of equal weight and no reason to prefer
        // either. The namespace in the title is the scope; a reader who wants a
        // different one has the switcher in the title bar.
        .when_some(action, |state, action| {
            let (id, label, emphasis) = action.button();
            let selector: SharedString = format!("empty-action-{id}").into();
            state.child(
                // The wrapper exists for one reason: a `BaseButton` carries no
                // debug selector, and "at most one action" is a spec clause that
                // nothing else in the file can check. A wrapper is cheaper than
                // an unverifiable rule.
                div()
                    .flex_none()
                    .debug_selector(move || selector.to_string())
                    .child(
                        recovery_button(id, label, action_focus, emphasis, 0isize, cx)
                            .tooltip(text_tooltip(action.tooltip(cx)))
                            .on_click(move |_event, window, cx| action.dispatch(window, cx)),
                    ),
            )
        })
        .into_any_element()
}

/// The one row that says a list is on its way, without a skeleton under it.
///
/// `UI-SPEC` §4.14's 200ms-to-2s tier, and the row the skeleton also opens with
/// so the two states do not differ by a band height. It is the summary strip's
/// own geometry: same 32px, same padding, same 6px dot slot. A reader who sees
/// the strip replaced by a taller band has been told something changed when
/// nothing did.
fn loading_status_band(spec: &ResourceSpec, cx: &App) -> gpui_kit::Stateful<Div> {
    h_flex()
        .id("table-loading-status")
        .debug_selector(|| "table-loading-status".to_owned())
        .role(Role::Status)
        .aria_label(format!("Loading {}…", spec.label))
        .flex_none()
        .w_full()
        .h(design::size::SUMMARY_STRIP)
        .px(TABLE_CONTENT_INSET)
        .gap(design::space::SM)
        .items_center()
        .font(ui_font(cx))
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT)
        .child(
            // The shared spinner reads the reduce-motion setting, so this table
            // stops turning when the reader asked for less motion. It carries the
            // band word's own rung rather than the tier below it: the word beside
            // it says what is loading, and the sweep is the only thing that says
            // it is still happening.
            crate::panels::common::spinner(
                IconName::LoaderCircle,
                design::icon::resting(cx),
                Size::Size(design::icon::IN_ROW),
            ),
        )
        .child(
            div()
                .text_color(design::role::fg_secondary(cx))
                .child(SharedString::from(format!("Loading {}…", spec.label))),
        )
}

/// The bottom band of a load that has outlasted [`LOADING_PROGRESS`].
///
/// `§4.14`: "spinner + 进度（行数 / 字节 / 百分比）". The count is the number the
/// cluster has already reported, so it moves while the reader watches — a
/// progress line that does not move is a worse wait than no progress line,
/// because it tells a reader the app is stuck when it is counting.
fn loading_progress_band(spec: &ResourceSpec, count: usize, cx: &App) -> AnyElement {
    h_flex()
        .id("table-loading-progress")
        .debug_selector(|| "table-loading-progress".to_owned())
        .role(Role::Status)
        .aria_label(format!("{} received so far", design::format::count(count)))
        .flex_none()
        .w_full()
        .h(design::size::OPEN_VIEWS)
        .px(TABLE_CONTENT_INSET)
        .gap(design::space::SM)
        .items_center()
        .border_t_1()
        .border_color(design::role::border_subtle(cx))
        .font(ui_font(cx))
        .text_size(design::text::CAPTION)
        .line_height(design::text::CAPTION_LINE_HEIGHT)
        .child(
            div()
                .flex_1()
                .text_color(design::role::fg_tertiary(cx))
                .child(SharedString::from(format!(
                    "{} \u{2026}",
                    design::format::count_with_noun(
                        count,
                        &spec.label_lower(),
                        &format!("{} received", spec.label_lower())
                    )
                ))),
        )
        .into_any_element()
}

/// The skeleton the shared table stands in with while the first list is on its
/// way, past the point where showing nothing has stopped being honest.
///
/// gpui-kit's own skeleton draws five fixed rows of anonymous bars, so it would
/// move every column the moment the first row arrived. This one keeps the
/// reader's column widths, their hidden columns and the table's own surface, and
/// it sizes itself from the row height the shared table already resolved.
///
/// `UI-SPEC` §4.14, in full: real layout dimensions so nothing jumps, a
/// `fg.tertiary` at 5% placeholder, 3px radii following the real marks, text
/// blocks at 60–80% of their column, **no zebra and no row rules**, and
/// deterministic pseudo-randomness so two screenshots of the same state are the
/// same picture. The deterministic part was already true — the fraction is
/// `(position * 23) % 37`, not a timer — and it is the reason this function is
/// worth keeping at all; a skeleton that re-randomised itself per frame could
/// never be compared against a screenshot.
#[allow(clippy::too_many_arguments)]
fn loading_table(
    spec: &ResourceSpec,
    columns: &[ResourceColumn],
    visible: &[usize],
    widths: &[Pixels],
    row_height: Pixels,
    viewport_height: Pixels,
    waited: Duration,
    cx: &App,
) -> AnyElement {
    // Fill the viewport instead of guessing a fixed number of rows. The two bands
    // the skeleton opens with are the *chrome* the live table has above its first
    // row — the loading status band and the column header — and neither of them
    // follows the reader's density, so subtracting the row height twice (which is
    // what this did) counted a 24px header on a dense table and filled one row too
    // many, so the last row sat below the fold the live table would have ended at.
    let header_height = header_cell_height() + design::border::LINE;
    let rows = rows_in_viewport(
        f32::from(viewport_height)
            - f32::from(design::size::SUMMARY_STRIP)
            - f32::from(header_height),
        row_height,
    );
    let width_at = |position: usize, index: usize| {
        widths
            .get(position)
            .copied()
            .unwrap_or_else(|| px(column_default_width(&columns[index])))
    };
    let loading_status = loading_status_band(spec, cx);
    let header = h_flex()
        .id("table-skeleton-header-row")
        .role(Role::Row)
        .aria_row_index(1)
        .flex_none()
        .w_full()
        // The live header band's own height, not the row height: §4.4 fixes the
        // header at 32 whatever the density is, because a header is chrome and the
        // density is a property of the rows being scanned. Sizing the skeleton's
        // header off the row put a dense table's columns 8px lower in the skeleton
        // than in the table that replaced it, which is the one jump the skeleton's
        // whole reason for existing is to avoid.
        .h(header_height)
        .overflow_hidden()
        // §4.14: "保持真实布局尺寸（不留跳）". Two things make a placeholder move
        // when the rows replace it, and both are here. The live cell is wrapped in
        // the shared table's own box, which pads it by 16 a side, and the
        // placeholder drew its bar against the column's edge instead — so every
        // value stepped 16px to the right on arrival. And a bare `h_flex` row
        // stretches its children, which pins a fixed-height bar to the *top* of a
        // 32px row, where the live line is centered: the bar then dropped 11px
        // into place. Padding and `items_center` are what make the two states one
        // layout.
        .items_center()
        // The live band's own surface, for the same reason it has one: a skeleton
        // header at the rows' value is a header that reads as a fifth row, and it
        // is on screen for every long load.
        .bg(table_header_surface(cx))
        // The one rule a skeleton keeps: the live header's own 1px
        // `border.subtle` baseline, so the columns do not move up a pixel when
        // the rows replace the placeholders.
        .border_b_1()
        .border_color(design::role::border_subtle(cx))
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
                .h_full()
                .items_center()
                // The live header's label sits inside the same inset as every body
                // cell, so the word does not move when the placeholders go.
                .px(TABLE_CONTENT_INSET)
                .overflow_hidden()
                // The live header puts a number column's label against the right
                // edge of the same 16 a side, so the skeleton does too. Left
                // against the cell it is not a shorter word in the same place: it
                // is the same word in a different one, and `READY` walked the whole
                // width of its column when the rows arrived.
                .when(column.is_right_aligned(), |header| header.justify_end())
                .when(position == 0, |header| {
                    header
                        .border_r_1()
                        .border_color(design::role::border_subtle(cx))
                })
                .font(ui_font(cx))
                .text_size(design::text::CAPTION)
                .line_height(design::text::CAPTION_LINE_HEIGHT)
                // `MEDIUM`, which is what an unsorted live label is set at. The
                // skeleton has no sort state to read, so it draws the resting
                // weight — a skeleton label at the sorted column's weight would
                // name a sort that is not there yet, and then drop a step when the
                // real header arrives.
                .font_weight(design::text::MEDIUM)
                .text_color(design::role::fg_secondary(cx))
                .child(header_label(column.title))
        }));
    // The count of rows stays readable, so the iterator does not shadow it.
    //
    // No zebra and no rule under a row: the live table has neither, and a
    // placeholder that does not match the thing it stands in for is a second
    // layout to re-learn. The earlier version striped the *opposite* set of rows
    // from the body, so the table visibly re-patterned at the moment the data
    // arrived.
    //
    // `§4.14` asks for a 1.6s breathe between `.05` and `.09`. It is derived
    // from the wait rather than from a per-row animation clock: gpui 0.6.6 has
    // no `track_*` equivalent for a purely visual loop, and giving a virtualized
    // table 20 rows their own animation to move four percent of alpha is a
    // per-frame cost for a motion most readers will not notice and some will
    // find distracting. The placeholder therefore breathes on the same clock the
    // wait runs on, which is one value per frame instead of one per row.
    //
    // Under reduced motion the breathe does not run at all: an opacity pulse
    // is motion, and the convention overview.rs documents for its own loading
    // ladder is that the *spinner* owns the preference while the skeleton
    // stays visible — a placeholder that froze entirely would read as a
    // stalled render rather than as content that has not arrived. The static
    // answer is a frozen frame of the pulse, taken at mid-swing (a quarter of
    // the triangle's own cycle) through the same resolver rather than as a
    // picked number: the floor and the peak are the band's private constants,
    // and a literal here would be a third owner of them.
    let breathe = if cx.reduce_motion() {
        design::skeleton_alpha(SKELETON_BREATHE / 4, SKELETON_BREATHE)
    } else {
        design::skeleton_alpha(waited, SKELETON_BREATHE)
    };
    let placeholder = design::role::fg_tertiary(cx).opacity(breathe);
    let skeleton_rows = (0..rows).map(|row_index| {
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
            // The same cross-axis centring the header row above takes: a bar of a
            // fixed height in a stretched row sits against the row's top edge,
            // and the live line it stands in for is centered.
            .items_center()
            .bg(table_row_surface(cx))
            .children(visible.iter().enumerate().map(|(position, &index)| {
                let column = &columns[index];
                let width = width_at(position, index);
                h_flex()
                    .w(width)
                    .flex_none()
                    .h_full()
                    .items_center()
                    // The live cell's own padding, so a value does not step 16px
                    // to the right when the placeholders are replaced.
                    .px(TABLE_CONTENT_INSET)
                    .overflow_hidden()
                    // And the live cell's own alignment. `UI-SPEC` §10.1 puts a
                    // number column's value against the right edge of its cell, so
                    // a placeholder against the left one is not a shorter version
                    // of the same thing — it is the same thing somewhere else, and
                    // all three number columns jumped the width of their own
                    // column the moment the rows arrived.
                    .when(column.is_right_aligned(), |cell| cell.justify_end())
                    .when(position == 0, |cell| {
                        cell.border_r_1()
                            .border_color(design::role::border_subtle(cx))
                    })
                    .child(if column.class == ColumnClass::Status {
                        // The status cell's real shape is a 6px dot *and* the word
                        // beside it, so its placeholder is a 6px dot and a bar. The
                        // dot alone was the right instinct — a bar where the column
                        // has a dot is a placeholder for a shape the column does
                        // not contain — and it was applied a column too far: the
                        // real cell is `dot + gap + word`, so a lone dot drew a
                        // status column two thirds shorter than every other column
                        // and the row of placeholders had a hole in it.
                        h_flex()
                            .flex_none()
                            .gap(STATUS_DOT_GAP)
                            .items_center()
                            .child(
                                div()
                                    .size(design::size::STATUS_DOT)
                                    .flex_none()
                                    .rounded_full()
                                    .bg(placeholder),
                            )
                            .child(
                                div()
                                    .h(SKELETON_BAR_HEIGHT)
                                    .w(skeleton_bar_width(
                                        px(f32::from(width) - 2.0 * f32::from(design::space::LG)),
                                        0.6,
                                    ))
                                    .flex_none()
                                    .rounded(design::radius::XS)
                                    .bg(placeholder),
                            )
                    } else {
                        // 42% to 78% of the *text area*, deterministically. §4.14
                        // asks for 60–80% of the column; measured against the cell
                        // instead, the cell's own 16 a side is charged twice — once
                        // by the padding and once by the clamp — which put the
                        // placeholder flush against the padding and 16px short of
                        // the value it stands in for. The long-text class is
                        // narrower on top of that, because it draws at 12 not 13.
                        let fraction = 0.6
                            + ((position * 23) % 17) as f32 / 100.0
                                * if column.is_mono() { 0.7 } else { 1.0 };
                        div()
                            .h(SKELETON_BAR_HEIGHT)
                            // The bar must stay inside its own column.
                            .w(skeleton_bar_width(width, fraction))
                            .flex_none()
                            .rounded(design::radius::XS)
                            .bg(placeholder)
                    })
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
/// The shared table wraps every cell in a box of its own: the row's height less
/// the padding it pads a cell by. A cell that takes the row's height instead
/// starts under that padding, so it hangs lower than the row it belongs to, and
/// it is the padding that puts it there. Taking the wrapper's box is the only
/// height that is on the row's center without the app having to know how much
/// the shared table padded by.
trait CellOnRowRhythm {
    fn cell_on_row_rhythm(self) -> Self;
}

impl<T: gpui_kit::Styled> CellOnRowRhythm for T {
    fn cell_on_row_rhythm(self) -> Self {
        self.h_full()
    }
}

/// The header label as §4.4 wants to see it.
///
/// `UI-SPEC` §4.4 asks for `uppercase` and gpui 0.6.6 has no text-transform, so
/// the label is uppercased here rather than in `columns.rs`: the title is a label
/// for people and is also what `aria_label`, the column menu and the tooltip read,
/// and shouting at all four of them at once would be the wrong fix. The
/// `+0.06em` tracking the spec pairs with the uppercase has no API either and is
/// reported rather than faked.
fn header_label(title: &'static str) -> SharedString {
    SharedString::from(title.to_uppercase())
}

/// Truncates a name in the middle rather than the tail.
///
/// `UI-SPEC` §10.1: "对象名截断用中间省略, k8s 的 hash 在尾部". A Kubernetes name
/// is `<prefix>-<suffix>-<hash>`, and the hash is the part that identifies one
/// pod from another — tail ellipsis deletes exactly the half of the string the
/// reader is looking for, and leaves them with `coredns-ae4f2c-` fifty times over.
/// The head carries what the workload is and the tail carries which one.
///
/// gpui has `text_ellipsis` and no middle equivalent, so this is done on the
/// string — and on the *string*, not on the element, which is the whole point:
/// a middle ellipsis has to be computed, so it can be computed from the width
/// the name actually has. The fixed 16-and-12 rule it replaced shortened every
/// name over 29 characters no matter how wide the reader had made the column, so
/// dragging `Name` from 252 to 400 revealed 148px of nothing and the column
/// behaved as if it were fixed. A name that fits is now drawn whole, and one
/// that does not is shortened to the room there is, two thirds of it from the
/// head because that is the half naming the workload.
///
/// `available` is the text area inside the cell and `char_width` the face's
/// average advance, so the decision is in the same units as
/// [`cell_text_overflows`], which is what decides the tooltip. The *split* is
/// measured rather than estimated, because an average is an average: a name cut
/// to `available / average` is drawn at whatever its own glyphs happen to add up
/// to, and Inter's tabular digits and hyphens are not what the average is made
/// of. `kube-apiserver-k8s-gpui-3n-c…-pla` overflowed the sticky column by one
/// glyph and lost it against the divider, so the head gives characters back one
/// at a time until the shaped string is inside the cell.
///
/// The tail keeps at least [`MIDDLE_ELLIPSIS_MIN_TAIL`] characters, because the
/// tail is the half the ellipsis exists to keep and a column narrow enough to
/// squeeze it to nothing has stopped being a column.
fn middle_ellipsis(
    window: &mut Window,
    text: &str,
    available: f32,
    typography: &DataTypography,
    char_width: f32,
) -> Option<String> {
    if !char_width.is_finite() || char_width <= 0.0 || available <= 0.0 {
        return None;
    }
    let count = text.chars().count();
    if count as f32 * char_width <= available {
        return None;
    }
    // The one character the ellipsis itself spends.
    let budget = (available / char_width).floor() as usize;
    let budget = budget.saturating_sub(1);
    if budget <= MIDDLE_ELLIPSIS_MIN_TAIL {
        return None;
    }
    let tail: String = text
        .chars()
        .skip(count.saturating_sub(MIDDLE_ELLIPSIS_MIN_TAIL))
        .collect();
    // The allowance the shaped width is not allowed to spend on its own.
    //
    // `shaped_line_width` sums HarfBuzz's float advances; gpui lays the same run
    // out on snapped advances, and the snapping rounds *up*, so the painted line is
    // always at least as wide as the number this function just measured. The
    // difference is invisible on a string with room to spare and fatal on the one
    // this loop is built to produce — the loop deliberately picks the longest head
    // that fits, so every candidate it returns lands within a pixel or two of the
    // budget, which is inside the error. Measured on the shipping face at 12px:
    // `nightly-reindex-warehouse-…-dpmz9` shapes to 213.7 in a 219px box and paints
    // 5.8px wider, so its last `9` lost the right half of its bowl to the cell's
    // `overflow_hidden`.
    //
    // A quarter of a pixel a glyph is the ceiling of that rounding at these sizes,
    // and it is charged per character rather than as a flat margin because the error
    // accumulates: a flat 2px would pass this string and fail the next one along.
    // It costs about one character of head on a 34-character name, which is the
    // cheapest way to be wrong here — the alternative is a glyph cut in half.
    let allowance = count as f32 * SHAPED_WIDTH_SLACK_PER_GLYPH;
    for head in (1..=(budget - MIDDLE_ELLIPSIS_MIN_TAIL)).rev() {
        let head: String = text.chars().take(head).collect();
        let candidate = format!("{head}\u{2026}{tail}");
        if shaped_line_width(window, &candidate, typography) + allowance <= available {
            return Some(candidate);
        }
    }
    // Too narrow for a head, a tail and an ellipsis: the cell's own tail
    // ellipsis takes it, which at least agrees with every other column about
    // what a clipped value looks like.
    None
}

/// What a cell with no value says.
///
/// An em dash, because a blank cell in a table of facts is ambiguous: it reads as
/// "nothing here" and as "not loaded yet" at the same time. The numeric columns
/// already answered it this way, and `docs/mockup` draws `\u{2014}` in the `Node`
/// cell of a Pending pod — so this is the mockup's mark, and the projector is the
/// only thing that was not drawing it.
const EMPTY_CELL_MARK: &str = "\u{2014}";

/// The fewest characters of the tail a shortened name keeps.
///
/// Six is the shortest Kubernetes hash suffix that still tells two pods of one
/// ReplicaSet apart, and a name narrower than head-plus-six is not shortened in
/// the middle at all — it is the shared cell's own tail ellipsis, which at least
/// agrees with every other column about what a clipped value looks like.
const MIDDLE_ELLIPSIS_MIN_TAIL: usize = 6;

/// What [`shaped_line_width`] under-reports, per character, in logical pixels.
///
/// See the use in [`middle_ellipsis`]: gpui lays a shaped run out on snapped
/// advances, so the painted line is never narrower than the float sum that measured
/// it. The two agree to a fraction of a pixel on any line with room, and disagree by
/// enough to cut a glyph in half on the one line a fitting loop deliberately fills to
/// the edge.
const SHAPED_WIDTH_SLACK_PER_GLYPH: f32 = 0.25;

/// The width of the lane a row reserves at its trailing edge, or nothing.
///
/// The lane exists to give the table a trailing spine, and a spine is only worth
/// reserving where there is room to reserve it. Two conditions, both about width:
///
/// * the cell has to be the last one on screen, because a lane in the middle of a
///   row would be a hole in the middle of the data, and
/// * the column has to have slack — its resolved width less its own floor, less
///   the lane, less the gap and less the cell's own 32px of padding, all of which
///   the value still needs. A `Deployment`'s trailing `Age` is 89px at its floor
///   and never grows, so there is no lane there and nothing changes for it; a
///   `Pod`'s `Node` is the one column that takes the room the others leave, so it
///   is the one column with somewhere to put a mark.
///
/// Pure, so the condition is a decision a test can ask about rather than a
/// judgement made once per frame.
fn trailing_lane(column: Option<&ResourceColumn>, width: Pixels, is_last: bool) -> Pixels {
    if !is_last {
        return px(0.0);
    }
    let Some(column) = column else {
        return px(0.0);
    };
    let slack = f32::from(width)
        - column.min_width()
        - f32::from(TRAILING_LANE)
        - f32::from(TRAILING_LANE_GAP)
        - 2.0 * f32::from(design::space::LG);
    if slack > 0.0 { TRAILING_LANE } else { px(0.0) }
}

/// One cell of the table: its value, the dot that leads the status column, the
/// pending badge when the operation has no other home, and the full-value
/// tooltip when the column clips what it holds.
///
/// `row_background` is the surface the row is painted on rather than the table's
/// own, so the status inks can be solved against the wash the row actually took.
/// It is the same value [`ResourceTableDelegate::row_state`] handed the row
/// element, which is what keeps a cell from being painted against another state.
#[allow(clippy::too_many_arguments)]
fn row_cell(
    row_index: usize,
    position: usize,
    row: &Row,
    columns: &[ResourceColumn],
    visible: &[usize],
    widths: &[Pixels],
    typography: &DataTypography,
    char_width: f32,
    // `CellInk::color` resolves the level against the row it is drawn on
    // already, so a selected row does not restate the wash here.
    row_background: Hsla,
    pending: Option<&PendingState>,
    has_status_column: bool,
    trailing: Option<design::Confidence>,
    window: &mut Window,
    cx: &App,
) -> Option<AnyElement> {
    let index = *visible.get(position)?;
    let cell = row.cells.get(index)?;
    let column = columns.get(index);
    let is_status = column.is_some_and(|column| column.class == ColumnClass::Status);
    let cell_key = ((row_index as u64) << 16) | index as u64;
    let cell_id = ElementId::NamedInteger("resource-cell".into(), cell_key);
    let cell_container_id = ElementId::NamedInteger("resource-cell-container".into(), cell_key);
    // Every cell is measured in the face it is drawn in. The long-text class is
    // the one that is not the body face, and a value shaped in the wrong face
    // either overflows a cell that fits or clips one that does not.
    let (mono_face, mono_width) = (
        column
            .is_some_and(ResourceColumn::is_mono)
            .then(|| mono_typography(cx)),
        mono_char_width(),
    );
    let (cell_face, cell_char_width) = match &mono_face {
        Some(face) => (face.clone(), mono_width),
        None => (table_face(cx, typography), char_width),
    };
    // The width the value has to live in, resolved before anything is drawn
    // rather than after: whether a name is shortened at all is a function of it
    // (see [`middle_ellipsis`]), so a cell that has to decide can no longer ask
    // at the bottom of the function.
    let width = widths
        .get(position)
        .copied()
        .or_else(|| column.map(|column| px(column_default_width(column))))
        .unwrap_or(px(0.0));
    // The lane the trailing mark lives in, if this cell is the one that carries it.
    //
    // The last column is the one that takes the room the others leave, so it is
    // the one with slack — and slack with nothing in it is the 40% of a wide table
    // that reads as an unfinished row. Reserving the lane turns that slack into
    // the trailing spine: every row's last value now ends on the same x, one lane
    // in from the panel edge, and the mark lands in the lane instead of pushing
    // the value sideways when it appears.
    let lane = trailing_lane(column, width, position + 1 == visible.len());
    // What the value itself has to live in. The lane and the gap in front of it
    // are not text area, so both come off the width every width decision below is
    // made against — the middle ellipsis, the overflow test and the tooltip all
    // read this one number, and a cell that reported an overflow for a value that
    // fits in the room it actually has would put a tooltip on every row.
    let reserved = if lane > px(0.0) {
        lane + TRAILING_LANE_GAP
    } else {
        px(0.0)
    };
    let value_width = (width - reserved).max(px(0.0));
    // The cell's own padding is the table's; the first cell's extra leading pad
    // is gone because the selection rail moved to the row's leading edge, where
    // `UI-SPEC` §4.4 puts it. A rail inset into a gutter needed a matching pad on
    // every first cell in both the header and the body, and a rail flush to the
    // edge needs neither.
    let element: AnyElement = if is_status {
        let severity = status_severity(cell.text.trim(), row_age(row));
        // Solved against the row the cell is on, not against the table: see
        // `status_ink_on_row`. On a plain row this is a no-op, because the channel
        // was already solved against this very surface.
        let dot_ink = status_dot_ink(severity, row_background, cx);
        let word_ink = status_word_ink(severity, row_background, cx);
        let cell_row = h_flex()
            .id(cell_container_id)
            .debug_selector(move || format!("resource-status-cell-{row_index}-{position}"))
            .role(Role::Cell)
            .aria_column_index(position + 1)
            .font(ui_font(cx))
            .text_size(typography.size)
            .line_height(typography.line_height)
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            // The one column that carries a long value with no room:
            // without an ellipsis it was cut mid-glyph.
            .text_ellipsis()
            .gap(STATUS_DOT_GAP)
            .items_center()
            .cell_on_row_rhythm();
        let cell_row = match pending {
            Some(pending) => cell_row.child(pending_badge(pending, row_index, cx)),
            None => cell_row,
        };
        let cell_row = if cell.text.is_empty() {
            cell_row
        } else {
            // A 6px dot, not a 16px glyph, and no icon at all. §4.4: the verdict
            // is a colour on a dot, and a glyph per severity puts four shapes in
            // every row of a 10,000-row table for a distinction the colour
            // already carries. `Unknown` is the one case a shape earns, because
            // "no verdict" is not a health and a grey dot would read as a
            // healthy one.
            cell_row.child(
                div()
                    .id(ElementId::NamedInteger(
                        "resource-row-health".into(),
                        cell_key,
                    ))
                    .debug_selector(move || format!("resource-row-health-{row_index}"))
                    .role(Role::Image)
                    .aria_label(status_accessible_label(cell.text.trim(), row_age(row)))
                    .size(design::size::STATUS_DOT)
                    .flex_none()
                    .rounded_full()
                    .bg(dot_ink),
            )
        };
        cell_row
            .child(
                // The word carries the verdict's own colour, which is the half of
                // §0 铁律三 the dot cannot: at 13px a reader reads the word and only
                // glances at the dot, and an uncoloured word beside a coloured dot
                // is a verdict the reader has to reconstruct. It used to have no
                // colour token at all, so `Running` and `CrashLoopBackOff` were the
                // same ink and the only difference in the cell was a 16px glyph.
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(word_ink)
                    .font_weight(design::text::MEDIUM)
                    .child(gpui_kit::Text::new(
                        cell_id,
                        SharedString::from(cell.text.clone()),
                    )),
            )
            .into_any_element()
    } else {
        // The cell takes the shared table's cell box, which is the row's own
        // height, and centers the configured line in it.
        //
        // It has to be a **flex row** to do that. `justify_center` is
        // `justify-content`, and gpui lays out through taffy, where
        // `justify-content` applies to a flex or grid container and to nothing
        // else — on a block it is silently dropped. The cell used to be a block,
        // so its single line sat at the top of the box with the whole slack
        // underneath: 4.5px above the cap and 18px below it in a 32px row, and
        // a baseline **7px higher than every other cell on the same row**. The
        // status cell beside it was already an `h_flex`, so `Running` was
        // centered while the name, the namespace, the numbers and the age all
        // climbed to the top of the row, and the reader saw one row with two
        // baselines in it. `items_center` is the cross axis of a row, which is
        // the vertical one here, and it is what the status cell has always used.
        //
        // The horizontal axis is a decision, not a side effect: `justify_start`
        // for a text column and `justify_end` for a number column, so the value's
        // own edge is the column's data edge in both. `text_right` would have
        // done the same on a block, and no longer reaches — the text is a flex
        // item now, and `text-align` does not move a flex item.
        //
        // This holds because the cell is `whitespace_nowrap`: a second line would
        // be a second line to center, not a taller cell.
        let mut element = h_flex()
            .id(cell_container_id)
            .debug_selector(move || format!("resource-data-cell-{row_index}-{position}"))
            .role(Role::Cell)
            .aria_column_index(position + 1)
            .font(cell_face.font.clone())
            .font_features(cell_face.features.clone())
            .text_size(cell_face.size)
            .line_height(cell_face.line_height)
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .items_center()
            .cell_on_row_rhythm();
        if column.is_some_and(ResourceColumn::is_right_aligned) {
            element = element.justify_end();
        }
        // The identifier is the one cell a reader scans down, so it is the one
        // cell at medium weight. Everything else is data beside it and reads at
        // regular: a column of 500-weight names is a column of shouting names.
        //
        // Its *size* is `text::SUBTITLE` rather than the body's own, stated
        // explicitly so the hierarchy is a declaration instead of an accident of
        // the two tokens currently being the same 13px. It is still
        // `max(…, typography.size)`, because a reader who raises the data font has
        // to get the name at the size they asked for too.
        if column.is_some_and(|column| column.class == ColumnClass::Identifier) {
            element = element
                .font_weight(design::text::MEDIUM)
                .text_size(cell_face.size.max(design::text::SUBTITLE))
                .line_height(
                    cell_face
                        .line_height
                        .max(design::text::SUBTITLE_LINE_HEIGHT),
                );
        }
        // The ink ladder, resolved by the column's own declaration so no kind can
        // decide a cell's colour for itself. A row that carries a wash is not
        // re-inked here: `CellInk::color` already solves every level against
        // the surface the cell is actually drawn on, so a selected row keeps the
        // same three steps a resting row has.
        //
        // One value overrides the column, and it is the one that is not a value:
        // a dash stands for "the cluster never reported this", so it wears the
        // placeholder rung whatever the column asked for. A `READY` column whose
        // dashes sit at body weight is a column where the dashes compete with
        // the numbers the reader is scanning for.
        let ink = if super::columns::is_absent(&cell.text) {
            Some(design::role::fg_tertiary(cx))
        } else {
            column.map(|column| column.ink.color(cx))
        };
        if let Some(ink) = ink {
            element = element.text_color(ink);
        }
        // Put the pending badge in the first cell when Status is hidden.
        if let Some(pending) = pending
            && position == 0
            && !has_status_column
        {
            h_flex()
                .font(ui_font(cx))
                .text_size(typography.size)
                .line_height(typography.line_height)
                .whitespace_nowrap()
                .gap(design::space::XS)
                .items_center()
                .w_full()
                .min_w(px(0.0))
                .cell_on_row_rhythm()
                .child(pending_badge(pending, row_index, cx))
                .child(element)
                .into_any_element()
        } else {
            // A cell with nothing in it says so. A blank cell in a data table is
            // two things at once — "this resource has no value here" and "this
            // has not loaded yet" — and the reader cannot tell them apart, so the
            // columns that already answer it with a dash were answering for
            // themselves and `Node` was not. The projector leaves the text empty;
            // the placeholder is the table's.
            //
            // It is `fg.tertiary`, not `fg.disabled`. `fg.disabled` is §1.4's ink
            // for text a control cannot act on, and this is data: at 2:1 on the
            // content surface a disabled-ink dash is a smudge in light and a hole
            // in dark, which is the opposite of saying something. Tertiary clears
            // 3:1 in both themes, and the glyph — not the colour — is what tells
            // the reader this is an absence rather than a value.
            let (text, shortened_in_the_middle) = if cell.text.is_empty() {
                (EMPTY_CELL_MARK.to_owned(), false)
            } else {
                let shortened = column
                    .filter(|column| column.class == ColumnClass::Identifier)
                    .and_then(|_| {
                        middle_ellipsis(
                            window,
                            cell.text.trim(),
                            cell_text_width(column, value_width),
                            &cell_face,
                            cell_char_width,
                        )
                    });
                match shortened {
                    Some(shortened) => (shortened, true),
                    None => (cell.text.to_string(), false),
                }
            };
            let empty = cell.text.is_empty();
            // Whether this value already carries the ellipsis that says it was cut.
            //
            // A name shortened in the middle has one, and the cell's own
            // `text_ellipsis` must not add a second: the drawn string is measured to
            // fit, but the box it lands in is a *fractional* layout width while
            // `available` is computed from the nominal column width, so the two can
            // disagree by a fraction of a pixel. On the shipping face that was enough
            // to lose the tail and paint `nightly-reindex-warehouse-…-dpmz9` as
            // `nightly-reindex-warehouse-…-dp…` — two ellipses on the one column a
            // reader scans down, and the only cell in the table whose clipped state
            // is spelled. Whoever shortened the value is the one who drew the mark,
            // so the box is left undecorated and a value this code could not shorten
            // (`middle_ellipsis` returned `None`) still falls through to the shared
            // tail ellipsis below.
            element
                .when(empty, |element| {
                    element.text_color(design::role::fg_tertiary(cx))
                })
                // The value sits in a box of its own inside the cell, which is what
                // makes the two things this cell has to do at once possible: the box
                // is the flex item `items_center` centers on the row's cross axis,
                // and it is the box that carries `text_ellipsis`, because
                // `text-overflow` is read by a *text* element and this one is a
                // flex item whose width is the shrink result rather than the
                // container's. The status cell's word has always been built this
                // way; this is the same shape, so a row is one rhythm end to end.
                //
                // The name is the one value whose tail is the point, so it is the
                // one value shortened in the middle. Every other value in the
                // table is tail-truncated by this box's own `text_ellipsis` — and
                // so is a name this column was too narrow to shorten, which is the
                // case the mark is still owed. A name it *did* shorten brings its
                // own, and takes the box's off with it; see
                // `shortened_in_the_middle`.
                .child(
                    div()
                        .debug_selector(move || {
                            format!("resource-cell-line-{row_index}-{position}")
                        })
                        .min_w_0()
                        .overflow_hidden()
                        .when(!shortened_in_the_middle, |line| line.text_ellipsis())
                        .child(gpui_kit::Text::new(cell_id, SharedString::from(text))),
                )
                .into_any_element()
        }
    };
    // The confidence marker rides the trailing edge of the last cell, opposite
    // the status dot at the leading edge of the status cell. It shares that cell
    // rather than owning a column of its own, so the table stays rectangular and
    // the row keeps its full width for the columns a reader came for.
    //
    // It is drawn *in the lane* rather than beside the value, and the lane is
    // reserved whether or not there is a marker to put in it. Both halves of that
    // are the same fix. Appending the mark to a `justify_end` row meant the last
    // column's value sat against the panel edge on every ordinary row and was
    // pushed one icon further in on the rows that had something to report — so a
    // stale cluster moved the last column's values, and a fresh one left them
    // somewhere else. A lane is reserved, so the value is on one x in every row
    // and the mark is a thing that arrives in a slot rather than a thing that
    // shoves its neighbour along.
    let element = if lane > px(0.0) {
        h_flex()
            .w_full()
            .min_w(px(0.0))
            .h_full()
            .items_center()
            .gap(TRAILING_LANE_GAP)
            .child(element)
            .child(
                div()
                    .debug_selector(move || format!("resource-row-lane-{row_index}"))
                    .flex_none()
                    .w(TRAILING_LANE)
                    .h_full()
                    .items_center()
                    .child(
                        trailing
                            .filter(|state| !state.is_definite())
                            .and_then(|state| confidence_marker(row_index, state, cx))
                            .unwrap_or_else(|| div().into_any_element()),
                    ),
            )
            .into_any_element()
    } else {
        element
    };
    // Each cell is measured in the face it renders in, so a value long
    // enough to clip gets a tooltip that carries the whole value. The shaped
    // width answers whether the value is clipped, and it also sizes the tooltip.
    // The width it is measured against is the *value's* width: the lane is not
    // text area, and a cell that reported an overflow for a value that fits in
    // the space it actually has would put a tooltip on every row of the table.
    let measured = measure_cell_text(window, &cell.text, &cell_face, value_width, cell_char_width);

    if !cell_text_overflows(column, &cell.text, value_width, cell_char_width, measured) {
        return Some(element);
    }
    let mut element = div()
        .w_full()
        .min_w(px(0.))
        .id(ElementId::NamedInteger("pod-cell".into(), cell_key))
        .child(element);
    element
        .interactivity()
        .tooltip(cell_tooltip(&cell.text, measured, &cell_face));
    Some(element.into_any_element())
}

/// The monospace face the long-text class renders in, with the same `tnum`
/// features every other cell carries so a port column's digits still line up.
fn mono_typography(cx: &App) -> DataTypography {
    let configured = DataTypography::from_theme_settings(cx);
    DataTypography {
        features: configured.features,
        font: configured.font,
        size: design::text::MONO_SM,
        line_height: design::text::MONO_SM_LINE_HEIGHT,
    }
}

/// The face every table cell but a machine-shaped one draws in.
///
/// `PROMPT` §2 rule #2 and `UI-SPEC` §10.1 D25: the table is sans, all of it.
/// Monospace is for YAML, logs, UIDs, IPs and image tags — a pod name is data,
/// not code, and a mono name at 12px is heavier than the sans beside it, so the
/// row stops reading as one family. The whole table used to take the *data* face,
/// which is the buffer face, so `pod-0` in monospace sat next to `Running` in
/// Inter. It also meant the advance estimate was measuring a face nobody drew
/// with: `CHAR_WIDTH_RATIO` is 0.52, a sans ratio, and the face it sized was a
/// monospace one.
///
/// Only the *family* is decided here. The size, the line height and the features
/// still come from the reader's data-font setting, because a setting that
/// enlarges the text has to enlarge it everywhere the text is read.
fn table_face(cx: &App, typography: &DataTypography) -> DataTypography {
    // The slashed zero is a code convention, and it is the right one in the buffer:
    // in a log line or a UID, `0` and `O` are a real ambiguity. In a table of pods
    // it is neither needed nor wanted — `UI-SPEC` §2.3 asks number columns for
    // `tnum` and nothing else, and a slashed `0` in every `0/1` and every restart
    // count puts a diagonal stroke through four columns of a 10,000-row table.
    // The reader's own `buffer_font_features` still wins: this drops the *default*
    // and leaves a setting that asks for the slash.
    let features = FontFeatures(Arc::from(
        typography
            .features
            .0
            .iter()
            .filter(|(tag, _)| tag.as_str() != "zero")
            .cloned()
            .collect::<Vec<_>>(),
    ));
    DataTypography {
        features,
        font: ui_font(cx),
        size: typography.size,
        line_height: typography.line_height,
    }
}

/// The advance estimate for the monospace face, which is the one face where the
/// advance really is a constant fraction of the size.
fn mono_char_width() -> f32 {
    f32::from(design::text::MONO_SM) * crate::settings::MONO_ADVANCE_EM
}

/// Everything the shared table draws, and every app decision behind it.
///
/// The delegate holds one frame's worth of the view's state rather than a
/// reference back into it, so a cell is a pure function of the snapshot it was
/// handed and a repaint cannot read a half-updated view. The weak handle is
/// only for the events: a click, a menu row.
struct ResourceTableDelegate {
    /// The view that owns the state machine behind this table.
    view: WeakEntity<PodsView>,
    /// Every column, in the order the resource declares them.
    columns: Arc<Vec<ResourceColumn>>,
    /// The visible column indexes, in display order.
    visible: Vec<usize>,
    /// One width per visible column, in display order.
    widths: Vec<Pixels>,
    /// The snapshot the rows come from, or `None` before the first list.
    snapshot: Option<Arc<IndexSnapshot>>,
    /// Snapshot indexes of the listed rows, in display order.
    shown: Arc<Vec<usize>>,
    /// Every selected row UID, and the one a single-row command acts on.
    selected: Arc<HashSet<SharedString>>,
    anchor: Option<SharedString>,
    /// Whether the app's table tab stop holds the focus.
    table_focused: bool,
    /// Whether that focus came from a key. A click focuses the table too, and a
    /// rail that appears for both is a rail that means nothing.
    keyboard_focus: bool,
    /// What the host is doing, which picks the empty state and the skeleton.
    status: TableStatus,
    sort: Option<Sort>,
    relevance: bool,
    /// Freshness belongs to the table, so every row it still draws inherits it.
    stale: bool,
    /// Whether the status column is on screen, which is where a pending
    /// operation's badge goes when it is.
    has_status_column: bool,
    /// The data typography, measured once per frame.
    typography: DataTypography,
    /// The operations the server has not confirmed, by row UID.
    pending: HashMap<SharedString, PendingState>,
    /// Which rung of `UI-SPEC` §4.14's ladder the reader is in.
    tier: LoadingTier,
    /// How long the reader has actually been waiting, for the skeleton's breath.
    waited: Duration,
    /// The row count the cluster has reported so far, for the progress line.
    progress: usize,
}

impl ResourceTableDelegate {
    fn new(view: WeakEntity<PodsView>, widths: Vec<f32>, typography: DataTypography) -> Self {
        Self {
            view,
            columns: Arc::new(Vec::new()),
            visible: Vec::new(),
            widths: widths.into_iter().map(px).collect(),
            snapshot: None,
            shown: Arc::new(Vec::new()),
            selected: Arc::new(HashSet::new()),
            anchor: None,
            table_focused: false,
            keyboard_focus: false,
            status: TableStatus::Idle,
            sort: None,
            relevance: false,
            stale: false,
            has_status_column: false,
            typography,
            pending: HashMap::new(),
            tier: LoadingTier::Nothing,
            waited: Duration::ZERO,
            progress: 0,
        }
    }

    /// The row one listed position shows, and the snapshot index behind it.
    fn row(&self, position: usize) -> Option<(&Row, usize)> {
        let index = *self.shown.get(position)?;
        let row = self.snapshot.as_ref()?.rows.get(index)?;
        Some((row, index))
    }

    /// The whole state one row is drawn with, shared by the row element and
    /// every cell in it so a cell cannot be painted against another state.
    fn row_state(
        &self,
        position: usize,
        cx: &App,
    ) -> Option<(RowVisual, Hsla, Option<PendingState>)> {
        let (row, _index) = self.row(position)?;
        let uid = row.obj.metadata.uid.as_deref();
        let selected = self.selected.contains(uid.unwrap_or_default());
        let visual = row_visual(
            selected,
            selected && self.anchor.as_deref() == uid,
            self.table_focused,
            self.keyboard_focus,
            !self.selected.is_empty(),
            position,
        );
        let background = row_visual_background(
            visual,
            row_selection_bg(cx),
            design::row_focus_bg(cx),
            table_row_surface(cx),
        );
        Some((
            visual,
            background,
            uid.and_then(|uid| self.pending.get(uid).cloned()),
        ))
    }

    /// The header cell of one visible column. It is the view's own header, so
    /// the delegate only hands over the sort state the view does not own, and the
    /// two facts its focus treatment is a function of: whether the app's tab stop
    /// holds the focus, and whether a key is what put it there.
    fn header_cell(&self, position: usize, cx: &mut App) -> AnyElement {
        let Some(view) = self.view.upgrade() else {
            return div().into_any_element();
        };
        let keyboard = self.keyboard_focus && self.table_focused;
        view.update(cx, |view, cx| {
            view.header_cell(position, self.sort, self.relevance, keyboard, cx)
        })
    }
}

impl TableDelegate for ResourceTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.visible.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.shown.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&index) = self.visible.get(col_ix) else {
            return Column::default();
        };
        let Some(column) = self.columns.get(index) else {
            return Column::default();
        };
        // Every column stays draggable. The last one used to opt out, which left
        // the one column that actually clips mid-glyph with no pointer way out
        // of it.
        //
        // Every column is still sortable, but the control is the app's own. The
        // component draws a `ChevronsUpDown` on *every* column it is given a sort
        // state for, so a seven-column Pod table put seven arrows and three
        // funnels in a 32px band and the band stopped being a header. The state
        // is left `None` so the component draws nothing, and
        // [`PodsView::column_sort_control`] draws a direction on the sorted column
        // and reveals it under the pointer on the rest.
        //
        // Three things come from the column's declared class rather than from
        // its title, and that is the whole point of the class: `§10.1` decides
        // alignment, stickiness and truncation per class, so adding a kind cannot
        // produce a column that is right-aligned in one table and left-aligned in
        // another.
        Column::new(column.column.id.as_str(), column.title)
            .paddings(cell_paddings())
            .width(
                self.widths
                    .get(col_ix)
                    .copied()
                    .unwrap_or_else(|| px(column_default_width(column))),
            )
            .min_width(px(COLUMN_MIN_WIDTH))
            // The ceiling is a reader's guard on a column they drag, and the flex
            // column is not one: `UI-SPEC` §10.1 gives it the viewport minus every
            // other visible column, so its own bound is the table. Handing it the
            // flat 640 instead meant the cap bound first on any table wider than
            // about 1530px — 734px of room resolved, 640 drawn, and the right-hand
            // 94px of a 1624px table was not a column at all. See
            // [`MAX_COLUMN_WIDTH`].
            .max_width(if column.is_flex() {
                px(self
                    .widths
                    .iter()
                    .map(|width| f32::from(*width))
                    .sum::<f32>())
            } else {
                px(MAX_COLUMN_WIDTH)
            })
            // `UI-SPEC` §10.1: the identifier column is sticky, so scrolling
            // sideways to compare two nodes does not move the name the reader is
            // reading them under.
            .when(column.class == ColumnClass::Identifier, |column| {
                column.fixed_left()
            })
            // A number column is right-aligned in the shared table's own header
            // too, so the label sits on the same edge as its values. It used to
            // be decided twice and could disagree.
            .when(column.is_right_aligned(), |column| column.text_right())
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        order: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let Some(&index) = self.visible.get(col_ix) else {
            return;
        };
        // The shared table's own control is not drawn any more, so this is a
        // backstop rather than a route. It walks the one cycle in this file, so a
        // pointer that ever did reach the component's control still lands on the
        // same three orders rather than on a second, differently-ordered set.
        let _ = order;
        view.update(cx, |view, cx| {
            view.cycle_column_sort(index, cx);
        });
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // `UI-SPEC` §4.4: the header band is transparent. It inherits the table's
        // own surface, which is the content surface the rows sit on, and the
        // 1px rule under it is the only thing that separates the two bands. A
        // fill of its own would be a second surface one step apart from the rows
        // — a difference of 1.038:1 in dark, which is below anything a reader
        // can see and above the threshold at which they start wondering why the
        // header is a slightly different colour.
        div().id("resource-header")
    }

    fn loading(&self, _cx: &App) -> bool {
        // A table with rows is never loading: the list is on screen and the
        // watch is behind it. This is `UI-SPEC` §4.14's "有 stale 缓存时绝不上
        // 骨架屏" as an invariant rather than as a condition — covering data a
        // reader can already read is a regression, not a loading state.
        self.shown.is_empty() && matches!(self.status, TableStatus::Listing)
    }

    fn render_loading(
        &mut self,
        size: Size,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // The shared table has already sized itself to the viewport and handed
        // over the row height, so the skeleton does not guess either.
        let spec = self.view.upgrade().map(|view| view.read(cx).spec.clone());
        let Some(spec) = spec else {
            return div().size_full().into_any_element();
        };
        // §4.14's middle tier. Under 500ms the wait is a flash and a skeleton of
        // anonymous bars is more motion than the reader is owed; a spinner with
        // the same words the strip already carries is honest and takes no room.
        if self.tier < LoadingTier::Skeleton {
            // A band at the top and nothing under it, rather than the band
            // stretched down the table. A 32px row of copy filling 900px of
            // space reads as a broken layout, and the point of the tier is that
            // the reader is not shown structure they have to unsee.
            return v_flex()
                .size_full()
                .child(loading_status_band(&spec, cx))
                .into_any_element();
        }
        let skeleton = loading_table(
            &spec,
            &self.columns,
            &self.visible,
            &self.widths,
            size.table_row_height(),
            window.viewport_size().height,
            self.waited,
            cx,
        );
        // Past two seconds the reader is owed a number as well as a spinner:
        // "loading" with no quantity is the thing `§4.14` calls out, because the
        // only question a reader has at two seconds is how much is coming.
        if self.tier < LoadingTier::Progress {
            return skeleton;
        }
        v_flex()
            .size_full()
            .child(skeleton)
            .child(loading_progress_band(&spec, self.progress, cx))
            .into_any_element()
    }

    fn render_empty(
        &mut self,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // The empty state is the view's: its words, its focus handles and its
        // recovery buttons. The delegate only carries it across.
        let Some(view) = self.view.upgrade() else {
            return div()
                .size_full()
                .bg(table_row_surface(cx))
                .into_any_element();
        };
        let element = view.update(cx, |view, cx| view.empty_state(window, cx));
        div()
            .size_full()
            .bg(table_row_surface(cx))
            .child(element)
            .into_any_element()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        self.header_cell(col_ix, cx)
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let (row, index) = match self.row(row_ix) {
            Some(row) => row,
            None => return div().id(("resource-row", row_ix)),
        };
        let Some((visual, background, _pending)) = self.row_state(row_ix, cx) else {
            return div().id(("resource-row", row_ix));
        };
        // The density-aware height, not the typography's own: `DataTypography::
        // table_row_height` floors at the 32px default, so a dense table would
        // have painted its 24px cells inside a 32px rail and the hover wash would
        // have stopped lining up with the row. One function decides the rhythm.
        let row_height = row_height(cx);
        let columns = Arc::clone(&self.columns);
        let visible = self.visible.clone();
        let row_label = format!(
            "{}. Enter opens details. {ROW_ACTIONS_KEYS} opens Row Actions.",
            row_accessible_label(&columns, &visible, &row.cells, row_age(row))
        );
        let position = row_ix;
        let hovered = row_hover_background(visual, background, cx);
        let mut row = div()
            .debug_selector(move || format!("resource-row-{position}"))
            .id(("resource-row", position))
            .aria_label(row_label)
            // The header is the grid's first row, so the data starts at 2. The
            // loading skeleton already numbered it that way, and two states that
            // disagree by one make a screen reader report the wrong row after an
            // action.
            .aria_row_index(grid_row_index(position))
            .aria_keyshortcuts(ROW_ACTIONS_KEYSHORTCUTS)
            // The trailing confidence marker belongs to this row, so it shows
            // while the row is hovered. The same group carries the hover wash, so
            // the two can never disagree about which row the pointer is on.
            .group(row_hover_group(position))
            .relative()
            .h(row_height)
            .bg(background)
            // `UI-SPEC` §4.4: hover is `rgba(255,255,255,.035)` in dark and
            // `rgba(15,17,20,.035)` in light, which is what the theme's row-hover
            // role solves for. The row had a hover *group* for its trailing
            // marker and no hover of its own, so a pointer travelling down a
            // 10,000-row list moved nothing at all — the reader could not see
            // which row a click would land on, and the only feedback was the
            // selection that the click itself caused.
            //
            // The wash is computed per state rather than swapped in as one
            // colour, because §4.4's `selected+hover → accent.wash 再 +4%` is
            // *more* of the state, not a different one. It used to be swapped in,
            // so the pointer arriving on a selected row replaced its accent wash
            // with the plain hover wash: the row went from selected-looking to
            // unselected-looking at the exact moment the reader was about to act
            // on it, and the only thing left marking it was the 2px rail.
            .group_hover(row_hover_group(position), |row| row.bg(hovered));
        // No rule under the row, and none around it. `UI-SPEC` §4.4 gives a table
        // row height and hover and nothing else: the rhythm comes from the row
        // height, and a rule under every row turns a list into a spreadsheet.
        //
        // The shared table draws one anyway, on the last row of a list that does
        // not fill its viewport, and the design allows a rule in three places and
        // this is not one of them. It also reads as a mistake: a rule under the
        // final row and under no other is a separator with nothing on either side
        // of it. The row covers it, because a row that ends the list has the
        // content surface below it and nothing else does.
        if position + 1 == self.shown.len() {
            row = row.child(
                div()
                    .debug_selector(move || format!("resource-row-tail-{position}"))
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(-design::border::LINE)
                    .h(design::border::LINE)
                    .bg(table_row_surface(cx)),
            );
        }
        //
        // The rail is 2px of `accent` flush to the row's leading edge, which is
        // where §4.4 puts it. It used to be inset by a gutter and accompanied by a
        // matching pad on every first cell, in the header and the body, so the
        // window's own sidebar divider and the row's mark were the same 8px apart
        // — the rail read as a border of the window rather than a mark on the
        // row. Flush, it reads as what it is. One colour and one width: the old
        // three-rail scheme (3px focused, 2px at 55% for a range, 2px in the
        // focus border colour) meant one mark said three different things.
        if visual.has_rail() {
            row = row.aria_active_descendant().child(
                div()
                    .debug_selector(move || format!("resource-row-rail-{position}"))
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(design::size::SELECTION_RAIL)
                    .bg(design::role::accent(cx)),
            );
        }
        // The keyboard's row is framed by a 1px `border.strong` rectangle, the
        // one border role the design reserves for a focused edge.
        //
        // Focus used to be told from selection by a wash four points of accent
        // lighter, which is not a difference a reader resolves and is not a
        // difference at all once the hue is gone. A frame is a shape: it survives
        // greyscale, it survives a reader who cannot separate the hues, and it
        // names one row without adding a second fill to decode. Drawn only when
        // the keyboard is on this row, so a click never leaves one behind.
        if visual.is_focused() {
            row = row.child(
                div()
                    .debug_selector(move || format!("resource-row-focus-edge-{position}"))
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .border_1()
                    .border_color(design::role::border_strong(cx)),
            );
        }
        let view = self.view.clone();
        let Some(focus) = self
            .view
            .upgrade()
            .map(|view| view.read(cx).table_focus.clone())
        else {
            return row;
        };
        row.on_mouse_down(MouseButton::Left, move |event, window, cx| {
            let Some(view) = view.upgrade() else {
                return;
            };
            // A click is not the keyboard arriving. Recording that here is the
            // only place the origin is knowable, and without it the row under the
            // pointer paints the same rail a `\u{2193}` does.
            view.update(cx, |view, _| view.keyboard_focus.set(false));
            window.focus(&focus, cx);
            // Selection and row actions use the snapshot index; the row itself is
            // addressed by its list position.
            // Shift extends from the anchor, Control or Command toggles.
            let extend = event.modifiers.shift;
            let toggle = event.modifiers.control || event.modifiers.platform;
            view.update(cx, |view, cx| {
                if extend {
                    view.extend_selection_to(index, window, cx);
                } else if toggle {
                    view.toggle_selection_at(index, window, cx);
                } else {
                    view.select_index(index, window, cx);
                }
            });
        })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some((_visual, background, pending)) = self.row_state(row_ix, cx) else {
            return div().into_any_element();
        };
        let Some((row, _)) = self.row(row_ix) else {
            return div().into_any_element();
        };
        let trailing = row_confidence(self.stale, row, &self.columns);
        let char_width = f32::from(self.typography.size) * CHAR_WIDTH_RATIO;
        row_cell(
            row_ix,
            col_ix,
            row,
            &self.columns,
            &self.visible,
            &self.widths,
            &self.typography,
            char_width,
            background,
            pending.as_ref(),
            self.has_status_column,
            Some(trailing),
            window,
            cx,
        )
        .unwrap_or_else(|| div().into_any_element())
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(view) = self.view.upgrade() else {
            return menu;
        };
        let Some((_row, index)) = self.row(row_ix) else {
            return menu;
        };
        let entries = view.update(cx, |view, cx| view.row_menu_entries_at(index, window, cx));
        entries.into_iter().fold(menu, PopupMenu::item)
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        let Some((row, _)) = self.row(row_ix) else {
            return String::new();
        };
        let Some(&index) = self.visible.get(col_ix) else {
            return String::new();
        };
        row.cells
            .get(index)
            .map(|cell| cell.text.to_string())
            .unwrap_or_default()
    }
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
            .tooltip(text_tooltip(label))
            .child(
                Icon::new(icon)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    .text_color(design::confidence::foreground(state, cx)),
            )
            .into_any_element(),
    )
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
///
/// `UI-SPEC` §4.14's 60–80% is a share of the **text area**, not of the cell: a
/// number column's value stands against the right edge of a box that is already
/// 16px in on both sides, so a bar measured against the cell is short by those
/// 32px as well as by its own share — and the padding was then charged a second
/// time by the clamp, which pinned the bar flush against the cell's padding and
/// 16px left of the value it stands in for.
fn skeleton_bar_width(width: Pixels, fraction: f32) -> Pixels {
    let cell = f32::from(width);
    let inset = 2.0 * f32::from(design::space::LG);
    let available = (cell - inset).max(0.0);
    px((available * fraction).clamp(0.0, available))
}

/// The width a column is drawn at before a reader has touched it.
///
/// `UI-SPEC` §10.1 and §10.2 decide it, and the decision is a table in
/// `columns.rs` rather than an arithmetic run here. The previous version shrank
/// every numeric column to whatever its own header measured, which meant the
/// widths on screen were a function of the installed font: a machine with a
/// wider Inter got wider columns, and `UI-SPEC` §10.2's 800px budget silently
/// stopped being true. A designed width is a number.
///
/// The one thing added to it is [`ResourceColumn::min_width`], the width below
/// which the column's own header stops being a word. §10.2's `Ready` is 56 and
/// its header needs 106, so every caller that used to measure a cell against 56
/// was measuring a cell that could only draw eight pixels of type in.
fn column_default_width(column: &ResourceColumn) -> f32 {
    column.min_width()
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
    move |window, cx| {
        let text = text.clone();
        let font = font.clone();
        let features = features.clone();
        let size = size;
        let width = width;
        Tooltip::element(move |_, _| {
            // The full value stays readable: a long one scrolls instead of
            // losing the tail behind a line clamp. Scrollable overflow needs a
            // stateful element, and a tooltip must stay id-free, so the style
            // is set directly.
            let mut content = div()
                .when_some(font.clone(), |this, font| this.font(font))
                .when_some(features.clone(), |this, features| {
                    this.font_features(features)
                })
                .when_some(size, |this, size| this.text_size(size))
                .when_some(width, |this, width| this.w(width))
                .whitespace_normal()
                .max_h(TOOLTIP_MAX_HEIGHT)
                .child(text.clone());
            content.style().overflow.y = Some(gpui_kit::Overflow::Scroll);
            content
        })
        .build(window, cx)
    }
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

/// Returns the column header height: the header band less the 1px rule the
/// shared table draws under it.
///
/// `UI-SPEC` §4.4 fixes the header at 32px, which is `design::size::ROW` and
/// not the current density: a dense table still reads a column name in a 32px
/// band, because the header is chrome and the density is a property of the rows
/// a reader is scanning. A 24px header with an 11px label has no line to sit on.
fn header_cell_height() -> Pixels {
    design::size::TABLE_HEADER - design::border::LINE
}

/// Returns the color a column header's label draws with.
///
/// One ink ladder with no hover step: `fg.secondary` at rest and `fg.primary`
/// for the sorted column — never the accent, which is the focus channel. Hover
/// is *shape*-only (the column's own wash and the control that reveals beside
/// the word — see [`header_label_weight`]); recolouring an unsorted label under
/// the pointer made it as loud as the sorted one, and the band's one question —
/// which column the table is ordered by — became unreadable.
///
/// `Relevance` is deliberately not one of the two bright cases. The order it names
/// belongs to the *query*, not to a column, and the old rule read "not Unsorted",
/// so a single typed filter lit all seven labels at once and the band stopped
/// having a sorted column to point at. See [`affordance_marks_the_column`].
fn header_label_color(affordance: SortAffordance, cx: &App) -> Hsla {
    if affordance_marks_the_column(affordance) {
        design::role::fg_primary(cx)
    } else {
        // `fg.secondary`, not `fg.tertiary`. A column header is the one label a
        // reader has to find before they can read anything at all, and the
        // quietest rung is the one the design reserves for things nobody must
        // read to do the job — a count, a placeholder, a group head. An 11px
        // uppercase word at that rung is a word a reader has to lean in for.
        design::role::fg_secondary(cx)
    }
}

/// The weight a header label is set at.
///
/// Weight, not ink, is what separates the sorted column from its neighbours —
/// and it is a channel that survives greyscale, which the 11px size does not. The
/// label used to be `SEMIBOLD` on every column, which made the weight a property
/// of the band rather than of the state, and then the *ink* was asked to carry
/// "sorted" as well. Hover was given the ink too, and a pointer crossing an
/// unsorted column made its label as loud as the sorted one, so the one question
/// a header exists to answer — which column is the table ordered by — became
/// unanswerable from the resting pixels.
///
/// Hover therefore says nothing with the label. It already says something twice
/// over, in shapes that cost no ink: the column's own wash and the control that
/// appears beside the word.
fn header_label_weight(affordance: SortAffordance) -> FontWeight {
    if affordance_marks_the_column(affordance) {
        design::text::SEMIBOLD
    } else {
        design::text::MEDIUM
    }
}

/// Reports whether this column is the one carrying the table's order.
///
/// `UI-SPEC` §4.4 gives the band exactly one mark, on the sorted column, and
/// `docs/mockup` draws `Name ^` and nothing else. `Relevance` is the one
/// affordance that names no column: the rows are ranked by how well they match
/// the query, so *every* column returns it and a rule of "not Unsorted" turned a
/// single typed filter into seven arrows and seven `fg.primary` labels in a 32px
/// band — the row of icons §4.4 rules out, back again through the side door.
///
/// It stays in the affordance because the table really is in that order, and
/// because the header's description is where a reader is told so and how to
/// leave it. It just does not get to paint.
fn affordance_marks_the_column(affordance: SortAffordance) -> bool {
    matches!(
        affordance,
        SortAffordance::Ascending | SortAffordance::Descending
    )
}

fn header_accessibility_description(title: &str, affordance: SortAffordance) -> String {
    // The order is the shared table's sort control, which sits beside the label
    // and draws its own glyph; `Shift+Enter` runs the same cycle from the
    // keyboard.
    let next_action = match affordance {
        SortAffordance::Ascending => {
            "Activate the column's sort control, or press Shift+Enter, to sort from high to low."
        }
        SortAffordance::Descending => {
            "Activate the column's sort control, or press Shift+Enter, to return to the default order."
        }
        SortAffordance::Relevance => {
            "Activate the column's sort control, or press Shift+Enter, to sort explicitly."
        }
        SortAffordance::Unsorted => {
            "Activate the column's sort control, or press Shift+Enter, to sort from low to high."
        }
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
fn focus_table(cx: &mut gpui_kit::VisualTestContext, view: &gpui_kit::Entity<PodsView>) {
    let handle = view.read_with(cx, |view, cx| view.table_focus_handle(cx));
    cx.update(|window, cx| window.focus(&handle, cx));
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui_kit::{Modifiers, TestAppContext};
    use k8s_core::controller::{StoreEvent, StoreOp};
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::panels::InspectorPanel;
    use crate::table_view::columns::pod_columns;
    use crate::table_view::source::{ResourceSource, SourceEvent, Subscription};

    /// `Shift+Enter` walks the same three states from the other end, starting
    /// from the order the table is already in.
    #[test]
    fn the_keyboard_sort_cycle_covers_the_same_three_orders() {
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
            "the cycle starts over on the selected column"
        );
        // Sorting the default column itself has two states, not three: the
        // order it already shows is the order the third press returns to.
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

    // A permanent glyph on every header says "sortable" once per column and
    // reads as noise, so only the sorted column shows a direction and the rest
    // keep theirs for the pointer.

    // A header marks sort with a glyph and a label, never with a filled cell:
    // the accent highlight in a list is the focus channel, and one color may
    // only mean one thing.
    #[gpui_kit::test]
    fn a_column_header_never_paints_the_row_selection_fill(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            let selection_fills = [
                design::row_selected_bg(cx),
                design::row_selected_bg(cx).opacity(0.5),
            ];
            // The header's ink set is closed: resting and sorted. Hover is shape
            // (wash + revealed control), never a third ink, so there is no
            // hovered value to test alongside them.
            let header_colors = [
                header_label_color(SortAffordance::Unsorted, cx),
                header_label_color(SortAffordance::Ascending, cx),
                header_label_color(SortAffordance::Descending, cx),
            ];
            for fill in selection_fills {
                for ink in header_colors {
                    assert_ne!(
                        ink, fill,
                        "a header label must not borrow the row selection fill"
                    );
                }
            }
            // And the header's own rule is a border role, never the accent that
            // means "this row is selected".
            for ink in [
                design::role::border_subtle(cx),
                design::role::border_base(cx),
            ] {
                assert_ne!(ink, design::role::accent(cx));
            }
        });
    }

    // One header cannot put the sort control on the far side of a right-aligned
    // label and the near side of a left-aligned one: a number used to render
    // `⇕ Ready` with the control a column-width away from its label. The control
    // is the app's own now, and it sits in the header cell beside the label, so
    // the contract is that *every* column has one — the reader has a pointer
    // target in every header — and that exactly one of them is the sorted one.
    //
    // The shared table is handed no sort state at all, because it draws a
    // `ChevronsUpDown` on every column it is given one and §4.4 gives the band
    // one mark.
    #[gpui_kit::test]
    fn every_column_gets_a_sort_control_next_to_its_own_label(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1600.), px(1000.)));
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

        for (id, header, index) in [
            ("restarts", "pod-header-restarts", numeric),
            ("status", "pod-header-status", text),
        ] {
            view.update(cx, |view, cx| {
                view.apply_sort(Some(Sort::ascending(index)), cx)
            });
            cx.run_until_parked();
            // The component draws a sort control for every column that carries a
            // sort state, so a state per column is a glyph per column. None is
            // handed over: the band keeps its one mark and the app draws the rest.
            cx.update(|_window, cx| {
                let state = view.read_with(cx, |view, _| view.table_state().cloned());
                let state = state.expect("the shared table");
                let state = state.read(cx);
                let orders: Vec<Option<ColumnSort>> = (0..state.delegate().columns_count(cx))
                    .map(|col| state.delegate().column(col, cx).sort)
                    .collect();
                assert!(
                    orders.iter().all(Option::is_none),
                    "{id}: the component draws no sort control of its own"
                );
            });
            // Every header carries the app's own control instead, so the reader
            // still has a pointer target in every one of them, in both alignments.
            // The label and the control are one cell, so the control's own bounds
            // have to sit inside the label's: that is what "beside its own label"
            // means once the component is no longer laying the two out.
            let header_bounds = cx
                .debug_bounds(header)
                .unwrap_or_else(|| panic!("{id}: the column keeps its own header cell"));
            let control = Box::leak(format!("pod-sort-control-{index}").into_boxed_str());
            let control_bounds = cx
                .debug_bounds(control)
                .unwrap_or_else(|| panic!("{id}: the header has a pointer target that sorts it"));
            assert!(
                control_bounds.left() >= header_bounds.left()
                    && control_bounds.right() <= header_bounds.right(),
                "{id}: the control sits inside its own header, not a column away"
            );
            // And exactly one label is the emphasised one, so the band carries
            // one mark rather than one per column.
            assert!(
                cx.debug_bounds("pod-header-sorted-label").is_some(),
                "{id}: the sorted column's label is the emphasised one"
            );
        }
    }

    // A numeric column that reserves 80 to 100px for two or three characters
    // takes the room away from the name column, which is the one that
    // truncates. The default follows the value instead, and follows the font
    // the reader actually set.

    // The Data font size setting enlarged the text but not the columns, so
    // every numeric value started clipping the moment the reader raised it.

    // A resource the app could not read is not healthy, and a cluster that
    // stopped answering is not stale on one row only. The row carries no
    // freshness of its own, so the answer comes from the status text it already
    // shows and from the table it is drawn in.

    // A current row needs no marker: a mark on every row would invent a second
    // thing to read.

    // Health is filled or heavy and confidence is hollow, so a row that carries
    // both never merges the two axes into one mark.

    // A screen reader used to hear "Status: Pending" and stop there. The verdict
    // only ever existed as a glyph, so nothing told the reader that Pending is a
    // warning, and nothing distinguished it from a pod that is merely waiting.

    // Losing keyboard focus must not leave the selection looking active.

    // A multi-row selection must mark one command target, not every row.

    // Selection is a fact about the resource, not about where the keyboard is,
    // so a row that lost focus keeps the fill it had. The two selected states
    // used to paint different washes, and the weaker one is what made a
    // selection look like it had evaporated when the reader alt-tabbed away.
    #[gpui_kit::test]
    fn selection_survives_the_table_losing_focus(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            let selected = row_selection_bg(cx);
            let focused = design::row_focus_bg(cx);
            let plain = table_row_surface(cx);
            let fill = |visual| row_visual_background(visual, selected, focused, plain);
            assert_eq!(
                fill(RowVisual::SelectedUnfocused),
                fill(RowVisual::SelectedFocused),
                "losing focus must not unselect a row"
            );
            // And a selection is still not a hover or a cursor: the three fills a
            // reader can be looking at have to be three colours.
            for visual in [
                RowVisual::SelectedUnfocused,
                RowVisual::SelectedFocused,
                RowVisual::SelectedMember,
            ] {
                assert_ne!(
                    fill(visual),
                    plain,
                    "a selected row cannot match an unselected one"
                );
                assert_ne!(
                    fill(visual),
                    design::row_hover_bg(cx),
                    "a selection cannot resolve to the hover wash"
                );
                assert_ne!(
                    fill(visual),
                    focused,
                    "a selection cannot resolve to the keyboard cursor's wash"
                );
            }
        });
    }

    // The rail names one command target, so a member and the active row stay
    // told apart; the focus edge is what tells a reader which of the *selected*
    // rows the next keystroke will act on, and it is a shape so it survives
    // greyscale. A range member used to take the muted fill, which measured
    // 1.018:1 in dark and 1.022:1 in light against the row beside it: the wash was
    // the only signal a member had and neither appearance could show it.
    #[gpui_kit::test]
    fn a_range_member_reads_as_selected_and_the_keyboard_row_as_focused(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            let selected = row_selection_bg(cx);
            let focused = design::row_focus_bg(cx);
            let plain = table_row_surface(cx);
            let fill = |visual| row_visual_background(visual, selected, focused, plain);
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
            }
            // The rail still names one command target, so a member and the
            // active row stay told apart without a second colour.
            assert!(!RowVisual::SelectedMember.has_rail());
            assert!(RowVisual::SelectedFocused.has_rail());
            // Focus is a hairline, not a fourth wash: exactly the rows the
            // keyboard can act on have one, and no others do.
            assert!(RowVisual::FocusRing.is_focused());
            assert!(RowVisual::SelectedFocused.is_focused());
            assert!(!RowVisual::SelectedMember.is_focused());
            assert!(!RowVisual::SelectedUnfocused.is_focused());
            assert!(!RowVisual::Plain.is_focused());
            // `UI-SPEC` §4.4: no zebra. The variant is gone rather than set to
            // the plain fill, because a state nobody can reach and a state that
            // happens to be invisible are different mistakes and only the first
            // one is a deletion.
            assert_eq!(
                row_visual(false, false, true, true, false, 1),
                RowVisual::Plain,
                "a row's position cannot change how it is painted"
            );
        });
    }

    // The keyboard cursor used to share the hover token, so a row the keyboard
    // was on and a row the pointer was over were the same colour, and in the
    // light appearance the focus wash was quieter than the zebra stripe beside
    // it.
    #[gpui_kit::test]
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
                row_selection_bg(cx),
            ] {
                assert_ne!(
                    state,
                    table_row_surface(cx),
                    "a row state must not be the table's own surface"
                );
            }
            // `PROMPT` §2.1 #10: a click produces no cursor. The two states
            // differ by *origin*, not by value, so no colour assertion can catch
            // this one — it is the flag, and the flag has to be the only thing
            // that separates them.
            assert_eq!(
                row_visual(false, false, true, true, false, 0),
                RowVisual::FocusRing,
                "the keyboard leaves a cursor on the first row"
            );
            assert_eq!(
                row_visual(false, false, true, false, false, 0),
                RowVisual::Plain,
                "a click leaves no cursor behind"
            );
        });
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn resource_grid_fills_viewport_after_scrollbar_cleanup(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
        cx.run_until_parked();

        let grid = cx.debug_bounds("resource-grid").expect("resource grid");
        // `UI-REDESIGN` D16 deleted the pill toolbar, and `UI-SPEC` §4.2 moved the
        // query box into the [`design::size::RESOURCE_HEADER`] band, which
        // `shell/panels.rs` owns.
        // What this file can still be held to is the part that is its own: the
        // table fills the 960px window it was given, and its own bands fit inside
        // it. The filter's position is Wave 2's to assert.
        let summary = cx.debug_bounds("table-summary").expect("summary strip");
        assert!(f32::from(grid.size.height) > 400.);
        assert!(summary.right() <= px(960.0));
        assert!(
            cx.debug_bounds("table-status").is_none(),
            "the pill toolbar is gone and nothing took its place"
        );
    }

    // `DESIGN.md §3.4` files `surface` under tables and inputs. The table used
    // to paint its rows on `canvas`, and the loading skeleton on the canvas too,
    // so the one level of the ramp a reader stares at for eight hours was never
    // on screen.
    #[gpui_kit::test]
    fn the_table_and_its_skeleton_sit_on_the_content_surface(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                table_row_surface(cx),
                design::role::surface_content(cx),
                "a row composites onto the content surface, not the app's own field"
            );
            assert_ne!(
                table_row_surface(cx),
                design::role::surface_app(cx),
                "the table must not float on the app surface"
            );
            assert_ne!(
                table_row_surface(cx),
                design::role::surface_chrome(cx),
                "a table is content, not chrome"
            );
        });
        assert!(cx.debug_bounds("resource-grid").is_some());
        assert!(
            cx.debug_bounds("resource-row-0").is_some(),
            "the live table paints the rows the skeleton stood in for"
        );
    }

    // The last column used to opt out of resizing, so the one column that clips
    // mid-glyph had no pointer way out of it. Every column has to stay draggable.
    #[gpui_kit::test]
    fn every_visible_column_keeps_its_resize_handle(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let (state, visible) = view.read_with(cx, |view, _cx| {
                (view.table_state().cloned(), view.visible_columns.len())
            });
            let Some(state) = state else {
                return;
            };
            let state = state.read(cx);
            let columns: Vec<_> = (0..state.delegate().columns_count(cx))
                .map(|index| state.delegate().column(index, cx))
                .collect();
            assert_eq!(columns.len(), visible);
            assert!(
                columns.iter().all(|column| column.resizable),
                "the last column must not opt out of resizing"
            );
        });
    }

    // A partial column at the right edge used to read as the end of the data:
    // no ellipsis, no fade, and no scrollbar thumb anywhere in the last 60px.

    // Every column edge has to have a band on it the pointer can find, and a
    // partial column at the right edge has to say it is clipped. The shared
    // table owns both now: it mounts a drag band on the boundary of every
    // resizable column and it draws its own horizontal scrollbar. The contract
    // the app can still break is leaving a column un-resizable or handing the
    // table its own narrower clip, so that is what this asserts: every visible
    // column reaches the component resizable, inside the width bounds a reader
    // can drag to, and the columns tile the header so every edge is a boundary.
    #[gpui_kit::test]
    fn every_column_edge_reaches_the_components_resize_band(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();

        let (state, headers) = view.read_with(cx, |view, _| {
            (
                view.table_state().cloned(),
                view.visible_columns
                    .iter()
                    .map(|&index| view.columns[index].column.id.to_string())
                    .collect::<Vec<_>>(),
            )
        });
        let Some(state) = state else {
            return;
        };
        cx.update(|_, cx| {
            let state = state.read(cx);
            let columns: Vec<_> = (0..state.delegate().columns_count(cx))
                .map(|index| state.delegate().column(index, cx))
                .collect();
            assert_eq!(columns.len(), headers.len());
            assert!(
                state.col_resizable,
                "the shared table must mount its resize bands at all"
            );
            // The table's own width, which is what the flex column's ceiling is:
            // `UI-SPEC` §10.1 hands that column the viewport minus every other
            // visible column, so a flat ceiling on it is a second, smaller budget
            // and the columns stop short of the panel.
            let table: f32 = columns.iter().map(|column| f32::from(column.width)).sum();
            for column in &columns {
                assert!(
                    column.resizable,
                    "every column edge gets a band, including the last"
                );
                assert_eq!(f32::from(column.min_width), COLUMN_MIN_WIDTH);
                // A dragged column stops at [`MAX_COLUMN_WIDTH`]; the flex column
                // stops at the table, because that is all the room there is. Both
                // have to be at or above the width the column is drawn at, or the
                // component clamps it and the table is narrower than it resolved.
                let ceiling = if f32::from(column.max_width) >= table {
                    table
                } else {
                    MAX_COLUMN_WIDTH
                };
                assert_eq!(f32::from(column.max_width), ceiling);
                assert!(
                    f32::from(column.width) <= f32::from(column.max_width),
                    "a column wider than its own ceiling is clamped by the component, and the \
                     table stops filling the panel"
                );
                assert!(
                    f32::from(column.width) >= f32::from(column.min_width),
                    "a column narrower than its own minimum has no edge to drag"
                );
            }
        });
        // The header is one cell per column, so every column edge is a real
        // boundary the component can hang its band on.
        for (id, header) in [
            ("name", "pod-header-name"),
            ("namespace", "pod-header-namespace"),
            ("status", "pod-header-status"),
        ] {
            if headers.iter().any(|visible| visible == id) {
                assert!(
                    cx.debug_bounds(header).is_some(),
                    "{id}: every visible column has a header cell"
                );
            }
        }
        // And the component's own horizontal scrollbar is what says a column is
        // still clipped, so the grid must not paint a scrollbar strip of its
        // own over it.
        assert!(
            cx.debug_bounds("resource-grid").is_some(),
            "the grid keeps the frame the table and its scrollbar sit in"
        );
    }

    // The Status header used to carry a one-click problems toggle *and* the
    // value popover, whose first row is the same filter (§4.10 puts `Only
    // problems` there with its `alt-shift-p` chip). Two controls for one filter,
    // and when the filter was on they drew the same glyph 20px apart. One
    // control per filterable column is the whole invariant, and it is the reason
    // a reader's second column filter needs no learning.
    #[gpui_kit::test]
    fn every_filterable_column_carries_one_filter_control(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        for (id, selector) in [
            ("namespace", "column-filter-trigger-namespace"),
            ("status", "column-filter-trigger-status"),
            ("node", "column-filter-trigger-node"),
        ] {
            let trigger = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{id} carries a value filter, so it carries a trigger"));
            assert_eq!(
                trigger.size.width, trigger.size.height,
                "{id}: a square hit area, so a click one pixel off the glyph still lands"
            );
            assert_eq!(
                f32::from(trigger.size.width),
                f32::from(design::size::HIT_MIN),
                "{id}: `design::size::HIT_MIN`, the same box on every column"
            );
            assert!(
                f32::from(trigger.size.width) > f32::from(design::size::STATUS_MARKER),
                "{id}: the box is bigger than the glyph it holds"
            );
        }
        assert!(
            cx.debug_bounds("status-filter").is_none(),
            "the standalone problems toggle is gone: §4.10 puts `Only problems` in the \
             popover, and a second control in the band drew the same funnel as this one"
        );
    }

    // The Overview counts a cluster's problem rows and then has to route to
    // them. Its button opened the Pods table with no way to ask for the filter
    // that page was reporting on, so the reader landed on an unfiltered list of
    // 10,101 rows and had to find the 9,900 themselves.
    #[gpui_kit::test]
    fn an_external_problems_filter_hides_the_healthy_rows_and_comes_back(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(mixed_health_factory(), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
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
        // so hiding the column takes its filter control away and leaves the filter
        // working. The other way round, the filter silently stopped filtering.
        view.update(cx, |view, cx| {
            view.set_problems_only(false, cx);
            view.toggle_column_visibility(2, cx);
            view.set_problems_only(true, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("column-filter-trigger-status").is_none(),
            "the control belongs to the column that is no longer there"
        );
        assert!(
            cx.debug_bounds("resource-row-1").is_none(),
            "a hidden status column does not turn the filter into no filter"
        );
    }

    /// The popover and the query box are one string, or they are two states that
    /// will diverge. This is the one invariant that cannot be checked by reading the
    /// code, because both halves are correct on their own and only the *round trip*
    /// is wrong: a popover that writes a clause the box cannot parse back leaves the
    /// reader editing a string that does not mean what the table is showing, and
    /// nothing on screen says so.
    #[test]
    fn the_popover_writes_into_the_query_the_box_holds() {
        // Ticking a value writes a clause, and every other clause the reader wrote
        // survives it *verbatim* — not re-rendered, because a popover that rewrote
        // the reader's words would be editing them.
        assert_eq!(
            set_field_values("ns=prod restarts>3", Field::Status, &["Failed".to_owned()]),
            "ns=prod restarts>3 status=Failed",
            "a tick adds its clause and leaves the reader's alone"
        );
        // Two ticks are one clause with two values, which is why the grammar gives
        // every text field a set rather than only the namespace.
        assert_eq!(
            set_field_values("", Field::Status, &["Failed".into(), "Pending".into()]),
            "status=Failed,Pending"
        );
        // A second tick replaces the first rather than adding a second clause: two
        // `status=` clauses would be a conjunction of two allow-lists, which admits
        // nothing.
        assert_eq!(
            set_field_values("status=Failed", Field::Status, &["Pending".into()]),
            "status=Pending"
        );
        // Unticking the last value clears the clause, which is the same thing
        // `Clear` does — one behaviour, not two.
        assert_eq!(
            set_field_values("ns=prod status=Failed", Field::Status, &[]),
            "ns=prod"
        );
        // The problems toggle is one clause among the others, added and removed the
        // same way.
        assert_eq!(
            with_problems_clause("ns=prod", true),
            format!("ns=prod {PROBLEMS_CLAUSE}"),
            "the problems clause joins the reader's own"
        );
        assert_eq!(
            with_problems_clause(&format!("ns=prod {PROBLEMS_CLAUSE}"), false),
            "ns=prod",
            "and comes back out without touching the rest"
        );
        // And the string the GUI produced is a query the parser accepts, which is
        // the half of the round trip that decides whether the box and the table can
        // ever agree.
        for written in [
            set_field_values("ns=prod", Field::Status, &["Failed".to_owned()]),
            with_problems_clause("ns=prod", true),
        ] {
            let filter = Filter::parse(&written).unwrap_or_else(|error| {
                panic!("the GUI wrote `{written}`, which does not parse: {error}")
            });
            assert_eq!(
                filter.to_query(),
                written,
                "`{written}` parses, so the box is showing the table's own filter"
            );
        }
        // A clause the popover did not write is still the same filter: this is what
        // makes typing `status!=Running` and ticking the header one action rather
        // than two spellings of one thing.
        assert!(has_problems_clause("status!=Running"));
        assert!(has_problems_clause(&format!("ns=prod {PROBLEMS_CLAUSE}")));
        assert!(
            !has_problems_clause("status=Running"),
            "an allow-list is not the problems filter"
        );
        assert!(!has_problems_clause(""), "an empty query has no clause");
    }

    // `UI-SPEC` §4.4 puts the selection rail flush at the row's leading edge and
    // gives the header no rail at all. Both halves of the old arrangement were
    // wrong independently: a rail inset by a gutter butted into the sidebar
    // divider and read as a border of the window, and a *second* rail under the
    // header said the same thing 32px higher.
    #[gpui_kit::test]
    fn the_row_rail_sits_at_the_tables_leading_edge(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let row = cx
            .debug_bounds("resource-row-rail-0")
            .expect("the row rail");
        assert_eq!(
            f32::from(row.size.width),
            f32::from(design::size::SELECTION_RAIL),
            "§4.4 fixes the rail at 2px"
        );
        assert_eq!(
            f32::from(row.left()),
            f32::from(cx.debug_bounds("resource-row-0").expect("the row").left()),
            "the rail is flush to the row's own leading edge, with no gutter"
        );
        assert!(
            cx.debug_bounds("resource-column-rail-0").is_none(),
            "the header has no column rail; it owns a bottom rule and a label"
        );
    }

    // Focus has to be visible after the hue is taken away, so it is drawn as a
    // shape — a hairline around the row the keyboard is on — rather than as
    // another wash of the accent the selection already uses. A test can only
    // check that the edge is there and that it lands on the keyboard's row; that
    // it is a *shape* rather than a tint is the reason it is a border at all.
    #[gpui_kit::test]
    fn the_keyboard_row_carries_a_focus_edge_and_the_rest_do_not(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let edge = cx
            .debug_bounds("resource-row-focus-edge-0")
            .expect("the keyboard row's focus edge");
        let row = cx.debug_bounds("resource-row-0").expect("the row");
        assert!(
            edge.size.width >= row.size.width,
            "the edge bounds the whole row, not one cell of it"
        );
        assert!(
            cx.debug_bounds("resource-row-focus-edge-1").is_none(),
            "only the row the keyboard is on carries the edge"
        );
    }

    // The control's box is a fixed square, so the focus border cannot move the
    // glyph, and the hit area has margin instead of sitting exactly on the
    // 20px minimum.
    #[gpui_kit::test]
    fn the_column_filter_triggers_do_not_overlap_each_other(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        // Two controls inside one column, or two columns' controls sharing an
        // edge, is how the Status band came to draw the same funnel twice 20px
        // apart. Geometry is the only thing that catches it: both controls were
        // individually correct.
        let mut boxes: Vec<_> = [
            ("namespace", "column-filter-trigger-namespace"),
            ("status", "column-filter-trigger-status"),
            ("node", "column-filter-trigger-node"),
        ]
        .into_iter()
        .filter_map(|(id, selector)| cx.debug_bounds(selector).map(|bounds| (id, bounds)))
        .collect();
        boxes.sort_by_key(|(_, bounds)| bounds.origin.x);
        for pair in boxes.windows(2) {
            let (left_id, left) = pair[0];
            let (right_id, right) = pair[1];
            assert!(
                left.origin.x + left.size.width <= right.origin.x,
                "{left_id} and {right_id} put two filter controls next to each other: the \
                 reader cannot tell which column either one belongs to"
            );
        }
    }

    // The live table numbered its first data row 1 while the loading skeleton
    // numbered it 2, so a screen reader reported the wrong row after an action.
    // Both states now read one function.

    // A failed request removes the pending state and shows an error.
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
        assert!(cx.debug_bounds("MENU_ITEM-Reset column widths").is_some());
        // The Status header owns the filter, so the menu keeps it keyboard
        // reachable under a group of its own.
        assert!(cx.debug_bounds("MENU_ITEM-Show only problems").is_some());
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn hiding_the_last_visible_column_points_to_open_columns(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        // `UI-SPEC` §10.2 hides `Image` and `IP` by default, so this walks the
        // columns that *start* visible rather than every index: toggling a hidden
        // column would show it, and the fixture would end up with two visible
        // columns and no last one to hide.
        view.update(cx, |view, cx| {
            let visible = view.visible_columns.clone();
            for index in visible.into_iter().skip(1) {
                view.toggle_column_visibility(index, cx);
            }
            let last = view.visible_columns[0];
            view.toggle_column_visibility(last, cx);
        });
        assert!(view.read_with(cx, |view, _| {
            view.notice
                .as_ref()
                .is_some_and(|notice| notice.message == "Open Columns to show a hidden column.")
        }));
    }

    #[gpui_kit::test]
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
    #[gpui_kit::test]
    fn hiding_the_sorted_column_leaves_relevance_mode(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();

        let filter = view.read_with(cx, |view, _| view.filter.clone());
        cx.update(|window, cx| filter.update(cx, |input, cx| input.set_text("pod", window, cx)));
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
            // `UI-REDESIGN` D16 deleted the toolbar and with it the only place the
            // order was spelled out in words. The header owns it now, and the
            // contract that survives is the one the *rows* depend on: an explicit
            // sort takes the table out of relevance mode, and the fallback is a
            // real sort on a visible column rather than a silent "no sorting".
            assert_eq!(
                view.selected_column, 0,
                "hiding the sorted column moved the keyboard's column to a visible one"
            );
        });
    }

    // Every recovery button disappears with the state it repairs, so focus
    // must not stay on it. The toolbar Retry had no focus handle before.
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
            .debug_bounds("MENU_ITEM-Open details")
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
    #[gpui_kit::test]
    fn the_row_menu_has_one_entry_that_opens_details(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(cx.debug_bounds("MENU_ITEM-Open details").is_some());
        assert!(
            cx.debug_bounds("MENU_ITEM-Describe").is_none(),
            "two labels for one action is a duplicate path"
        );
    }

    // The shared table hangs its own `.context_menu` on the table's inner div,
    // and that menu's builder is a closure the table entity runs. So a plain
    // right-click on a row rebuilds the menu from inside the table's own
    // update, and the delegate re-enters this view to collect the entries.
    // Reading the table back from there aborted the app, which is why
    // `reveal_row` records the position and `reveal_pending_row` applies it.
    // The row menu is the only path that reaches the delegate this way, so
    // nothing else in the suite would notice it going back.
    #[gpui_kit::test]
    fn a_row_menu_built_inside_the_tables_own_update_does_not_read_the_table_back(
        cx: &mut TestAppContext,
    ) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let table = view
            .read_with(cx, |view, _cx| view.table_state().cloned())
            .expect("a frame has built the table");
        // Standing in for the shared table's menu builder: the table entity is
        // mid-update when the delegate is asked for its entries.
        cx.update(|window, cx| {
            build_menu(window, cx, move |menu, window, cx| {
                table.update(cx, move |state, cx| {
                    state.delegate_mut().context_menu(0, menu, window, cx)
                })
            });
        });
        cx.run_until_parked();
        // The row still had to be selected, and the table still has to be
        // focused, or the fix would be to skip the work rather than defer it.
        assert_eq!(view.read_with(cx, |view, _cx| view.selected_uids.len()), 1);
        assert!(cx.debug_bounds("resource-grid").is_some());
    }

    /// `PROMPT` §2.1 #13 and `UI-SPEC` §4.4: 32 comfort (the default), 28 normal,
    /// 24 dense. Comfort is the default *because* the default view is already
    /// filtered down to the rows that need attention, so the common case is a
    /// dozen rows and there is nothing to gain by making them tight.
    ///
    /// The value was a field and a getter before this round and nothing read it,
    /// so the whole ladder was a comment. It is read by the row, by the shared
    /// table's own sizing and by the page arithmetic, from one function.
    #[gpui_kit::test]
    fn density_moves_the_row_and_nothing_else(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let measured = |cx: &mut gpui_kit::VisualTestContext| {
            cx.debug_bounds("resource-row-0")
                .map(|row| f32::from(row.size.height))
        };
        assert_eq!(measured(cx), Some(32.0), "comfort is the default");
        // The header is chrome and does not follow the density: a 24px header
        // with an 11px label has no line to sit on, and `§4.4` fixes it at 32.
        let header = cx.debug_bounds("pod-header-name").expect("the name header");
        assert_eq!(f32::from(header.size.height), 31.0);

        for (density, expected) in [
            (Density::Normal, 28.0),
            (Density::Dense, 24.0),
            (Density::Comfort, 32.0),
        ] {
            view.update(cx, |view, cx| view.set_density(density, cx));
            cx.run_until_parked();
            assert_eq!(
                measured(cx),
                Some(expected),
                "{density:?} is {expected}px, and the header is still 32"
            );
            assert_eq!(
                f32::from(
                    cx.debug_bounds("pod-header-name")
                        .expect("the name header")
                        .size
                        .height
                ),
                31.0,
                "§4.4: the header band is chrome, not a row"
            );
        }
    }

    /// `UI-SPEC` §4.13, §4.14 and §4.15 are the three states a Kubernetes table
    /// spends most of its life in, and they are the three most likely to be wrong
    /// in ways a screenshot of *data* cannot show. These assert the numbers the
    /// spec fixes, in both appearances, because that is what a code review can
    /// check and what a reader feels.
    ///
    /// `§4.13`: a 24px lead mark in the resting ink and a 15px semibold title,
    /// vertically centred with 48px of slack rather than pinned to the top.
    #[gpui_kit::test]
    fn the_empty_state_is_a_24px_icon_over_a_15px_title(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| Box::new(EmptySource) as Box<dyn ResourceSource>);
        let (_view, cx) = cx.add_window_view(|_, cx| PodsView::new(factory, None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let state = cx.debug_bounds("resource-empty").expect("the empty state");
        let icon = cx
            .debug_bounds("empty-icon")
            .expect("the empty state's icon");
        let title = cx
            .debug_bounds("empty-title")
            .expect("the empty state's title");
        assert_eq!(
            f32::from(icon.size.height),
            f32::from(design::icon::LEAD),
            "§4.13: a 24px icon, and not a coloured large one"
        );
        assert!(
            title.size.height >= design::text::TITLE_LINE_HEIGHT,
            "§4.13: the title is 15/600, and it was 13 — the same size as a cell"
        );
        // Centred, with the 48px of slack `§4.13` asks for on each side. The slack
        // is what stops the block from reading as pinned to the top of a tall
        // table, which is where a reader's eye does not start.
        let above = f32::from(title.top() - state.top());
        let below = f32::from(state.bottom() - title.bottom());
        assert!(
            above >= 48.0 && below >= 48.0,
            "the block is centred with 48px either side: {above} above, {below} below"
        );
        assert!(
            (above - below).abs() < 8.0,
            "and it is centred, not merely padded: {above} against {below}"
        );
    }

    /// `R1h`: an empty state's description is `body 13`, beside a `title 15/600`.
    ///
    /// §2.3's rule is that the four levels separate by **2px and by weight**, and it
    /// names `body 13` against `metadata 11` as the pair that fails it. The table's
    /// own description was `caption 11` directly under `title 15/600` — four pixels
    /// apart and the same weight, so the sentence read as a second heading rather
    /// than as the note under one, and it carried `caption`'s `+0.06em` tracking,
    /// which is letterspacing meant for short uppercase labels.
    ///
    /// The comparison the token cannot make is the one R1 asks for: the Dock made
    /// this exact move for the exact same reason (`panels/dock.rs`,
    /// `label_body(hint)`), so a table empty state and a Dock empty state are now
    /// the same shape at the same size. Before this they were not, and nothing in
    /// either file could tell you.
    #[gpui_kit::test]
    fn an_empty_states_description_is_body_beneath_its_title(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        // Three of the four empty kinds deliberately carry no description, so the
        // state has to be driven into one that has one. The status filter does it:
        // every fixture pod is Running, so switching it on hides all of them and
        // §4.13's "被筛掉了" row is the state on screen.
        view.update(cx, |view, cx| view.set_problems_only(true, cx));
        cx.run_until_parked();
        let title = cx
            .debug_bounds("empty-title")
            .expect("the empty state's title");
        let detail = cx
            .debug_bounds("empty-detail")
            .expect("the filtered-out state has a description");
        // A description wraps, so its height is a whole number of lines and the
        // token is read off the *line* rather than the box: `body` is 13/18 and
        // `caption` is 11/14, and 18 divides a body-measured box where 14 does not.
        // Comparing boxes directly would have been the wrong test — two lines of
        // `caption` are 28px and one line of `body` is 18px, so a naive
        // "shorter than the title" assertion passes on the wrong token.
        let detail_height = f32::from(detail.size.height);
        let body = f32::from(design::text::BODY_LINE_HEIGHT);
        let caption = f32::from(design::text::CAPTION_LINE_HEIGHT);
        assert!(
            detail_height >= body && (detail_height % body).abs() < 0.5,
            "§2.3: the description is `body` 13/18 and was `caption` 11/14 — a \\
             sentence is a paragraph, and §2.3 budgets 40ch for exactly this slot. \\
             Measured {detail_height}px, which is not a whole number of 18px lines."
        );
        assert!(
            (detail_height % caption).abs() >= 0.5,
            "and specifically not a whole number of 14px lines: {detail_height}px"
        );
        // The pair has to separate on both axes §2.3 names, or it is one level.
        // The title is one line and the description is at least one, so the
        // comparable number is the line height: 20 against 18.
        let title_line = f32::from(design::text::TITLE_LINE_HEIGHT);
        assert!(
            title_line - body >= 2.0,
            "§2.3: adjacent levels differ by at least 2px — title {title_line} against \\
             description {body}"
        );
        assert!(
            f32::from(title.size.height) <= title_line + 1.0,
            "the title is one line of `title` 15/20, which is what makes the \\
             comparison above about lines and not about boxes"
        );
    }

    /// `§4.13`: at most one action. A namespaced empty table used to render two
    /// buttons of equal weight — the state's own recovery and a `Switch Namespace`
    /// on top of it — and a reader with two buttons of equal weight and no reason
    /// to prefer either is a reader who hesitates.
    #[gpui_kit::test]
    fn an_empty_state_offers_at_most_one_action(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| Box::new(EmptySource) as Box<dyn ResourceSource>);
        let (_view, cx) = cx.add_window_view(|_, cx| PodsView::new(factory, None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        // The one action is the state's own recovery, and the namespace switcher
        // is in the title bar where the scope is chosen.
        assert!(
            cx.debug_bounds("empty-action-empty-refresh").is_some(),
            "a genuinely empty table still offers one way forward"
        );
        assert!(
            cx.debug_bounds("empty-action-empty-namespace").is_none(),
            "§4.13 allows zero or one action, and the scope lives in the title bar"
        );
    }

    /// `§4.14`: a wait that resolves fast shows *nothing*, and a wait that has
    /// outlasted 200ms shows a 32px spinner band — the same height as the summary
    /// strip it replaces, so the reader is not told something changed when only
    /// the waiting did.
    #[gpui_kit::test]
    fn the_loading_bands_are_the_height_the_spec_fixes(cx: &mut TestAppContext) {
        init_app(cx);
        let factory: SourceFactory = Box::new(|| {
            Box::new(StartupLoadingSource {
                reason: crate::shell::STARTUP_LOADING_REASON.to_owned(),
            }) as Box<dyn ResourceSource>
        });
        let (_view, cx) = cx.add_window_view(|_window, cx| PodsView::new(factory, None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let band = cx
            .debug_bounds("table-loading-status")
            .expect("§4.14's spinner band");
        let strip = cx.debug_bounds("table-summary").expect("the summary strip");
        assert_eq!(
            f32::from(band.size.height),
            f32::from(design::size::SUMMARY_STRIP),
            "the spinner band and the summary strip are the same 32px, so a load \\
             does not move the table"
        );
        assert_eq!(
            f32::from(band.size.height),
            f32::from(strip.size.height),
            "measured on both, because two tokens that happen to agree today are \\
             not the same token"
        );
    }

    /// `§4.15`: the error is a 32px band under the table body, with a 3px
    /// `danger` rule across its top edge and the rows still on screen. The old
    /// shape replaced the whole table with a 40px coloured disc and stole focus,
    /// so a watch that stopped threw away data the reader could still read.
    ///
    /// The band was above the body and this test asserted it: `bar.bottom()` was
    /// required to land at or above the first row's `top()`, "the bar is inside
    /// the table body and the rows begin below it". That assertion pinned the
    /// defect — the band above the body is the band that pushes the header and
    /// every row down 32px when a watch stops and back up when it starts — so it
    /// is inverted here rather than kept. The rows are above the bar now, and
    /// they hold their y.
    #[gpui_kit::test]
    fn the_error_is_a_32px_bar_and_the_rows_stay(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("resource-row-0").is_some());
        view.update(cx, |view, cx| {
            view.host.update(cx, |host, cx| {
                host.report_watch_error("watch ended".to_owned(), cx)
            });
        });
        cx.run_until_parked();
        let bar = cx
            .debug_bounds("table-error-bar")
            .expect("§4.15's inline error bar");
        let rule = cx
            .debug_bounds("table-error-bar-rule")
            .expect("the bar's 3px danger rule");
        assert_eq!(
            f32::from(bar.size.height),
            f32::from(INLINE_ERROR_HEIGHT),
            "§4.15 fixes the bar at 32px"
        );
        assert_eq!(f32::from(rule.size.height), 3.0, "and the rule at 3px");
        // §7: one left edge across everything in the region. The error bar, the
        // summary strip above it and the table's first column all put their text at
        // the table's own padding-x, and the bar used to be the exception at 11 —
        // 3px of rule plus an 8px pad — which is a difference no reader can name
        // and every reader can see.
        assert_eq!(
            rule.left(),
            bar.left(),
            "the danger rule spans the band's own width, so it starts at 0 and the \\
             content pads to the table's own padding-x inboard of it"
        );
        for (what, selector) in [
            ("the error bar's guidance", "table-error-guidance"),
            ("the summary strip's count", "table-summary-count"),
        ] {
            let left = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{what} is on screen"))
                .left()
                - bar.left();
            assert_eq!(
                f32::from(left),
                f32::from(design::space::LG),
                "{what} starts at the same x as every other element in the region \\
                 (§7), not at the rule's own edge"
            );
        }
        assert!(
            f32::from(bar.top())
                >= f32::from(cx.debug_bounds("resource-row-0").unwrap().bottom())
                    - f32::from(design::border::LINE),
            "the bar is under the rows, not over them: a band above the body moves \
             the header and every row down 32px when a watch stops"
        );
        assert!(
            cx.debug_bounds("resource-row-0").is_some(),
            "§4.15: a watch that stopped leaves the rows the reader can still read"
        );
        assert!(
            cx.debug_bounds("pods-error").is_none(),
            "the full-area error panel is gone"
        );
        // §4.14's stale rule — "有 stale 缓存时绝不上骨架屏" — is **not** asserted
        // here, and the reason is worth recording. The skeleton rows are
        // `ElementId::NamedInteger("table-skeleton-row", row)`, and
        // `debug_bounds` is keyed by the static selector a cell registers, so a
        // named-integer row has no selector to look up. Renaming the id to prove the
        // assertion bites leaves both the positive and the negative case green,
        // which makes it a test that cannot fail: worse than no test. What this test
        // does check is the half that *is* addressable — the rows are still laid out
        // and the 32px band is there — and the skeleton half is covered where the
        // skeleton is actually on screen, by
        // `the_loading_skeleton_follows_the_visible_columns_and_widths`.
    }

    /// A transient band must never displace the content a reader is looking at.
    ///
    /// The notice banner and the error bar are both transient and both used to
    /// sit *above* the table body, so the header and every row moved 28px or 32px
    /// when one appeared and moved back when it cleared — and the notice clears
    /// itself on an eight-second timer, so the window rearranged itself twice
    /// while somebody was reading rows. They now stack upward from the body,
    /// which is the direction the selection bar has always used, so nothing in
    /// the header stack is conditional at all.
    ///
    /// The invariant is the one the window's contract names: for every band that
    /// exists in more than one of these states, the y it starts at is the same y.
    /// Four states — neither band, the notice only, the error only, both — and one
    /// number per anchor. The anchors are the header and the first three body
    /// rows, because those are the four bands a reader's eye is actually resting
    /// on when a watch stops.
    #[gpui_kit::test]
    fn a_transient_band_never_moves_the_header_or_the_rows(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();

        let anchors = |cx: &mut gpui_kit::VisualTestContext| {
            [
                "pod-header-name",
                "resource-row-0",
                "resource-row-1",
                "resource-row-2",
            ]
            .map(|selector| {
                let bounds = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} is on screen"));
                (selector, bounds.origin.y)
            })
        };

        let resting = anchors(cx);
        assert!(
            cx.debug_bounds("table-error-bar").is_none()
                && cx.debug_bounds("table-notice").is_none(),
            "the fixture starts with neither band"
        );

        // The notice only. `Severity::Error` is the one severity that does not arm
        // the eight-second dismissal, so the band stays up for as long as the
        // test needs it without a timer the harness would have to wait out.
        view.update(cx, |view, cx| {
            view.notify(
                "Column widths could not be saved.".to_owned(),
                Severity::Error,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("table-notice").is_some(),
            "the notice is up"
        );
        assert_eq!(
            anchors(cx),
            resting,
            "a notice is a band under the table, so the header and the first three \
             rows hold their y"
        );

        // The error only.
        view.update(cx, |view, cx| {
            view.notice = None;
            view.host.update(cx, |host, cx| {
                host.report_watch_error("watch ended".to_owned(), cx)
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("table-notice").is_none(),
            "the notice is gone"
        );
        assert!(
            cx.debug_bounds("table-error-bar").is_some(),
            "the fault is up"
        );
        assert_eq!(
            anchors(cx),
            resting,
            "a watch that stopped never moves the header or the first three rows"
        );

        // Both at once. They stack upward from the body: the fault is about the
        // data on screen and sits against it, the message is about something the
        // reader just did and sits under it, and the selection bar is the
        // reader's own committed state and is last.
        view.update(cx, |view, cx| {
            view.notify(
                "Exec is available only for Pods.".to_owned(),
                Severity::Error,
                cx,
            )
        });
        cx.run_until_parked();
        let bar = cx.debug_bounds("table-error-bar").expect("the fault");
        let notice = cx.debug_bounds("table-notice").expect("the message");
        assert!(
            bar.bottom() <= notice.top(),
            "both bands up at once: the fault against the rows, the message under it"
        );
        assert_eq!(
            anchors(cx),
            resting,
            "two transient bands at once still move nothing above the body"
        );
    }

    /// The rail is not allowed to be a proportion, and this is the case that
    /// proves it.
    ///
    /// 110 healthy rows out of 10,010 is 1.1%: a true stacked bar spends 0.9px of
    /// a 96px track on it, the reader resolves nothing there, and the strip says
    /// *100% failed* about a cluster that is 1% failed. That is a silent
    /// breakage, which is the only kind of bug a rendering change can hide, so
    /// the two states that used to be indistinguishable — a mostly-broken table
    /// and an entirely broken one — are measured here rather than eyeballed.
    ///
    /// The tally is injected rather than listed: 10,010 rows through the host
    /// would test the source, and the thing that regressed is the 96 pixels.
    ///
    /// The buckets are a partition, so each injected tally is one the strip can
    /// actually produce: 110 healthy and 9,900 waiting is 10,010 rows. The old
    /// fixture asked for 110 healthy, 9,900 in error *and* 9,900 waiting — 19,910
    /// rows, with the same population counted under two names, which is the bug
    /// the partition was made to fix. The waiting bucket carries the error ink in
    /// the mostly-broken case, which is the state this fixture is about.
    #[gpui_kit::test]
    fn the_strip_rail_shows_a_one_percent_cluster_and_stays_calm_at_zero(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        // `refresh_summary` is memoised on the snapshot generation, so the view
        // keeps whatever the tally is between renders.
        let tally = |cx: &mut TestAppContext, healthy, pending| {
            view.update(cx, move |view, cx| {
                view.summary = RowSummary {
                    healthy,
                    warning: 0,
                    danger: 0,
                    pending,
                    pending_severity: (pending > 0).then_some(Severity::Error),
                };
                cx.notify();
            });
            cx.run_until_parked();
        };

        tally(cx, 110, 9_900);
        let rail = cx.debug_bounds("table-summary-rail").expect("the rail");
        let healthy = cx
            .debug_bounds("table-summary-rail-healthy")
            .expect("1% of the cluster still has to be on screen");
        let danger = cx
            .debug_bounds("table-summary-rail-pending")
            .expect("the failures have their own segment");
        let track = f32::from(SUMMARY_MICROBAR_WIDTH);
        let gap = f32::from(design::space::XXS);
        assert!(
            f32::from(healthy.size.width) >= RAIL_MIN_SEGMENT,
            "110 of 10,010 rows is {}px of a {track}px bar — below the floor the \\
             bar says the cluster is 100% broken",
            0.011 * track
        );
        assert!(
            f32::from(danger.left() - healthy.right()) >= gap,
            "the 1% is separated from the 99% and not painted inside it"
        );
        assert!(
            f32::from(danger.size.width) <= RAIL_MAX_SHARE * track,
            "a nearly-all-failed cluster may not fill the track: a full bar of one \\
             ink is the one thing this glyph must never say"
        );
        assert!(
            f32::from(danger.right() - rail.left()) < track,
            "and the tail of the track has to stay visible for the same reason"
        );

        // 9,901 healthy of 10,010 is the mirror image and has to be just as
        // legible, or the strip only works when the cluster is on fire.
        tally(cx, 9_901, 109);
        let healthy = cx
            .debug_bounds("table-summary-rail-healthy")
            .expect("the healthy share is on screen");
        let danger = cx
            .debug_bounds("table-summary-rail-pending")
            .expect("and so is the 1% that is not");
        assert!(
            f32::from(healthy.size.width) > f32::from(danger.size.width),
            "mostly-fine and mostly-broken have to look different, at 99% and at \\
             1% as much as in the middle"
        );
        assert!(
            f32::from(healthy.size.width) <= RAIL_MAX_SHARE * track,
            "and the healthy share is capped exactly like the failing one: health \\
             gets no more of the bar for being the good news"
        );

        // Nothing wrong: no rail at all, so a healthy cluster is not told about a
        // fault that does not exist. `§4.14`'s rule for the loading band is the
        // same rule — a band that appears to say something is a band that lies.
        tally(cx, 0, 0);
        assert!(
            cx.debug_bounds("table-summary-rail").is_none(),
            "a table with no rows has no shape to draw and no fault to show"
        );
        assert!(
            cx.debug_bounds("table-summary-rail-danger").is_none(),
            "and nothing in the band is wearing the danger channel"
        );
        assert_eq!(
            f32::from(
                cx.debug_bounds("table-summary")
                    .expect("the strip is always there")
                    .size
                    .height
            ),
            f32::from(design::size::SUMMARY_STRIP),
            "the calm case is the same 32px, not a collapsed one"
        );
    }

    /// `UI-SPEC` §4.4: the table is the product's signature surface and the summary
    /// strip is one of its three signature elements. It has to be the same 32px as
    /// the bands around it, and it has to be there in both appearances, because a
    /// strip that only renders when the table has data is a strip that appears and
    /// disappears as the reader filters.
    #[gpui_kit::test]
    fn the_summary_strip_is_always_there_and_32px(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let strip = cx.debug_bounds("table-summary").expect("the summary strip");
        assert_eq!(
            f32::from(strip.size.height),
            f32::from(design::size::SUMMARY_STRIP),
            "§4.4 fixes the strip at 32px"
        );
        // It is `Role::Status`, so a change in the counts is announced and not
        // only drawn.
        assert_eq!(
            view.read_with(cx, |view, _| view.selection_count()),
            0,
            "the fixture starts with nothing selected"
        );
    }

    /// `UI-SPEC` §10.2 fixes the Pod row exactly, and the numbers in it are
    /// arithmetic: 252 + 104 + 148 + 56 + 64 + 48 = 672, plus six 16px gaps and
    /// 32px of padding, is the 800px the centre column is budgeted. A width that
    /// drifts is not a cosmetic slip — it is either a horizontal scrollbar on the
    /// screen the reader spends their day in, or a `Node` column too narrow to
    /// name a machine.
    ///
    /// This is the assertion that keeps the column spec from quietly rotting: the
    /// widths live in a declaration, and a declaration is only as good as the
    /// thing that checks it.
    #[test]
    fn the_pod_columns_are_the_ones_the_spec_fixed() {
        let columns = pod_columns();
        let shown: Vec<(&str, f32)> = columns
            .iter()
            .filter(|column| !default_hidden_columns("Pod").contains(&column.column.id.as_str()))
            .map(|column| (column.title, column.default_width()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Name", 252.0),
                ("Namespace", 104.0),
                ("Status", 148.0),
                ("Ready", 56.0),
                ("Restarts", 64.0),
                ("Age", 48.0),
                ("Node", 170.0),
            ],
            "§10.2's Pod row, in order and at the designed widths"
        );
        // `Node` is the one column that takes the room the others leave, and 170
        // is the floor below which a node name cannot be read.
        let node = columns
            .iter()
            .find(|column| column.column.id == "node")
            .expect("the node column");
        assert!(node.is_flex(), "Node is flex, not a fixed 320px");
        // `§10.2`: "Image 不在默认列里. 它又长（190px 起）又很少是扫描目标，会把
        // Node 挤到 150px 以下." The two hidden columns are therefore the spec's
        // decision and not a preference.
        assert_eq!(
            default_hidden_columns("Pod"),
            ["image", "ip"],
            "Image and IP are one click away, and both push Node under its floor"
        );
        // Every numeric column sits at or above the width floor, so a resize drag
        // can never clip a header.
        for column in &columns {
            if column.is_right_aligned() {
                assert!(
                    column.default_width() >= COLUMN_MIN_WIDTH,
                    "{} is narrower than the smallest draggable column",
                    column.title
                );
            }
        }
        // The identifier is the only sticky, middle-truncated, sans-13/500 column.
        let name = columns.first().expect("the name column is first");
        assert_eq!(name.class, ColumnClass::Identifier);
        assert_eq!(name.ink, CellInk::Primary);
        // `D25`: the table is sans except the long-text class, and the long-text
        // class is the only place mono survives.
        let mono: Vec<&str> = columns
            .iter()
            .filter(|column| column.is_mono())
            .map(|column| column.title)
            .collect();
        assert_eq!(
            mono,
            ["Image", "IP"],
            "mono survives only where the value is machine-shaped"
        );
    }

    /// A header that cannot be read names nothing.
    ///
    /// Three of §10.2's widths could not be read at *any* width: `Ready` 56,
    /// `Restarts` 64 and `Age` 48 are the width of `1/2`, `3` and `12d`, and
    /// after the cell's 32px of padding and the 20px the sort control takes they
    /// left 4px, 12px and nothing at all for `READY`, `RESTARTS` and `AGE`. So a
    /// 941px window showed `RESTARTS` as `REST…`, `Ready` and `Age` as a bare
    /// `…`, and a reader could not tell three columns from each other or from
    /// the ellipsis. The widths are not the bug and the spec that fixed them is
    /// not wrong about the data; what was missing is a floor for the *header*.
    ///
    /// This is the invariant that floor and [`fit_columns`] exist to keep, and
    /// it is checked over every kind and every width a window can be rather than
    /// at one, because a silent breakage that only shows up at one width is the
    /// kind nobody sees before a reader does.
    #[test]
    fn every_column_the_table_draws_can_draw_its_own_header() {
        let kinds = [
            "Pod",
            "Deployment",
            "StatefulSet",
            "ReplicaSet",
            "DaemonSet",
            "Service",
            "Node",
            "ConfigMap",
            "Secret",
            "Job",
            "CronJob",
            "Ingress",
            "Namespace",
        ];
        // `WINDOW_MIN` is 960 and the narrowest centre the shell can offer is a
        // 48px icon rail with the Inspector floating over it, so this is the
        // whole range from "narrower than the window may be" to a wide display.
        let mut narrower: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for budget in (240..=2_560).step_by(8) {
            let budget = budget as f32;
            for kind in kinds {
                let columns = columns_for(kind, true);
                let hidden: HashSet<String> = default_hidden_columns(kind)
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect();
                let wanted = visible_indices(&columns, &hidden);
                // The view protects these plus the sorted column; protecting less
                // here only asks the fit to give up more, so it is the harder test.
                let protected = [
                    columns
                        .iter()
                        .position(|column| column.class == ColumnClass::Identifier),
                    columns
                        .iter()
                        .position(|column| column.column.id == "status"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                let kept = fit_columns(&columns, &wanted, &protected, budget);
                // `column_default_width` is what the view hands the component, so
                // this is the width the header cell is laid out in.
                let total: f32 = kept
                    .iter()
                    .map(|&index| column_default_width(&columns[index]))
                    .sum();
                assert!(
                    total <= budget || kept.iter().all(|index| protected.contains(index)),
                    "{kind} at {budget}px draws {total}px with {kept:?} and still has a column to give up"
                );
                for &index in &kept {
                    let column = &columns[index];
                    assert!(
                        column_default_width(column) >= column.header_min_width(),
                        "{} is {}px, which cannot draw {}",
                        column.title,
                        column_default_width(column),
                        column.title
                    );
                }
                assert_eq!(
                    kept.first(),
                    wanted.first(),
                    "{kind} at {budget}px dropped the identifier"
                );
                // Opening the window gives the columns back. A column given up for
                // width is a layout decision, and a reader who widens the window
                // has to get their row back.
                if let Some(was) = narrower.get(kind) {
                    assert!(
                        was.iter().all(|index| kept.contains(index)),
                        "{kind} at {budget}px lost a column it had at a narrower width"
                    );
                }
                narrower.insert(kind, kept);
            }
        }
    }

    /// `UI-SPEC` §10.3 fixes twelve kinds, and the count is the point: a kind with
    /// no columns falls through to Name/Namespace/Age, which for a DaemonSet is
    /// three columns that cannot tell a reader anything about a DaemonSet.
    #[test]
    fn every_built_in_kind_has_the_columns_the_spec_fixed() {
        for (kind, expected) in [
            (
                "Pod",
                vec![
                    "Name",
                    "Namespace",
                    "Status",
                    "Ready",
                    "Restarts",
                    "Age",
                    "Node",
                    "Image",
                    "IP",
                ],
            ),
            (
                "Deployment",
                vec![
                    "Name",
                    "Namespace",
                    "Ready",
                    "Up-to-Date",
                    "Available",
                    "Age",
                ],
            ),
            ("StatefulSet", vec!["Name", "Namespace", "Ready", "Age"]),
            (
                "DaemonSet",
                vec![
                    "Name",
                    "Namespace",
                    "Desired",
                    "Current",
                    "Ready",
                    "Up-to-Date",
                    "Available",
                    "Age",
                ],
            ),
            (
                "ReplicaSet",
                vec!["Name", "Namespace", "Desired", "Current", "Ready", "Age"],
            ),
            (
                "Job",
                vec![
                    "Name",
                    "Namespace",
                    "Status",
                    "Completions",
                    "Duration",
                    "Age",
                ],
            ),
            (
                "CronJob",
                vec![
                    "Name",
                    "Namespace",
                    "Schedule",
                    "Suspend",
                    "Active",
                    "Last schedule",
                    "Age",
                ],
            ),
            (
                "Node",
                vec![
                    "Name",
                    "Status",
                    "Roles",
                    "Version",
                    "Internal IP",
                    "OS Image",
                    "Age",
                ],
            ),
            (
                "Service",
                vec![
                    "Name",
                    "Namespace",
                    "Type",
                    "Cluster IP",
                    "External IP",
                    "Ports",
                    "Age",
                ],
            ),
            (
                "Ingress",
                vec![
                    "Name",
                    "Namespace",
                    "Class",
                    "Hosts",
                    "Address",
                    "Ports",
                    "Age",
                ],
            ),
            ("ConfigMap", vec!["Name", "Namespace", "Data", "Age"]),
            ("Namespace", vec!["Name", "Status", "Workloads", "Age"]),
        ] {
            let columns = columns_for(kind, true);
            let titles: Vec<&str> = columns.iter().map(|column| column.title).collect();
            assert_eq!(titles, expected, "{kind} has the columns §10.3 fixed");
        }
        assert_eq!(
            design::KIND_ICON_COUNT,
            12,
            "the kind icons are twelve too, so the twelve kinds are one set"
        );
    }

    /// `UI-SPEC` §10.3: "Event 不做表格". An event is a timeline, and the table
    /// has to *say* so rather than fall through to a three-column fallback: a
    /// reader who opens Events and finds "No columns for this kind" is being told
    /// the cluster has no events, which is a lie about a cluster full of them.
    #[test]
    fn events_are_declined_rather_than_half_supported() {
        assert!(declines_table("Event"));
        assert!(declines_table("events"));
        assert!(
            !is_known_kind("Event"),
            "an Event has no column layout, and `declines_table` is how the empty \\
             state knows that is a decision rather than a gap"
        );
        assert_eq!(
            empty_state_kind(&TableStatus::Streaming, "", "Event", None, false),
            EmptyState::NoTimeline,
            "an Event view explains itself instead of claiming an empty scope"
        );
    }

    /// `UI-SPEC` §0 铁律三 inverts the usual reading: a healthy resource is grey
    /// and only `Pending` / `Failed` / `Error` are coloured. It is the one rule in
    /// this file whose violation is invisible in a screenshot of healthy rows and
    /// ruinous in one of unhealthy ones, so it is asserted on the values rather
    /// than on the pixels.
    #[gpui_kit::test]
    fn healthy_is_grey_and_only_a_problem_takes_a_colour(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            // The plain row: no selection wash, so the resolver's own channel and
            // the quiet healthy grey come straight through (the wash-solve is a
            // no-op on the surface it already solved against).
            let row = design::role::surface_content(cx);
            assert_eq!(
                status_dot_ink(Severity::Success, row, cx),
                design::role::fg_tertiary(cx),
                "a healthy row's dot is the quietest ink on screen"
            );
            assert_eq!(
                status_word_ink(Severity::Success, row, cx),
                design::role::fg_secondary(cx),
                "a healthy row's word is one step above its dot"
            );
            // The mark and its word are the SAME channel, one role apiece — the
            // promise `Severity`'s mark/word split makes, and the thing the old
            // two-resolver path could drift away from. Same hue and saturation;
            // only the lightness differs, because the 6px mark and the 13px word
            // are held to different floors (graphic 3:1, text 4.5:1).
            for severity in [Severity::Warning, Severity::Error] {
                let dot = status_dot_ink(severity, row, cx);
                let word = status_word_ink(severity, row, cx);
                assert!(
                    (dot.h - word.h).abs() < 1e-4 && (dot.s - word.s).abs() < 1e-4,
                    "a status cell's dot and word are one channel: dot {dot:?}, word {word:?}"
                );
            }
            // And no status ink is the accent, which is spent on the row
            // selection and the one `primary` button. `§0` 铁律二 caps it at two
            // uses per screen and this table is on most of them.
            for ink in [
                status_dot_ink(Severity::Success, row, cx),
                status_word_ink(Severity::Warning, row, cx),
                status_word_ink(Severity::Error, row, cx),
            ] {
                assert_ne!(ink, design::role::accent(cx));
            }
        });
    }

    /// `UI-SPEC` §0 铁律三 grades `Pending` by age, and this is the function the
    /// whole rule runs through. A cluster mid-rollout has thousands of `Pending`
    /// pods; if they all read the same, the reader has nothing to look at and the
    /// few hundred that are actually stuck are lost inside them.
    #[test]
    fn a_pending_pod_is_graded_by_how_long_it_has_been_waiting() {
        let wait = |seconds: u64| Some(Duration::from_secs(seconds));
        assert_eq!(
            status_severity("Pending", wait(2)),
            Severity::Success,
            "a pod scheduled two seconds ago is doing what a scheduled pod does"
        );
        assert_eq!(
            status_severity("Pending", wait(30)),
            Severity::Success,
            "thirty seconds is still the grace period, not a problem"
        );
        assert_eq!(
            status_severity("Pending", wait(31)),
            Severity::Warning,
            "past thirty seconds a Pending pod is worth a second look"
        );
        assert_eq!(
            status_severity("Pending", wait(301)),
            Severity::Error,
            "past five minutes a Pending pod is stuck and has to be findable"
        );
        assert_eq!(
            status_severity("Pending", None),
            Severity::Success,
            "a pod with no timestamp has not told us it is stuck"
        );
        // The grade is a function of age alone, so a status that is *not* a phase
        // keeps `design`'s own mapping and cannot be drifted by this rule.
        assert_eq!(status_severity("Running", wait(9_999)), Severity::Success);
        assert_eq!(status_severity("Failed", wait(1)), Severity::Error);
        assert_eq!(status_severity("Unknown", wait(9_999)), Severity::Neutral);
    }

    /// `PROMPT` §2.4: a native list jumps to a name as you type. A table of
    /// 10,000 resources that cannot is a table a reader has to scroll, and the
    /// whole feature is a prefix buffer and a `starts_with`.
    #[gpui_kit::test]
    fn typing_in_the_table_jumps_to_a_row_by_name(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        let selected = |cx: &mut gpui_kit::VisualTestContext| {
            view.read_with(cx, |view, cx| {
                view.selected_name(cx).map(|name| name.to_string())
            })
        };
        // The fixture is `pod-0`, `pod-1`, `pod-2` and nothing is selected, so
        // the first keystroke has to search from the top rather than step from a
        // cursor that does not exist yet.
        cx.simulate_keystrokes("2");
        cx.run_until_parked();
        assert_eq!(selected(cx).as_deref(), Some("pod-2"));
        // A second character extends the prefix, so `20` matches nothing and the
        // selection stays where it is rather than jumping to a second row.
        cx.simulate_keystrokes("0");
        cx.run_until_parked();
        assert_eq!(selected(cx).as_deref(), Some("pod-2"));
        // The prefix expires, which is what makes `c`, pause, `c` spell two
        // separate searches rather than one impossible one.
        cx.executor()
            .advance_clock(TYPEAHEAD_RESET + Duration::from_millis(1));
        cx.run_until_parked();
        cx.simulate_keystrokes("1");
        cx.run_until_parked();
        assert_eq!(selected(cx).as_deref(), Some("pod-1"));
        // A modified keystroke is a shortcut, not type-ahead.
        cx.simulate_keystrokes("ctrl-down");
        cx.run_until_parked();
        assert_eq!(selected(cx).as_deref(), Some("pod-1"));
    }

    /// `PROMPT` §2.1 #10: a click produces no focus ring. The two states differ by
    /// *origin* rather than by value, so no colour or bounds assertion can catch a
    /// regression here — only the flag can, and this is the flag.
    #[gpui_kit::test]
    fn a_click_leaves_no_keyboard_cursor(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        assert!(
            !view.read_with(cx, |view, _| view.keyboard_focus.get()),
            "focusing the table from a test is not the keyboard arriving"
        );
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.keyboard_focus.get()),
            "a key press records the keyboard's origin"
        );
        let clicked = cx
            .debug_bounds("resource-row-2")
            .expect("the third row")
            .center();
        cx.simulate_click(clicked, gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |view, _| view.keyboard_focus.get()),
            "a click clears it again, so the rail that appears is the selection's \\
             and not a focus ring"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.selection_count()),
            1,
            "the click still selected the row it landed on"
        );
    }

    // The status mark is the only place the verdict is drawn, so it has to carry
    // an accessible name. A screen reader used to read "Status: Pending" and
    // never learned that Pending is a warning, or that one Pending pod is stuck
    // and nine thousand are not.
    #[gpui_kit::test]
    fn the_status_dot_leads_the_status_cell_at_the_dot_size(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (_view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        let status_index = pod_columns()
            .iter()
            .position(|column| column.column.id == "status")
            .expect("status column");
        assert_eq!(status_index, 2, "the status dot leads the status cell");
        let dot = cx
            .debug_bounds("resource-row-health-0")
            .expect("the status dot is its own element");
        // `UI-SPEC` §4.4: "6px dot (r-full) + 6px gap + 文字 13/500", and 不用图标.
        // A 16px glyph per severity put four shapes into every row of a
        // 10,000-row table to carry a distinction the dot's colour already makes.
        assert_eq!(
            f32::from(dot.size.width),
            f32::from(design::size::STATUS_DOT),
            "the status mark is a 6px dot, not an icon"
        );
        assert_eq!(
            f32::from(dot.size.width),
            f32::from(dot.size.height),
            "the status mark is round"
        );
        // A row that answered gets no confidence marker, so the two channels do
        // not both paint on every row.
        assert!(cx.debug_bounds("resource-row-confidence-0").is_none());
    }

    #[gpui_kit::test]
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
            .debug_bounds("MENU_ITEM-Open details")
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

    #[gpui_kit::test]
    fn row_menu_hides_service_account_without_a_pod_handler(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(cx.debug_bounds("MENU_ITEM-Open details").is_some());
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

    #[gpui_kit::test]
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
        assert!(cx.debug_bounds("MENU_ITEM-Open details").is_some());
        assert!(cx.debug_bounds("MENU_ITEM-Open Service Account").is_none());
    }

    /// Serves one object, so a test can put a Service in a Services view and a Pod in a
    /// Pods view without a source per kind.
    struct SingleObjectSource(Arc<DynamicObject>);

    impl ResourceSource for SingleObjectSource {
        fn subscribe(&mut self, events: UnboundedSender<SourceEvent>) -> Box<dyn Subscription> {
            let _ = events.send(SourceEvent::Init);
            let _ = events.send(SourceEvent::Store(StoreEvent {
                op: StoreOp::Apply,
                obj: Arc::clone(&self.0),
            }));
            let _ = events.send(SourceEvent::InitDone);
            Box::new(NoopSubscription { _events: events })
        }
    }

    /// A Service with a `spec.ports` list and a Pod with a `containerPort` list, which are
    /// two different documents naming the same number.
    fn service_object() -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {
                    "name": "api-svc",
                    "namespace": "prod",
                    "uid": "svc-1",
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": {
                    "type": "ClusterIP",
                    "ports": [{
                        "name": "http",
                        "port": 80,
                        "targetPort": 8080,
                        "protocol": "TCP"
                    }],
                },
            }))
            .expect("test service"),
        )
    }

    fn pod_object_with_a_container_port() -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web-0",
                    "namespace": "prod",
                    "uid": "pod-1",
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": {
                    "nodeName": "node-01",
                    "containers": [{
                        "name": "app",
                        "image": "app:1",
                        "ports": [{ "name": "http", "containerPort": 8080, "protocol": "TCP" }],
                    }],
                },
                "status": { "phase": "Running", "podIP": "10.244.0.1" },
            }))
            .expect("test pod"),
        )
    }

    fn single_object_factory(object: Arc<DynamicObject>) -> SourceFactory {
        Box::new(move || {
            Box::new(SingleObjectSource(Arc::clone(&object))) as Box<dyn ResourceSource>
        })
    }

    /// A row in a kind that declares no port of its own and cannot be forwarded.
    fn deployment_object() -> Arc<DynamicObject> {
        Arc::new(
            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {
                    "name": "web",
                    "namespace": "prod",
                    "uid": "deploy-1",
                    "creationTimestamp": "2026-09-22T00:00:00Z",
                },
                "spec": { "replicas": 3 },
            }))
            .expect("test deployment"),
        )
    }

    // The row a user right-clicked is the row that decides the ports: a Service resolves
    // its own `spec.ports`, a Pod its `containerPort` list, and a kind that can never be
    // forwarded offers no entry at all rather than one that refuses.
    #[gpui_kit::test]
    fn port_forward_resolves_the_row_it_started_from(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                single_object_factory(service_object()),
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Service", "Services", true),
                cx,
            )
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |view, _| {
            let seen = Rc::clone(&seen);
            view.on_forward_requested(move |target, _, _| seen.borrow_mut().push(target));
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_port_forward(window, cx));
        });
        assert_eq!(
            seen.borrow().as_slice(),
            [PortForwardTarget {
                namespace: Some("prod".to_owned()),
                name: "api-svc".into(),
                // The Service's own port name labels the choice, and its `targetPort` is
                // the number the forward has to reach.
                ports: vec![crate::panels::forwards::ContainerPort {
                    port: 8080,
                    container: Some("http".into()),
                }],
            }]
        );

        // A Pod keeps the container port it declares, under the container's own name.
        let (pods, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                single_object_factory(pod_object_with_a_container_port()),
                None,
                None::<InspectorBinding>,
                ResourceSpec::pods(),
                cx,
            )
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        pods.update(cx, |view, _| {
            let seen = Rc::clone(&seen);
            view.on_forward_requested(move |target, _, _| seen.borrow_mut().push(target));
        });
        cx.run_until_parked();
        focus_table(cx, &pods);
        cx.simulate_keystrokes("down");
        cx.update(|window, cx| {
            pods.update(cx, |view, cx| view.request_port_forward(window, cx));
        });
        assert_eq!(
            seen.borrow().as_slice(),
            [PortForwardTarget {
                namespace: Some("prod".to_owned()),
                name: "web-0".into(),
                ports: vec![crate::panels::forwards::ContainerPort {
                    port: 8080,
                    container: Some("app".into()),
                }],
            }]
        );

        // A Deployment cannot be forwarded at all, so the action is not on its row.
        let (deployments, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                single_object_factory(deployment_object()),
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Deployment", "Deployments", true),
                cx,
            )
        });
        deployments.update(cx, |view, _| {
            view.on_forward_requested(|_, _, _| {});
        });
        cx.run_until_parked();
        focus_table(cx, &deployments);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("MENU_ITEM-Start Port Forward").is_none(),
            "a kind that can never be forwarded must not offer the action"
        );
    }

    // A Service whose ports resolve to nothing has no address a forward could reach, and
    // saying so by name is the difference between a fixable answer and a shrug.
    #[gpui_kit::test]
    fn a_service_with_no_resolvable_port_says_which_service(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PodsView::for_resource(
                single_object_factory(test_service(0)),
                None,
                None::<InspectorBinding>,
                ResourceSpec::new("Service", "Services", true),
                cx,
            )
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |view, _| {
            let seen = Rc::clone(&seen);
            view.on_forward_requested(move |target, _, _| seen.borrow_mut().push(target));
        });
        cx.run_until_parked();
        focus_table(cx, &view);
        cx.simulate_keystrokes("down");
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request_port_forward(window, cx));
        });
        assert!(
            seen.borrow().is_empty(),
            "a Service with no target port must not open a dialog that cannot start"
        );
        let notice = view.read_with(cx, |view, _| {
            view.notice.as_ref().map(|n| n.message.to_string())
        });
        let notice = notice.expect("the refusal is reported");
        assert!(
            notice.contains("svc-0"),
            "the refusal names the Service, got {notice:?}"
        );

        focus_table(cx, &view);
        cx.simulate_keystrokes("f10");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("MENU_ITEM-Start Port Forward").is_none(),
            "an entry that can only refuse is not an affordance"
        );
    }

    // Shift+F10 and the Menu key are the keyboard context-menu keys.
    #[gpui_kit::test]
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

    // Rows follow the data font size, so Page Up and Page Down stay in view.

    // A cell is judged by the width its text really takes, so a wide glyph run
    // gets a tooltip and a short value does not.
    #[gpui_kit::test]
    fn cell_overflow_follows_the_shaped_width(cx: &mut TestAppContext) {
        init_app(cx);
        let typography = cx.update(|cx| DataTypography::from_theme_settings(cx));
        let columns = crate::table_view::pod_columns();
        let name = columns
            .iter()
            .find(|column| column.column.id == "name")
            .expect("name column");
        let width = px(name.default_width());
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
    #[gpui_kit::test]
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

    /// §2.3's middle ellipsis, over the longest name a real cluster produces.
    ///
    /// `nightly-reindex-warehouse-partition-cleanup-29845620-dpmz9` is 58
    /// characters and lives in this repo's own kind cluster. It is the case the
    /// rule exists for: the head says which job this is and the tail says which
    /// run, so a tail ellipsis deletes the half that identifies the pod and
    /// leaves fifty rows reading `nightly-reindex-wareh…`.
    ///
    /// The budget is **shaped**, not counted. The split is asserted against the
    /// width the candidate actually takes in the shipping face, which is the only
    /// way to catch the failure this replaced: a name cut to `available /
    /// average` is drawn at whatever its own glyphs add up to, and Inter's
    /// tabular digits and hyphens are not what the average is made of. One glyph
    /// too wide and the sticky column overflows its own 1px divider.
    ///
    /// The three states are asserted separately because each is a different bug:
    /// a name that fits has to be drawn **whole** (the fixed 16-and-12 rule
    /// shortened every name over 29 characters no matter how wide the reader had
    /// dragged the column, so widening `Name` revealed 148px of nothing), a name
    /// that does not has to keep **both** ends, and a column too narrow for a
    /// head, a tail and an ellipsis has to hand the cell back to the shared tail
    /// ellipsis rather than invent a shorter tail.
    #[gpui_kit::test]
    fn a_long_name_is_shortened_in_the_middle_with_a_shaped_budget(cx: &mut TestAppContext) {
        init_app(cx);
        let cx = cx.add_empty_window();
        const NAME: &str = "nightly-reindex-warehouse-partition-cleanup-29845620-dpmz9";
        let typography = cx.update(|_, cx| DataTypography::from_theme_settings(cx));
        let char_width = f32::from(typography.size) * CHAR_WIDTH_RATIO;

        cx.update(|window, _cx| {
            let name_column = pod_columns()
                .into_iter()
                .find(|column| column.column.id == "name")
                .expect("the name column");
            let available = cell_text_width(Some(&name_column), px(name_column.default_width()));

            // A name that fits is not shortened at all, so a reader who drags the
            // column wider gets the whole name and not a fixed-length stub.
            assert_eq!(
                middle_ellipsis(window, "web-0", available, &typography, char_width),
                None,
                "a name that fits the cell is drawn whole"
            );

            let shortened = middle_ellipsis(window, NAME, available, &typography, char_width)
                .expect("a 58-character name does not fit §10.2's 252px column");
            let (head, tail) = shortened
                .split_once('\u{2026}')
                .unwrap_or_else(|| panic!("{shortened:?} is shortened in the middle"));
            assert!(
                NAME.starts_with(head) && !head.is_empty(),
                "the head of {shortened:?} is a prefix of the name, so the workload is \
                 still named"
            );
            assert!(
                NAME.ends_with(tail) && !tail.is_empty(),
                "the tail of {shortened:?} is a suffix of the name, so the run is still \
                 identified — a tail ellipsis would have deleted exactly this"
            );
            assert!(
                shortened.chars().count() < NAME.chars().count(),
                "and it is shorter than the name it stands for"
            );
            assert!(
                shaped_line_width(window, &shortened, &typography) <= available,
                "the shortened name is {}px in a {available}px cell — the split is \
                 shaped, so a counted budget that overflows the sticky column's own \
                 divider cannot pass here",
                shaped_line_width(window, &shortened, &typography)
            );
            // The same margin the cell is measured with, so a name that passes here
            // is a name the box can hold. Without it the loop returns the longest
            // head that fits — by construction within a pixel of the budget, which
            // is inside gpui's per-glyph advance snapping — and the last glyph loses
            // the right half of itself to `overflow_hidden`. It shipped as
            // `…-dpmz9` painted as `…-dp` with a sliced `9`, on the one column a
            // reader scans down.
            let slack = NAME.chars().count() as f32 * SHAPED_WIDTH_SLACK_PER_GLYPH;
            assert!(
                shaped_line_width(window, &shortened, &typography) + slack <= available,
                "the shortened name plus the per-glyph snapping allowance must still \
                 fit: {}px + {slack}px in a {available}px cell",
                shaped_line_width(window, &shortened, &typography)
            );
            // The tail is held at the hash, and the head gives ground first: the
            // head is what has room to lose.
            assert!(
                head.chars().count() >= 1 && tail.chars().count() == MIDDLE_ELLIPSIS_MIN_TAIL,
                "the tail keeps {MIDDLE_ELLIPSIS_MIN_TAIL} characters and the head takes \
                 what is left, rather than splitting the budget evenly: {shortened:?}"
            );

            // A column narrower than head-plus-six has stopped being a column, and
            // the shared cell's own tail ellipsis is the honest fallback: it agrees
            // with every other column about what a clipped value looks like.
            assert_eq!(
                middle_ellipsis(window, NAME, 40.0, &typography, char_width),
                None,
                "too narrow for a head, a tail and an ellipsis: the cell clips it"
            );

            // §4.4's first column carries a 1px right divider inside its 252px, so
            // the text area it measures against is 219 and not 220. Charging the
            // rule is what keeps the shaped split and the laid-out box the same
            // number; without it the split is a pixel wider than the cell that has
            // to hold it.
            let name_only = cell_text_width(Some(&name_column), px(name_column.default_width()));
            assert_eq!(
                name_only,
                name_column.default_width() - 2.0 * f32::from(design::space::LG) - 1.0,
                "the identifier's text area is its width less both 16px pads and the \
                 1px divider §4.4 puts on its right edge"
            );
            let namespace_column = pod_columns()
                .into_iter()
                .find(|column| column.column.id == "namespace")
                .expect("the namespace column");
            assert_eq!(
                cell_text_width(
                    Some(&namespace_column),
                    px(namespace_column.default_width())
                ),
                namespace_column.default_width() - 2.0 * f32::from(design::space::LG),
                "and a column with no divider is charged only its own padding"
            );
        });
    }

    // The pending badge shows the deadline, so a slow request is not silent.

    /// Every column of a row has to read as one row, and the first thing that
    /// breaks that is text sitting at different heights across the columns.
    ///
    /// `gpui_kit::Table` wraps each cell in a `div` that fills the row, so a
    /// cell's own bounds are centered whatever the cell does with its line. The
    /// line is a child of it, and the child is the only thing that can be wrong:
    /// a cell that centres nothing hugs the top of the row and still measures as
    /// centered. So the assertions here read the line, not the cell.
    ///
    /// The defect that shaped them: the data cell was a **block**, and
    /// `justify_center` is `justify-content`, which taffy drops on anything but a
    /// flex or grid container. Its single line therefore sat at the top of the
    /// 32px row — 4.5px above the cap and 18px below it — while the status cell
    /// beside it was an `h_flex` with `items_center` and was properly centered.
    /// Measured on the shipping 13px face: the data baseline 13.5px above the
    /// status word's, on every row, in every column but one.
    ///
    /// The status cell packs a glyph beside a shorter line, so its own box is not
    /// its line either; the glyph is the observable half of that centering.
    ///
    /// The product typography is installed here rather than left to the theme
    /// default, because a 15px test font fills a 28px row on its own and hides
    /// the very slack this is about.
    #[gpui_kit::test]
    fn every_cell_shares_the_vertical_center_of_its_row(cx: &mut TestAppContext) {
        assert_cell_centers_match_the_row(cx, None);
    }

    /// The row grows with the configured data line, so the cells have to keep
    /// their center when the reader raises the size rather than only at 12px.
    #[gpui_kit::test]
    fn a_raised_data_font_keeps_every_cell_on_the_row_center(cx: &mut TestAppContext) {
        // 24px of data font is a 36px line box, which clears the 32px default
        // row and so actually moves it. 20px is a 30px line, which the default
        // row now absorbs — the fixture would have proved nothing.
        assert_cell_centers_match_the_row(cx, Some(24));
    }

    fn assert_cell_centers_match_the_row(cx: &mut TestAppContext, data_font_size: Option<u8>) {
        init_app(cx);
        cx.update(crate::settings::install_product_typography_defaults);
        if let Some(size) = data_font_size {
            cx.update(|cx| {
                crate::settings::SettingsStore::update(cx, |store, cx| {
                    store
                        .set_user_settings(&format!(r#"{{ "buffer_font_size": {size} }}"#), cx)
                        .expect("the data size applies");
                });
            });
        }
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1400.0), px(800.0)));
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        let typography = cx.update(|_, cx| DataTypography::from_theme_settings(cx));
        // The table renders at the table's own rhythm, which is the shared one.
        // It no longer reserves a row divider: `UI-SPEC` §4.4 gives a table row
        // height and hover and nothing else.
        let row_height = typography.table_row_height();
        if data_font_size.is_some() {
            assert!(
                row_height > design::size::ROW,
                "the fixture has to actually raise the row, or this test proves nothing"
            );
        } else {
            assert_eq!(row_height, design::size::ROW, "the shipping row is 32px");
            assert!(
                typography.line_height < row_height,
                "18px of line in a 32px row is the slack the defect showed in"
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
            "the row is on the table's rhythm"
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
            // Centring a line in a box too short for it still centres it, so
            // centring alone never caught the crop: the cell also has to hold
            // the whole configured line.
            assert!(
                cell.size.height >= typography.line_height,
                "the data cell in column {position} is {}px for a {}px line",
                f32::from(cell.size.height),
                f32::from(typography.line_height)
            );
            assert!(
                (cell.center().y - row.center().y).abs() <= tolerance,
                "the data cell in column {position} drew its line at y={} in a row centered \
                 on y={}",
                f32::from(cell.center().y),
                f32::from(row.center().y)
            );
            // …and the cell's own box being on the row's center is not the same
            // claim. The cell fills the row, so its box is centered whatever it
            // does with its line: a cell that centres nothing and hugs the top
            // passes every assertion above. The *line* is the observable half, and
            // reading it is what caught the defect this file shipped with — a
            // block cell whose `justify_center` taffy drops, which put every data
            // baseline 7px above the status word on the same row.
            let line_selector: &'static str =
                Box::leak(format!("resource-cell-line-0-{position}").into_boxed_str());
            let line = cx
                .debug_bounds(line_selector)
                .unwrap_or_else(|| panic!("{line_selector} is laid out"));
            assert!(
                (line.center().y - row.center().y).abs() <= tolerance,
                "the value in column {position} drew its line at y={} in a row centered on y={} \
                 — a cell that fills the row is not a cell that centres its line",
                f32::from(line.center().y),
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
    /// The test harness already installs a theme, and the app's own design
    /// tokens fall back to the product palette, so a test only has to say that
    /// it wants the harness's window.
    fn init_app(cx: &mut TestAppContext) {
        crate::init_ui(cx);
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn empty_clear_filter_action_is_keyboard_operable(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        let filter = view.read_with(cx, |view, _| view.filter.clone());
        cx.update(|window, cx| {
            filter.update(cx, |input, cx| input.set_text("missing", window, cx));
        });
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

    // Exercises the real key binding and action path.
    #[gpui_kit::test]
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

    #[gpui_kit::test]
    /// Enter and the row menu's `Open Details` both report the activated row.
    /// A click does not, because a click only selects.
    #[gpui_kit::test]
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

    // An unrecognised kind has no columns, so an empty list is expected.
    #[gpui_kit::test]
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

        let (status, rows) = view.read_with(cx, |view, cx| {
            (format!("{:?}", view.status(cx)), view.row_count(cx))
        });
        eprintln!(
            "PROBE unknown_kind: status={status} rows={rows} resource-empty={:?}",
            cx.debug_bounds("resource-empty")
        );
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

    // The status filter is the one the reader did not type, so it has to be
    // named before a name that happens not to match.

    // The NoProblems state used to tell the reader to "Clear the filter" for a
    // filter they never typed, which points at the wrong control.
    #[gpui_kit::test]
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
        view.update(cx, |view, cx| view.set_problems_only(true, cx));
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
    #[gpui_kit::test]
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
    //
    // `UI-SPEC` §4.4 moved the count out of the deleted toolbar and into the
    // selection action bar, which is docked at the *bottom* and appears only once
    // more than one row is selected. Both halves are asserted: the bar is absent
    // for a single row, and when it is there it is below the rows rather than
    // over them.
    #[gpui_kit::test]
    fn the_selection_bar_counts_and_announces_a_range(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_window, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(900.)));
        cx.run_until_parked();
        focus_table(cx, &view);
        assert!(
            cx.debug_bounds("selection-bar").is_none(),
            "an empty selection needs no bar"
        );
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 1);
        assert!(
            cx.debug_bounds("selection-bar").is_none(),
            "§4.4 shows the bar at more than one selected row, not at one"
        );
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 2);
        let count = cx
            .debug_bounds("selection-count")
            .expect("the selection count");
        assert!(count.size.width > px(0.0));
        // `Role::Status` is the live region GPUI can express: there is no
        // `aria_live` on the element API, and a polite status region is what
        // announces the count when it changes.
        let last_row = cx.debug_bounds("resource-row-1").expect("the second row");
        assert!(
            count.top() > last_row.bottom(),
            "§4.4 docks the bar at the bottom of the table, not over the rows"
        );
        // §4.4: 40px, `surface.content`, a 1px `border.subtle` on top and no
        // shadow. The height is the load-bearing number — it is what makes the bar
        // a band rather than a floating card.
        let bar = cx.debug_bounds("selection-bar").expect("the bar");
        assert_eq!(
            f32::from(bar.size.height),
            f32::from(design::size::SELECTION_BAR),
            "§4.4 fixes the bar at 40px"
        );
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.selection_count()), 1);
        assert!(
            cx.debug_bounds("selection-bar").is_none(),
            "the bar follows the count down to nothing"
        );
    }

    // A selection that outlives its rows keeps one active row.
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

    // The delete bar's marker is a severity-coloured glyph directly beside 13px
    // body text, so it shares `design::size::STATUS_MARKER` with every other
    // status mark. A bigger mark must not grow the bar either: the 28px control
    // and the 2px band on each side still decide its height.
    #[gpui_kit::test]
    fn the_delete_bars_marker_matches_the_status_markers(cx: &mut TestAppContext) {
        init_app(cx);
        let subscribes = Arc::new(AtomicUsize::new(0));
        let (view, cx) =
            cx.add_window_view(|_, cx| PodsView::new(test_factory(subscribes), None, cx));
        cx.run_until_parked();
        // Wide enough that the confirmation sentence stays on one line. The bar's
        // height is being measured to find out whether the mark grew it, and a
        // wrapped sentence grows it for an unrelated reason.
        cx.simulate_resize(gpui_kit::size(px(1600.), px(1000.)));
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
        // §4.4 fixes the bar at 40px, which is the slot the selection action bar
        // lives in: the confirmation is that bar escalated, not a second element
        // in a second place. A 16px mark inside it costs the bar nothing, which is
        // the whole point of asserting the height.
        let bar = cx.debug_bounds("multi-delete-bar").expect("the bar");
        assert_eq!(
            f32::from(bar.size.height),
            f32::from(design::size::SELECTION_BAR),
            "the confirmation is the selection bar in its escalated state"
        );
    }

    // Confirming removes the bar, so the focus cannot stay on a button that is
    // no longer on screen.
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
        // `UI-SPEC` §4.14: a list that resolves inside 500ms gets a spinner and
        // not a skeleton, so a skeleton assertion has to move the clock past the
        // tier — otherwise it is asserting that the skeleton is gone.
        cx.executor()
            .advance_clock(SKELETON_AFTER + Duration::from_millis(1));
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();

        assert!(cx.debug_bounds("table-loading-skeleton").is_some());
        assert!(
            cx.debug_bounds("table-skeleton-header-namespace").is_none(),
            "a hidden column gets no skeleton header"
        );
        let name = cx
            .debug_bounds("table-skeleton-header-name")
            .expect("name skeleton header");
        let expected = cx.update(|_window, cx| {
            view.read_with(cx, |view, _cx| {
                let position = view
                    .visible_columns
                    .iter()
                    .position(|&index| view.columns[index].column.id == "name")
                    .expect("name column stays visible");
                view.column_widths
                    .get(position)
                    .copied()
                    .map(px)
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

    use gpui_kit::TestAppContext;
    use k8s_core::controller::{StoreEvent, StoreOp};
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::table_view::source::{ResourceSource, SourceEvent, Subscription};

    /// Installs the component library the harness window renders through.
    ///
    /// gpui-kit keeps its own theme, and `cx.theme()` panics without it, so the
    /// harness does not install one: a test that wants the app's own design
    /// tokens has to ask for them here.
    fn init_app(cx: &mut TestAppContext) {
        crate::init_ui(cx);
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

        {
            let (status, rows, sel) = view.read_with(cx, |view, cx| {
                (
                    format!("{:?}", view.status(cx)),
                    view.row_count(cx),
                    view.selection_count(),
                )
            });
            eprintln!("PROBE delete_conf: status={status} rows={rows} sel={sel}");
        }

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
