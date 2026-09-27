//! Cluster overview with health, counts, and node capacity.
//! Loads one snapshot without a watch. Node metrics are optional.

use std::{
    cell::Cell,
    cmp::Ordering,
    future::Future,
    rc::Rc,
    time::{Duration as TimeDuration, SystemTime, UNIX_EPOCH},
};

use gpui::{
    AnyElement, ClickEvent, Context, FocusHandle, Hsla, IntoElement, KeyDownEvent,
    ListHorizontalSizingBehavior, MouseButton, ParentElement, Render, Role, ScrollStrategy,
    SharedString, StatefulInteractiveElement, Styled, Task, UniformListScrollHandle, Window, div,
    px, uniform_list,
};
use k8s_core::overview::{
    HealthLevel, NodeCapacity, NodeUsage, OVERVIEW_SOURCE_COUNT, Overview, ReplicaSummary,
    SOURCE_NOT_LOADED, WorkloadCounts,
};
use k8s_core::projection::Sort;
use tokio::runtime::Handle;
use ui::prelude::*;
use ui::{ScrollAxes, Scrollbars, TintColor, Tooltip, WithScrollbar};

use super::common::{empty_state, label_panel_title, label_section, label_small, label_text};
use crate::design::{self, Severity, space};
use crate::session::OpsFuture;
use crate::settings::{self, DataTypography};
use k8s_core::cluster_data::ClusterDataSource;

/// Loads one snapshot with optional node metrics.
#[derive(Clone)]
pub struct OverviewHandle {
    handle: Handle,
    service: ClusterDataSource,
}

impl OverviewHandle {
    pub fn new(handle: Handle, source: impl Into<ClusterDataSource>) -> Self {
        Self {
            handle,
            service: source.into(),
        }
    }

    /// Loads and aggregates one snapshot.
    pub fn load_future(&self, metrics: bool) -> OpsFuture<Overview> {
        let service = self.service.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            join_abortable(
                &handle,
                async move { service.port().overview(metrics).await },
            )
            .await
            .map_err(|error| {
                format!(
                    "Overview request failed: {error}. Retry, or make sure the cluster connection works."
                )
            })?
        })
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn join_abortable<T>(
    handle: &Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
{
    let task = handle.spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    task.await
}

/// Formats CPU cores.
pub fn format_cores(cores: f64) -> String {
    if (cores - cores.round()).abs() < 0.005 {
        format!("{}", cores.round() as i64)
    } else {
        format!("{cores:.2}")
    }
}

/// Formats bytes with binary units.
pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [(&str, f64); 4] = [
        ("TiB", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let value = bytes / scale;
            return if (value - value.round()).abs() < 0.05 {
                format!("{} {unit}", value.round() as i64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    format!("{} B", bytes.round() as i64)
}

fn format_millicores(millicores: f64) -> String {
    format!("{}m", millicores.round() as i64)
}

/// Column minimum widths, in column order.
///
/// A capacity cell holds a figure and the capacity it is measured against, so a
/// column narrower than its own value ends every row in an ellipsis, and an
/// ellipsis reports no value at all. The minimums are therefore sized for the
/// text a row actually renders: a 28-character node name, `1.3 GiB / 31.1 GiB`,
/// `161m / 20 cores`, and the severity slot a request cell reserves for the
/// overcommit glyph. Below this grid the table keeps the minimums and scrolls,
/// instead of squashing numbers into unreadable widths.
const CAPACITY_NODE_WIDTH: f32 = 208.0;
const CAPACITY_CPU_REQUEST_WIDTH: f32 = 128.0;
const CAPACITY_MEMORY_REQUEST_WIDTH: f32 = 152.0;
const CAPACITY_CPU_LIMITS_WIDTH: f32 = 72.0;
const CAPACITY_MEMORY_LIMITS_WIDTH: f32 = 92.0;
const CAPACITY_CPU_NOW_WIDTH: f32 = 144.0;
const CAPACITY_MEMORY_NOW_WIDTH: f32 = 144.0;
/// Panel width below which the section headings lose their explanations.
///
/// The explanation sentence is the only place the panel says what a column
/// holds, and dropping it at the same width that pushes columns out of view
/// removes the reader's last way to find out. It is therefore tied to the
/// supported window rather than to a comfortable one: the rule is read against
/// the panel's own width, which is never wider than the window, and the minimum
/// window is 960px, so the sentence is still there at the floor and only a panel
/// narrower than 900px gives it up.
const OVERVIEW_COMPACT_WIDTH: f32 = 900.0;
/// Workload kinds per grid row.
///
/// A pair such as `104 / 10,004` is 12 characters of data font, about 86px, and
/// the label above it is narrower than that. Five of them and four 8px gaps is
/// under 460px, so the narrowest centre panel the shell can build holds all five
/// on one row. Three columns was a full step too conservative: it wrapped one
/// kind onto a row of its own and left two empty slots beside it. The two
/// constants are now equal, kept separate so the distinction stays a decision
/// rather than an accident.
const WORKLOAD_COLUMNS_COMPACT: usize = 5;
const WORKLOAD_COLUMNS_WIDE: usize = 5;
/// Rows the capacity table shows before it scrolls.
///
/// A cluster can have hundreds of nodes. The table is virtualised, so the
/// viewport is bounded and only the visible rows are built.
const CAPACITY_TABLE_MAX_ROWS: f32 = 10.0;
/// Widest gap the width measurement ignores while a splitter is dragged.
const WIDTH_MEASURE_STEP: f32 = 8.0;

/// Column minimum widths, in column order.
const CAPACITY_COLUMN_MIN_WIDTHS: [f32; 7] = [
    CAPACITY_NODE_WIDTH,
    CAPACITY_CPU_REQUEST_WIDTH,
    CAPACITY_MEMORY_REQUEST_WIDTH,
    CAPACITY_CPU_LIMITS_WIDTH,
    CAPACITY_MEMORY_LIMITS_WIDTH,
    CAPACITY_CPU_NOW_WIDTH,
    CAPACITY_MEMORY_NOW_WIDTH,
];

/// Share of the spare width each column takes.
///
/// The node name is the longest string on the row and the one a reader looks up,
/// so it takes the largest share; the numeric columns take the rest in the order
/// of how much text they hold. The live columns now carry a reading *and* the
/// capacity it is measured against, so they are sized like the request columns
/// rather than like the bare limits.
const CAPACITY_COLUMN_WEIGHTS: [f32; 7] = [3.0, 2.0, 2.0, 1.0, 1.0, 2.0, 2.0];

/// Column headings, in column order.
///
/// A heading is a noun phrase, so the live columns name the reading they hold
/// rather than a bare `Now`. The panel is sentence case everywhere else, so the
/// headings are too; the `CPU` acronym stays uppercase.
const CAPACITY_COLUMNS: [&str; 7] = [
    "Node",
    "CPU requests",
    "Memory requests",
    "CPU limits",
    "Memory limits",
    "CPU now",
    "Memory now",
];

/// Tab order of the capacity table inside the Overview panel.
const CAPACITY_TABLE_TAB_INDEX: isize = 2;

/// How the capacity table answers keyboard navigation.
const CAPACITY_TABLE_DESCRIPTION: &str = "Node requests, limits, and current usage. \
Use Up and Down to move between nodes. Use Left and Right to move between columns. \
Use Home and End for the first or last column in a row. \
Use Control+Home or Control+End for the first or last cell in the table. \
Select a column heading to sort by it.";

/// The keys the capacity table answers, for assistive technology.
const CAPACITY_TABLE_KEYS: &str = "ArrowLeft ArrowRight ArrowUp ArrowDown Home End \
Control+Home Control+End PageUp PageDown";

/// Live columns: current CPU and current memory, one axis per cell.
const CAPACITY_LIVE_COLUMN_COUNT: usize = 2;

/// Column count without the live reading columns.
const CAPACITY_COLUMN_COUNT: usize = CAPACITY_COLUMN_MIN_WIDTHS.len() - CAPACITY_LIVE_COLUMN_COUNT;

/// The Overview refreshes on request only.
///
/// The toolbar says so, because a timestamp next to a refresh button implies a
/// timer that this panel does not run.
const MANUAL_REFRESH_LABEL: &str = "Manual refresh";

/// Column widths for the available width, never below the column minimums.
fn capacity_column_widths(available: f32, metrics_available: bool) -> Vec<f32> {
    let count = if metrics_available {
        CAPACITY_COLUMN_MIN_WIDTHS.len()
    } else {
        CAPACITY_COLUMN_COUNT
    };
    let minimum: f32 = CAPACITY_COLUMN_MIN_WIDTHS[..count].iter().sum();
    // Below the minimum grid the columns keep their minimums and the table
    // scrolls, instead of squashing numbers into unreadable widths.
    let table_width = if available.is_finite() {
        available.max(minimum)
    } else {
        minimum
    };
    let spare = table_width - minimum;
    let weight_total: f32 = CAPACITY_COLUMN_WEIGHTS[..count].iter().sum();
    (0..count)
        .map(|column| {
            let share = CAPACITY_COLUMN_WEIGHTS[column] / weight_total;
            CAPACITY_COLUMN_MIN_WIDTHS[column] + spare * share
        })
        .collect()
}

/// Width the columns take on their own. The row chrome is added on top of it,
/// so a table is wider than the sum of its columns.
fn capacity_table_width(widths: &[f32]) -> f32 {
    widths.iter().sum()
}

/// Padding the panel puts around the capacity grid: the scroll padding, the
/// group border, and the group inset the capacity section carries, on both
/// sides.
fn capacity_panel_padding() -> f32 {
    2.0 * (2.0 * f32::from(space::LG) + f32::from(design::border::LINE))
}

/// Width the capacity grid may use in a panel measured at `measured`.
///
/// The measurement is taken at paint time and applied on the next frame, so on
/// the frame right after a shrink-resize it is still the width the panel used
/// to have. The window is the hard ceiling: a table wider than the window it
/// sits in forces a horizontal scroll, and the minimum window size has to keep
/// the table readable without one.
fn capacity_available_width(measured: f32, window_width: f32) -> f32 {
    let padding = capacity_panel_padding();
    (measured - padding).clamp(0.0, (window_width - padding).max(0.0))
}

/// Width a capacity row adds around its cells: the row padding on both sides
/// and the gap between neighbouring columns.
///
/// The heading and every body row carry the same chrome, so the columns share
/// what is left of the table box. A row whose cells add up to more than the row
/// holds pushes the last column outside the table, where the scroll container
/// clips it and the minimum window size grows a scrollbar.
fn capacity_row_chrome(column_count: usize) -> f32 {
    let gap = f32::from(space::SM);
    2.0 * gap + column_count.saturating_sub(1) as f32 * gap
}

/// The surface the capacity table is painted on.
///
/// `DESIGN.md` §3.4 assigns `surface` to tables, and every row wash in `design`
/// composites onto it. The row base used to stay on `canvas` while the zebra,
/// the hover and the selection all landed on `surface`, so the stripe was mixed
/// against a base it is not drawn on — a difference small enough to survive a
/// screenshot and wrong on every pixel. The heading takes the same base, so the
/// header band and the rows under it are one opaque table rather than a strip of
/// canvas above a surface.
fn capacity_table_surface(cx: &gpui::App) -> Hsla {
    design::surface::input(cx).alpha(1.0)
}

/// The figure the panel prints when the cluster reported nothing.
///
/// One dash for every kind of "no answer", so a denied source, a source that has
/// not answered yet, and a genuinely empty list all read as the same absence
/// rather than as three different zeroes.
const NO_ANSWER: &str = "—";

/// True when a figure is the no-answer dash rather than a reading.
fn is_no_answer(figure: &str) -> bool {
    figure == NO_ANSWER
}

fn format_pct(value: f64) -> String {
    format!("{value:.0}%")
}

fn request_ratio(requested: f64, allocatable: Option<f64>) -> Option<f64> {
    allocatable
        .filter(|value| *value > 0.0)
        .map(|value| requested / value * 100.0)
}

/// The figure a limit column shows.
///
/// Kubernetes reads a zero limit as "no limit declared", and a bare `0` reads
/// as "this node's limit is zero", which is a different and much worse claim.
fn format_limit(limit: f64, format: impl Fn(f64) -> String) -> String {
    if limit <= 0.0 {
        NO_ANSWER.to_owned()
    } else {
        format(limit)
    }
}

/// The two-part figure a request cell shows: `1.15 / 20 cores`.
///
/// The cell keeps the pair and nothing else. The share of allocatable would
/// push the value past the column width, and a cell that ends in an ellipsis
/// reports no value at all. The share lives in the row label, the tooltip, and
/// the severity glyph of an oversubscribed node, so it is stated in words in
/// both the tooltip and the cell's accessible name.
fn request_figure(
    requested: f64,
    allocatable: Option<f64>,
    format_requested: impl Fn(f64) -> String + Copy,
    format_allocatable: impl Fn(f64) -> String + Copy,
) -> String {
    let requested_text = format_requested(requested);
    match allocatable.filter(|value| *value > 0.0) {
        Some(allocatable) => format!("{requested_text} / {}", format_allocatable(allocatable)),
        None => format!("{requested_text} requested"),
    }
}

/// The figure plus its share of allocatable, for a row label and a tooltip.
fn request_text(
    requested: f64,
    allocatable: Option<f64>,
    format_requested: impl Fn(f64) -> String + Copy,
    format_allocatable: impl Fn(f64) -> String + Copy,
) -> String {
    let Some(allocatable) = allocatable.filter(|value| *value > 0.0) else {
        return format!(
            "{} requested · allocatable unavailable",
            format_requested(requested)
        );
    };
    let figure = request_figure(
        requested,
        Some(allocatable),
        format_requested,
        format_allocatable,
    );
    let percentage = format_pct(requested / allocatable * 100.0);
    if requested > allocatable {
        format!("{figure} · {percentage} oversubscribed")
    } else {
        format!("{figure} · {percentage} of allocatable")
    }
}

fn format_node_usage(usage: Option<&NodeUsage>, metrics_available: bool) -> String {
    let Some(usage) = usage else {
        return if metrics_available {
            "Waiting for the first sample".to_owned()
        } else {
            "Metrics unavailable".to_owned()
        };
    };
    let mut parts = Vec::new();
    if let Some(cpu) = usage.cpu_millicores {
        parts.push(format!("CPU {}", format_millicores(cpu)));
    }
    if let Some(memory) = usage.memory_bytes {
        parts.push(format!("Memory {}", format_bytes(memory)));
    }
    if parts.is_empty() {
        return if metrics_available {
            "Waiting for the first sample".to_owned()
        } else {
            "Metrics unavailable".to_owned()
        };
    }
    parts.join(" · ")
}

/// The short, honest answer a live cell gives before the cluster reports.
///
/// The cluster answered for some nodes and not yet for this one, or metrics-server
/// is not installed. A zero would be a reading the app never took, and a cell
/// sized for a number would only show the first word of the full sentence, which
/// stays in the row's spoken label instead.
fn no_reading(metrics_available: bool) -> &'static str {
    if metrics_available {
        "Waiting"
    } else {
        "No metrics"
    }
}

/// One live reading with the capacity it is measured against.
///
/// A bare `161m` next to a bare `3 GiB` cannot be read: 3 GiB is 10% of one node
/// and 90% of another, and nothing on the row says which. Both live columns
/// therefore print the pair, exactly as the request columns do, so a row states
/// one kind of quantity in one place. `1 core = 1000m` needs no explanation
/// because both numbers are printed.
fn format_cpu_now(
    usage: Option<&NodeUsage>,
    capacity: &NodeCapacity,
    metrics_available: bool,
) -> String {
    match usage.and_then(|usage| usage.cpu_millicores) {
        Some(millicores) => request_figure(
            millicores / 1_000.0,
            capacity.allocatable_cpu,
            |cores| format_millicores(cores * 1_000.0),
            |cores| format!("{} cores", format_cores(cores)),
        ),
        None => no_reading(metrics_available).to_owned(),
    }
}

fn format_memory_now(
    usage: Option<&NodeUsage>,
    capacity: &NodeCapacity,
    metrics_available: bool,
) -> String {
    match usage.and_then(|usage| usage.memory_bytes) {
        Some(memory) => request_figure(
            memory,
            capacity.allocatable_memory,
            format_bytes,
            format_bytes,
        ),
        None => no_reading(metrics_available).to_owned(),
    }
}

fn format_clock(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .unwrap_or(TimeDuration::ZERO)
        .as_secs();
    let seconds = seconds % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

/// The freshness of the snapshot, with the refresh mode named.
///
/// The panel loads on open and on request, so the copy says "manual refresh".
/// A bare "last refreshed" next to a refresh button would promise a timer that
/// does not exist.
fn refresh_status(
    state: &OverviewState,
    refreshing: bool,
    last_refreshed: Option<&SystemTime>,
) -> String {
    let updated = || match last_refreshed {
        Some(at) => format!(" · updated {}", format_clock(*at)),
        None => String::new(),
    };
    if refreshing {
        return format!("Refreshing… · {MANUAL_REFRESH_LABEL}{}", updated());
    }
    match state {
        OverviewState::Loading => "Refreshing…".to_owned(),
        OverviewState::Ready(overview) => match overview_data_state(overview) {
            OverviewDataState::Error | OverviewDataState::Forbidden => {
                format!("{MANUAL_REFRESH_LABEL} · not loaded")
            }
            OverviewDataState::Partial => format!("{MANUAL_REFRESH_LABEL}{}", updated()),
            _ => format!("{MANUAL_REFRESH_LABEL}{}", updated()),
        },
        OverviewState::Failed(_) => format!("{MANUAL_REFRESH_LABEL} · refresh failed"),
    }
}

fn data_text(
    text: impl Into<SharedString>,
    typography: &DataTypography,
    color: Color,
    cx: &gpui::App,
) -> gpui::Div {
    div()
        .font(typography.font.clone())
        .font_features(typography.features.clone())
        .text_size(rems_from_px(f32::from(typography.size)))
        .line_height(rems_from_px(f32::from(typography.line_height)))
        .text_color(color.color(cx))
        .child(text.into())
}

/// The one figure the page leads with, at the display size.
///
/// The data face keeps the figures tabular and aligned with the table below, and
/// the size is the token reserved for the number a surface exists to communicate,
/// so the hero is the largest text on the page without introducing a new size.
fn display_text(
    text: impl Into<SharedString>,
    typography: &DataTypography,
    color: Color,
    cx: &gpui::App,
) -> gpui::Div {
    div()
        .font(typography.font.clone())
        .font_features(typography.features.clone())
        .text_size(rems_from_px(f32::from(design::text::DISPLAY)))
        .line_height(rems_from_px(f32::from(design::text::DISPLAY_LINE_HEIGHT)))
        .text_color(color.color(cx))
        .child(text.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverviewDataState {
    Empty,
    Partial,
    /// Every source was denied: the cluster is fine, the account is not.
    Forbidden,
    Error,
    Complete,
}

fn workload_data_available(workloads: &WorkloadCounts) -> bool {
    [
        workloads.deployments,
        workloads.stateful_sets,
        workloads.daemon_sets,
        workloads.jobs,
        workloads.cron_jobs,
    ]
    .iter()
    .any(|summary| summary.desired != 0 || summary.available != 0)
}

fn overview_data_state(overview: &Overview) -> OverviewDataState {
    let has_data = overview.has_data
        || overview.nodes.count > 0
        || overview.health.total_pods > 0
        || workload_data_available(&overview.workloads)
        || !overview.capacities.is_empty()
        || overview
            .usage
            .as_ref()
            .is_some_and(|usage| !usage.is_empty());
    let has_source_error = overview
        .unavailable_sources
        .iter()
        .any(|source| source.reason != SOURCE_NOT_LOADED);
    if has_source_error && overview.unavailable_sources.len() >= OVERVIEW_SOURCE_COUNT {
        // A denial is a configuration problem, not a cluster problem. It gets
        // its own state so the copy can name the missing permission.
        return if overview.fully_denied() {
            OverviewDataState::Forbidden
        } else {
            OverviewDataState::Error
        };
    }
    if !has_data && !has_source_error {
        return OverviewDataState::Empty;
    }
    if !overview.unavailable_sources.is_empty() {
        return OverviewDataState::Partial;
    }
    if overview.nodes.count == 0 || overview.capacities.len() != overview.nodes.count {
        OverviewDataState::Partial
    } else {
        OverviewDataState::Complete
    }
}

fn source_failure_detail(overview: &Overview) -> String {
    overview
        .unavailable_sources
        .iter()
        .map(|source| format!("{}: {}", source.source, source.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The permissions a denied snapshot is missing, written for a human.
fn denied_permissions(overview: &Overview) -> Vec<&'static str> {
    overview
        .denied_sources()
        .map(|(_, permission)| permission)
        .collect()
}

/// Copy for a denial: what is missing and what to do about it.
///
/// The raw API text stays in the tooltip. This sentence names the permission,
/// because a denial is answered by an RBAC change, not by a retry.
fn denial_copy(overview: &Overview) -> String {
    let permissions = denied_permissions(overview);
    if permissions.is_empty() {
        return "The cluster denied access to the overview. Grant the permissions this account needs, then refresh."
            .to_owned();
    }
    let named = permissions
        .iter()
        .map(|permission| format!("'{permission}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if permissions.len() > 1 {
        format!(" and {} more", count(permissions.len() - 1))
    } else {
        String::new()
    };
    format!("Access denied: grant {named}{more} in your role, then refresh.")
}

/// How much of the cluster needs attention, counted but not itemised.
///
/// The banner carries the verdict and this count; the figures below carry the
/// detail. Every clause names its own unit and no total is added up across them:
/// pods, nodes, and workload *objects* are three different counts, and one
/// number that mixes them cannot be reconciled with the replica figure 100px
/// below it.
///
/// `unknown` is in the pod count here and out of the hero's severity, and the two
/// cannot move apart, and this copy and the hero cannot answer the same question
/// the title does. The title comes from `k8s_core::overview::level`, which counts
/// an unreadable pod: a cluster with a pod it cannot vouch for is not one it can
/// call healthy, and saying so is the honest reading. The hero is a different
/// question -- the largest bucket's own verdict -- where `Unknown` means no
/// verdict, so it stays quiet. The table row under both is a third question
/// again: what happened to this pod, answered with a dash. Three questions, three
/// answers, and none of them is the other's.
///
/// What this copy must not do is add unread pods to a running count. "4 pods not
/// running" reads as four pods the app knows are down, and a reader who checks
/// that against the pod table finds the number disagrees. So they get their own
/// clause and their own verb.
fn attention_copy(overview: &Overview) -> String {
    let health = overview.health;
    let mut clauses: Vec<String> = Vec::new();
    if overview.nodes.not_ready > 0 {
        clauses.push(format!(
            "{} not ready",
            design::format::count_with_noun(overview.nodes.not_ready, "node", "nodes")
        ));
    }
    let pod_problems = health.pending + health.failed;
    if pod_problems > 0 {
        clauses.push(format!(
            "{} not running",
            design::format::count_with_noun(pod_problems, "pod", "pods")
        ));
    }
    // A pod the kubelet lost track of is a different fact from one that is not
    // running, and the banner above no longer counts it as a health problem --
    // `level()` reads the cluster's condition, and "we cannot vouch for this pod"
    // is the confidence channel, not the health one. Folding it into "not
    // running" gave the reader a number they would check against a pod list that
    // disagrees with it, so it gets its own clause and its own verb.
    if overview.unknown_pods > 0 {
        clauses.push(format!(
            "{} not reporting",
            design::format::count_with_noun(overview.unknown_pods, "pod", "pods")
        ));
    }
    if overview.unavailable_workloads > 0 {
        clauses.push(format!(
            "{} below target",
            design::format::count_with_noun(
                overview.unavailable_workloads,
                "workload",
                "workloads"
            )
        ));
    }
    if clauses.is_empty() {
        return "Nothing needs attention.".to_owned();
    }
    format!("{}.", clauses.join(", "))
}

/// Alpha of the status wash under the banner.
///
/// The theme status backgrounds are full-strength fills. Painted at full
/// strength under a wide banner they read as a slab, and the dark theme's
/// warning background is a low-saturation brown that reads as disabled. A tint
/// of the status colour keeps the meaning and loses the weight.
const HEALTH_WASH_ALPHA: f32 = 0.12;

/// The banner fill: the status colour over the canvas, at low alpha.
fn health_wash(severity: Severity, cx: &gpui::App) -> Hsla {
    design::composite_surface(
        design::surface::canvas(cx),
        severity.color(cx).opacity(HEALTH_WASH_ALPHA),
    )
}

// The notice fill lives in `design` beside the glyph and the word, so the rule
// that a component may not keep a private status map has one place to point at.

/// Shape cue for the banner.
///
/// The banner previously carried a local override because the shared icon
/// mapped warning onto the info glyph, which beside a caution fill read as
/// neutral information. The shared mapping now returns a distinct warning
/// glyph, so the banner and every other surface agree on what caution looks
/// like, and the override would only let the two drift apart again.
fn health_banner_icon(severity: Severity) -> IconName {
    design::health_icon(severity)
}

/// Whether the snapshot on screen is the answer the cluster is giving now.
///
/// This is a different question from how the cluster is doing, and the two
/// answers have to stay apart: a cluster the app could not read has no health
/// verdict to report, and a snapshot that is being replaced is not yet wrong.
/// Collapsing the two is what makes a status display lie during an incident.
fn snapshot_confidence(
    state: &OverviewState,
    refreshing: bool,
    refresh_failed: bool,
) -> design::Confidence {
    use design::Confidence;
    if refreshing || refresh_failed {
        // The previous snapshot is still on screen while a newer one is on its
        // way, or a refresh failed and the one on screen is the last good answer.
        return Confidence::Stale;
    }
    match state {
        OverviewState::Loading | OverviewState::Failed(_) => Confidence::Unknown,
        OverviewState::Ready(overview) => match overview_data_state(overview) {
            OverviewDataState::Empty
            | OverviewDataState::Forbidden
            | OverviewDataState::Error
            // A partial snapshot is the app's own problem — an RBAC denial, a
            // source that did not answer — so the app has no verdict about the
            // part it could not read. It reports `Unknown` here, which is what
            // puts the hollow `?` on the banner, because the health channel
            // cannot speak for it: `health_semantics` keeps that state muted and
            // the missing data is a footnote, not a verdict.
            | OverviewDataState::Partial => Confidence::Unknown,
            OverviewDataState::Complete => Confidence::Known,
        },
    }
}

/// The observation answer, on the banner's trailing edge.
///
/// Health is drawn with filled and outlined glyphs, so confidence uses the hollow
/// shapes `design::confidence` owns: on a banner carrying both, the shape alone
/// says which axis a mark belongs to. A known answer draws nothing, because the
/// health hue beside it already spoke and a second mark would imply a second
/// thing to read.
fn confidence_marker(state: design::Confidence, cx: &gpui::App) -> gpui::Stateful<gpui::Div> {
    let foreground = design::confidence::foreground(state, cx);
    let label = design::confidence_label(state);
    let mut marker = div()
        .id("overview-confidence")
        .debug_selector(|| "overview-confidence".to_owned())
        .flex_none()
        .flex()
        .items_center()
        .pr(space::MD)
        .child(
            Icon::new(design::confidence::icon(state))
                .size(IconSize::XSmall)
                .color(Color::Custom(foreground)),
        )
        .role(Role::Status)
        .aria_label(label);
    marker.interactivity().tooltip(Tooltip::text(label));
    marker
}

fn overview_is_compact(width: f32) -> bool {
    width < OVERVIEW_COMPACT_WIDTH
}

/// Long caption text earns a tooltip so a truncated value stays readable.
fn caption_tooltip(text: &str) -> Option<String> {
    (text.chars().count() > 48).then(|| text.to_owned())
}

fn health_level_semantics(level: HealthLevel) -> (&'static str, Severity) {
    match level {
        HealthLevel::Healthy => ("Cluster healthy", Severity::Success),
        HealthLevel::Warning => ("Cluster needs attention", Severity::Warning),
        HealthLevel::Error => ("Failed pods detected", Severity::Error),
    }
}

/// The cluster verdict, from the cluster's own facts.
///
/// This is the one place in the app that answers "is this cluster healthy", so
/// it must not be reachable from the connection: `Overview::level` counts
/// failed pods, pods the API has not placed, nodes that are not ready, and
/// workloads below their target, and nothing else. A snapshot that could not be
/// read completely has no verdict at all and says so through the confidence
/// channel instead.
fn health_semantics(overview: &Overview) -> (&'static str, Severity) {
    match overview_data_state(overview) {
        OverviewDataState::Empty => ("No cluster data", Severity::Muted),
        // The cluster is missing a permission, not unhealthy. Amber here would
        // spend the caution channel on the app's own configuration and leave the
        // reader hunting for a cluster problem that is not there, so the state
        // is muted and the hollow confidence mark carries "I could not read it".
        OverviewDataState::Partial => ("Partial cluster data", Severity::Muted),
        OverviewDataState::Forbidden => ("Access denied", Severity::Warning),
        OverviewDataState::Error => ("Failed to load cluster data", Severity::Error),
        OverviewDataState::Complete => health_level_semantics(overview.level()),
    }
}

/// The banner answers one question: is this cluster healthy, and what needs
/// attention. The figures below give the counts, so nothing is printed twice.
fn health_copy(overview: &Overview) -> (&'static str, String) {
    let (title, _) = health_semantics(overview);
    let detail = match overview_data_state(overview) {
        OverviewDataState::Empty => {
            "The cluster returned no node or pod data. Make sure the cluster connection works. Refresh the overview."
                .to_owned()
        }
        OverviewDataState::Partial => {
            let mut detail = "Some cluster data is unavailable. The overview shows the data the cluster returned."
                .to_owned();
            if overview.access_denied() {
                detail.push(' ');
                detail.push_str(&denial_copy(overview));
            }
            detail
        }
        OverviewDataState::Forbidden => denial_copy(overview),
        OverviewDataState::Error => {
            "Failed to load the cluster overview. Retry, or make sure the cluster connection works."
                .to_owned()
        }
        OverviewDataState::Complete => attention_copy(overview),
    };
    (title, detail)
}

fn health_severity(overview: &Overview) -> Severity {
    health_semantics(overview).1
}

/// A count for a user-facing figure.
///
/// Every count in this panel goes through the shared formatter, so a figure
/// reads the same here and in the table toolbar.
fn count(value: usize) -> String {
    design::format::count(value)
}

/// The cluster-wide node readiness, in the panel's own ratio spelling.
///
/// `ratio` owns the dash and the separator, so the same `2 / 3` appears here and
/// on the `Nodes ready` figure instead of a third spacing of its own.
fn node_context(overview: &Overview) -> (String, Severity) {
    if overview.nodes.count == 0 {
        ("No nodes reported".to_owned(), Severity::Muted)
    } else if overview.nodes.not_ready == 0 {
        (
            format!(
                "{} ready",
                ratio(overview.nodes.ready, overview.nodes.count)
            ),
            Severity::Success,
        )
    } else {
        (
            format!(
                "{} ready · {} not ready",
                ratio(overview.nodes.ready, overview.nodes.count),
                count(overview.nodes.not_ready)
            ),
            Severity::Warning,
        )
    }
}

/// The workload kinds the panel itemises, with the name a person would say.
///
/// The grid the reader scans and the sentence assistive technology reads come from this one list
/// and [`workload_summaries`], in the same order, so a kind cannot be on screen and missing from
/// the label that names it.
const WORKLOAD_LABELS: [&str; 5] = [
    "Deployments",
    "Stateful sets",
    "Daemon sets",
    "Jobs",
    "Cron jobs",
];

/// The snapshot source behind each kind in [`WORKLOAD_LABELS`].
///
/// A source the cluster refused is the difference between "this cluster has no
/// StatefulSet" and "this app was not allowed to look", and the two used to
/// render as the same `0/0`.
const WORKLOAD_SOURCES: [&str; 5] = [
    "deployments",
    "statefulsets",
    "daemonsets",
    "jobs",
    "cronjobs",
];

/// The five kinds' replica totals, in [`WORKLOAD_LABELS`] order.
fn workload_summaries(counts: &WorkloadCounts) -> [ReplicaSummary; 5] {
    [
        counts.deployments,
        counts.stateful_sets,
        counts.daemon_sets,
        counts.jobs,
        counts.cron_jobs,
    ]
}

/// Replica counts arrive signed, but a negative replica count is not a thing.
fn replica_counts(summary: ReplicaSummary) -> (usize, usize) {
    (
        usize::try_from(summary.available).unwrap_or(0),
        usize::try_from(summary.desired).unwrap_or(0),
    )
}

/// `available / desired` for one kind, both counts through the shared formatter.
///
/// A kind the cluster reported nothing for prints the dash, like every other
/// "no answer" on the panel. `0/0` was the same string as a reading, in the same
/// full-strength ink as a kind with real data.
fn replica_figure(summary: ReplicaSummary) -> String {
    let (available, desired) = replica_counts(summary);
    ratio(available, desired)
}

/// True when the snapshot could not read this kind's source at all.
fn workload_source_unavailable(overview: &Overview, source: &str) -> bool {
    overview
        .unavailable_sources
        .iter()
        .any(|entry| entry.source == source)
}

/// The per-kind workload figures, `None` where the app could not read the kind.
///
/// These are the five parts of the workload ratio. They are laid out as a grid,
/// because one `·`-separated line of five pairs is a sentence and the eye cannot
/// find one kind in it. A kind the cluster refused has no figure at all, which is
/// a different statement from a kind with nothing in it.
fn workload_breakdown(overview: &Overview) -> Vec<(&'static str, Option<String>)> {
    if !workload_data_available(&overview.workloads) {
        return Vec::new();
    }
    WORKLOAD_LABELS
        .iter()
        .copied()
        .zip(WORKLOAD_SOURCES)
        .zip(workload_summaries(&overview.workloads))
        .map(|((label, source), summary)| {
            let figure =
                (!workload_source_unavailable(overview, source)).then(|| replica_figure(summary));
            (label, figure)
        })
        .collect()
}

/// The same five pairs as one sentence, for a row's spoken label.
fn workload_detail(overview: &Overview) -> String {
    if !workload_data_available(&overview.workloads) {
        return "The cluster reported no workload data. Refresh to check again.".to_owned();
    }
    workload_breakdown(overview)
        .into_iter()
        .map(|(label, figure)| match figure {
            Some(figure) => format!("{label} {figure}"),
            None => format!("{label} not read"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one figure the page leads with.
///
/// The verdict already lives in the banner above it, so what belongs in the
/// largest type on the page is the load-bearing ratio: the pods that are running
/// against the total the cluster reports. `typography.md > Conveying hierarchy`
/// asks for the content people care about to be the content that grows, and this
/// is the number they came for.
struct VitalFigure {
    /// Sentence-case label, the same capitalisation the rest of the panel uses.
    label: &'static str,
    value: String,
    /// The one thing the ratio does not say. A run of counts joined by dots is a
    /// sentence, so the caption names the largest bucket, counts the rest, and
    /// the full breakdown goes to the tooltip and to assistive technology.
    caption: String,
    /// The whole sentence, for the tooltip and the accessible label.
    detail: String,
    /// Severity of the caption, so an off-number is never carried by color alone:
    /// the caption text says the same thing.
    severity: Severity,
    /// True when the caption names a bucket the reader can go and look at.
    ///
    /// `all running` and `No pods reported` are not things to follow, so they
    /// stay quiet text. A bucket with a count in it becomes the control that
    /// opens the Pods table filtered to those pods.
    actionable: bool,
}

/// A ratio below the hero, in a fixed order.
///
/// A cluster has no natural ranking between its own counts, so the order is the
/// app's and never changes: a reader who learns it on one cluster finds the same
/// row on the next.
struct SignalFigure {
    label: &'static str,
    value: String,
    caption: String,
    detail: String,
    severity: Severity,
}

/// A value like `3 / 3`, or a dash when the cluster reported nothing.
fn ratio(available: usize, total: usize) -> String {
    if total == 0 {
        return NO_ANSWER.to_owned();
    }
    format!("{} / {}", count(available), count(total))
}

/// Caption ink, from the shared severity vocabulary.
///
/// The caption says the same thing in words, so the colour only reinforces it and
/// never carries the state on its own — which is exactly why it has to be the
/// same colour the health channel uses. A private `Severity` to `Color` map put a
/// second answer next to it: a caption that reads amber where the banner beside
/// it reads green is `DESIGN.md` §2's "one colour, two meanings", and a new
/// severity would have been folded into the muted arm without anybody noticing.
fn caption_color(severity: Severity, cx: &gpui::App) -> Color {
    Color::Custom(severity.color(cx))
}

/// The hero figure and the ratios under it, in the order the panel draws them.
fn vital_figures(overview: &Overview) -> (VitalFigure, Vec<SignalFigure>) {
    let health = overview.health;
    let nodes = overview.nodes;
    let workloads = overview.workloads;

    // The caption names the largest bucket that is not running and says how much
    // is in the buckets it leaves out: the ratio beside it already says how many
    // are, but a page that reports 9,900 pending and hides 7 more unready pods
    // only tells the truth to the reader who hovers.
    //
    // The order is a tie-break, not a priority list, and it reads backwards on
    // purpose: `max_by_key` returns the *last* of several equal maxima, so the
    // worst verdict is listed last and wins the tie. `unknown` is first because it
    // is the one bucket that is not a verdict at all — a pod in the `Unknown`
    // phase is a pod the kubelet lost, and `DESIGN.md` §4 says a thing the app
    // could not read is not a thing that is wrong — so it is the bucket that must
    // lose a tie against a real problem. Both directions of that are asserted in
    // `an_unknown_pod_is_not_a_warning_on_the_hero`.
    let unready = [
        (overview.unknown_pods, "unknown"),
        (health.pending, "pending"),
        (health.failed, "failed"),
    ];
    let unready_total: usize = unready.iter().map(|(number, _)| *number).sum();
    let pod_caption = if health.total_pods == 0 {
        "No pods reported".to_owned()
    } else {
        match unready
            .iter()
            .max_by_key(|(number, _)| *number)
            .filter(|(number, _)| *number > 0)
        {
            Some((number, name)) => {
                let rest = unready_total.saturating_sub(*number);
                let lead = format!("{} {name}", count(*number));
                if rest == 0 {
                    lead
                } else {
                    format!("{lead} · {}", count(rest))
                }
            }
            None => "all running".to_owned(),
        }
    };
    // The page's worst verdict, and nothing else. `unknown` is deliberately not
    // in the ladder: the table draws a pod in the `Unknown` phase as a dash with
    // no verdict, and this caption sat a screenful above it in the banner's own
    // amber, so one pod the kubelet lost read as a caution event on an otherwise
    // healthy cluster. The count still goes in the caption and still routes to
    // those pods — `problems_only` keeps a pod with no verdict — it just does not
    // spend the caution channel.
    let pod_severity = if health.failed > 0 {
        Severity::Error
    } else if health.pending > 0 {
        Severity::Warning
    } else {
        Severity::Muted
    };

    // An empty caption is the row saying nothing: the ratio already reads "1 / 1",
    // and `all ready` repeated the number in words without adding one.
    let node_caption = if nodes.count == 0 {
        "No nodes reported".to_owned()
    } else if nodes.not_ready == 0 {
        String::new()
    } else {
        format!("{} not ready", count(nodes.not_ready))
    };

    let replicas = workloads.replicas();
    let desired = usize::try_from(replicas.desired).unwrap_or(0);
    let available = usize::try_from(replicas.available).unwrap_or(0);
    let workload_caption = if desired == 0 {
        "No workloads reported".to_owned()
    } else if available < desired {
        format!(
            "{} of {} below target",
            count(desired - available),
            count(desired)
        )
    } else {
        "all replicas ready".to_owned()
    };

    let hero = VitalFigure {
        label: "Pods ready",
        value: ratio(health.running, health.total_pods),
        caption: pod_caption,
        detail: format!(
            "Pods running {}, {} pending, {} failed",
            ratio(health.running, health.total_pods),
            count(health.pending),
            count(health.failed)
        ),
        severity: pod_severity,
        actionable: unready_total > 0,
    };
    let signals = vec![
        SignalFigure {
            label: "Nodes ready",
            value: ratio(nodes.ready, nodes.count),
            caption: node_caption,
            detail: format!("Nodes ready {}", ratio(nodes.ready, nodes.count)),
            severity: if nodes.not_ready > 0 {
                Severity::Warning
            } else {
                Severity::Muted
            },
        },
        SignalFigure {
            label: "Workloads ready",
            value: ratio(available, desired),
            caption: workload_caption,
            detail: workload_detail(overview),
            severity: if available < desired {
                Severity::Warning
            } else {
                Severity::Muted
            },
        },
    ];
    (hero, signals)
}

/// The node's own sentence, for the row's accessible name and its tooltip.
///
/// It states the node-level share of allocatable that `utilization_pct` already
/// computes. Nothing on a row spells that number out, and the two request cells
/// each carry only their own axis, so without it the blended figure — the one
/// that says whether *this node* is hot — is computed in the data layer and read
/// nowhere.
fn capacity_summary(capacity: &NodeCapacity, usage: Option<&NodeUsage>) -> String {
    let mut parts = vec![capacity.name.clone()];
    if let Some(pct) = capacity.utilization_pct {
        parts.push(format!(
            "Node utilization: {}{}",
            format_pct(pct),
            if pct > 100.0 {
                " oversubscribed"
            } else {
                " of allocatable"
            }
        ));
    }
    parts.push(format!(
        "CPU requests: {}",
        request_text(
            capacity.requested_cpu,
            capacity.allocatable_cpu,
            format_cores,
            |cores| format!("{} cores", format_cores(cores)),
        )
    ));
    parts.push(format!(
        "Memory requests: {}",
        request_text(
            capacity.requested_memory,
            capacity.allocatable_memory,
            format_bytes,
            format_bytes,
        )
    ));
    parts.push(format!(
        "CPU limits: {}",
        format_limit(capacity.limits_cpu, format_cores)
    ));
    parts.push(format!(
        "Memory limits: {}",
        format_limit(capacity.limits_memory, format_bytes)
    ));
    if let Some(usage) = usage {
        parts.push(format!(
            "Current usage: {}",
            format_node_usage(Some(usage), true)
        ));
    }
    parts.join(", ")
}

#[expect(clippy::large_enum_variant)]
enum OverviewState {
    Loading,
    Ready(Overview),
    Failed(String),
}

fn apply_refresh_result(
    state: &mut OverviewState,
    refreshing: &mut bool,
    refresh_error: &mut Option<String>,
    current_epoch: u64,
    result_epoch: u64,
    result: Result<Overview, String>,
) -> Option<SystemTime> {
    if current_epoch != result_epoch {
        return None;
    }
    *refreshing = false;
    let had_snapshot = matches!(state, OverviewState::Ready(_));
    match result {
        Ok(overview) => {
            *state = OverviewState::Ready(overview);
            *refresh_error = None;
            Some(SystemTime::now())
        }
        Err(reason) if had_snapshot => {
            *refresh_error = Some(reason);
            None
        }
        Err(reason) => {
            *state = OverviewState::Failed(reason);
            None
        }
    }
}

/// Renders and refreshes the cluster overview.
///
/// `show_problems` is the host's route from the panel's biggest number to the
/// rows behind it: it opens the Pods table with the problems filter on. The
/// panel has no navigation of its own, so the host installs it and the caption
/// stays plain text until it does.
pub type ShowProblemsCallback = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct OverviewView {
    refresh_focus: FocusHandle,
    retry_focus: FocusHandle,
    table_focus: FocusHandle,
    handle: Option<OverviewHandle>,
    metrics_available: bool,
    state: OverviewState,
    refreshing: bool,
    refresh_error: Option<String>,
    last_refreshed: Option<SystemTime>,
    content_width: Rc<Cell<f32>>,
    /// Column and direction the capacity table is sorted by. It opens on the
    /// node column, ascending, so the first paint is in a documented order.
    capacity_sort: Sort,
    /// Cursor cell of the capacity table, for keyboard navigation.
    capacity_cursor: (usize, usize),
    capacity_scroll: UniformListScrollHandle,
    show_problems: Option<ShowProblemsCallback>,
    /// Discards results from an older refresh.
    epoch: u64,
    _task: Option<Task<()>>,
}

impl OverviewView {
    pub fn new(
        handle: Option<OverviewHandle>,
        metrics_available: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            refresh_focus: cx.focus_handle().tab_stop(true).tab_index(0),
            retry_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            table_focus: cx
                .focus_handle()
                .tab_stop(true)
                .tab_index(CAPACITY_TABLE_TAB_INDEX),
            handle,
            metrics_available,
            state: OverviewState::Loading,
            refreshing: false,
            refresh_error: None,
            last_refreshed: None,
            content_width: Rc::new(Cell::new(0.0)),
            capacity_sort: Sort::ascending(0),
            capacity_cursor: (0, 0),
            capacity_scroll: UniformListScrollHandle::new(),
            show_problems: None,
            epoch: 0,
            _task: None,
        }
    }

    /// Installs the host's route from the pod figure to the pods that need
    /// attention.
    ///
    /// Without it the panel's biggest number reports 9,900 pending pods and
    /// offers no way to look at them, which is the one thing on this page a
    /// reader is most likely to want to do next.
    pub fn set_show_problems_callback(&mut self, callback: Option<ShowProblemsCallback>) {
        self.show_problems = callback;
    }

    pub fn focus_default_control(&self) -> FocusHandle {
        match &self.state {
            OverviewState::Failed(_) => self.retry_focus.clone(),
            OverviewState::Ready(overview)
                if matches!(
                    overview_data_state(overview),
                    OverviewDataState::Error | OverviewDataState::Forbidden
                ) =>
            {
                self.retry_focus.clone()
            }
            _ => self.refresh_focus.clone(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_default_control()
    }

    /// Reloads when metrics-server availability changes.
    pub fn set_metrics_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.metrics_available == available {
            return;
        }
        self.metrics_available = available;
        self.refresh(cx);
    }

    /// Loads and aggregates a new snapshot.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let epoch = self.epoch.wrapping_add(1);
        self.epoch = epoch;
        self._task = None;
        self.refreshing = true;
        self.refresh_error = None;
        let Some(handle) = self.handle.clone() else {
            self.refreshing = false;
            let reason = "Not connected to a cluster.".to_owned();
            if matches!(self.state, OverviewState::Ready(_)) {
                self.refresh_error = Some(reason);
            } else {
                self.state = OverviewState::Failed(reason);
            }
            cx.notify();
            return;
        };
        if matches!(self.state, OverviewState::Failed(_)) {
            self.state = OverviewState::Loading;
        }
        let future = handle.load_future(self.metrics_available);
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = future.await;
            this.update(cx, |view, cx| {
                if view.epoch == epoch {
                    let completed_at = apply_refresh_result(
                        &mut view.state,
                        &mut view.refreshing,
                        &mut view.refresh_error,
                        view.epoch,
                        epoch,
                        result,
                    );
                    if let Some(completed_at) = completed_at {
                        view.last_refreshed = Some(completed_at);
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> AnyElement {
        let status = refresh_status(&self.state, self.refreshing, self.last_refreshed.as_ref());
        let status_label = status.clone();
        h_flex()
            .id("overview-toolbar")
            .debug_selector(|| "overview-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .tab_group()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "overview-toolbar-title".to_owned())
                    .child(label_panel_title("Cluster Overview")),
            )
            // The status belongs to the refresh button, so it sits against it
            // rather than against the far edge: `Manual refresh · updated
            // 04:44:04 UTC` is one statement, and the reader's eye should not
            // cross the whole panel to pair the two halves of it.
            .child(div().flex_1().min_w(px(0.)))
            .child(
                div()
                    .id("overview-refresh-status")
                    .debug_selector(|| "overview-refresh-status".to_owned())
                    .min_w(px(0.))
                    .role(Role::Status)
                    .aria_label(status_label)
                    .child(label_small(status).color(Color::Muted).truncate()),
            )
            .child(
                div()
                    .flex_none()
                    .debug_selector(|| "overview-refresh".to_owned())
                    .child(
                        Button::new("overview-refresh", "Refresh")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .tab_index(0isize)
                            .track_focus(&self.refresh_focus)
                            .aria_label("Refresh the cluster overview")
                            .tooltip(Tooltip::text("Refresh the cluster data"))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The first paint of the panel, before the snapshot arrives.
    ///
    /// The shared empty state owns the loading affordance, so Overview shows
    /// the same progress bar as every other panel, and a short panel keeps the
    /// bar instead of clipping it.
    fn render_loading(&self) -> AnyElement {
        div()
            .id("overview-loading")
            .size_full()
            .min_h(px(0.))
            .child(empty_state(
                IconName::LoadCircle,
                "Loading overview",
                "Listing nodes, pods, and workloads",
            ))
            .into_any_element()
    }

    fn retry_button(&self, id: &'static str, cx: &Context<Self>) -> AnyElement {
        Button::new(id, "Retry")
            .style(ButtonStyle::Tinted(TintColor::Accent))
            .size(ButtonSize::Medium)
            .tab_index(1isize)
            .track_focus(&self.retry_focus)
            .aria_label("Retry loading cluster overview")
            .on_click(cx.listener(|view, _, _, cx| view.refresh(cx)))
            .into_any_element()
    }

    fn error_hint(reason: &str) -> &'static str {
        if reason == "Not connected to a cluster." {
            "Connect to a cluster, then retry loading the overview."
        } else {
            "Retry, or make sure the cluster connection works."
        }
    }

    fn render_error(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        self.render_failure(
            "overview-error",
            IconName::Warning,
            Severity::Error,
            "Failed to load the overview",
            Self::error_hint(reason),
            reason,
            cx,
        )
    }

    /// The cluster answered, and the answer was "forbidden".
    ///
    /// It gets its own icon, title and hint: telling the user to check the
    /// connection would send them after the wrong thing, because the
    /// connection is fine. The missing permissions are in the hint, so the
    /// state explains itself without a hover; the raw API text stays in the
    /// tooltip.
    fn render_forbidden(&self, overview: &Overview, cx: &Context<Self>) -> AnyElement {
        let detail = source_failure_detail(overview);
        let hint = denial_copy(overview);
        self.render_failure(
            "overview-forbidden",
            IconName::Lock,
            Severity::Warning,
            "Access denied",
            &hint,
            &detail,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_failure(
        &self,
        id: &'static str,
        icon: IconName,
        severity: Severity,
        title: &'static str,
        hint: &str,
        detail: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut panel = v_flex()
            .id(id)
            .debug_selector(|| id.to_owned())
            .size_full()
            .min_h(px(0.))
            .items_center()
            .justify_center()
            .gap(space::SM)
            .px(space::XL)
            .role(Role::Alert)
            .aria_label(format!("{title}. {hint}"))
            .child(
                Icon::new(icon)
                    // The shared empty state in `panels::common` leads with
                    // `design::size::ICON_LARGE`; Zed's `IconSize::XLarge` is
                    // 48px and made this one failure state twice the size of
                    // every other one in the app.
                    .size(IconSize::Custom(rems_from_px(f32::from(
                        design::size::ICON_LARGE,
                    ))))
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(label_text(title))
            .child(label_small(hint).color(Color::Muted))
            .child(self.retry_button("overview-retry", cx));
        if !detail.is_empty() {
            panel
                .interactivity()
                .tooltip(Tooltip::text(detail.to_owned()));
        }
        panel.into_any_element()
    }

    fn render_refresh_error(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let mut notice = h_flex()
            .id("overview-refresh-error")
            .w_full()
            .min_h(design::size::ROW)
            .gap(space::SM)
            .items_center()
            .role(Role::Alert)
            .aria_label("Refresh failed. The previous overview is still shown.")
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Custom(Severity::Error.marker(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(space::XS)
                    .child(label_text("Failed to refresh the overview").color(Color::Default))
                    .child(
                        label_small("The previous snapshot is still shown. Retry, or make sure the cluster connection works.")
                            .color(Color::Muted),
                    ),
            )
            .child(self.retry_button("overview-refresh-retry", cx));
        notice
            .interactivity()
            .tooltip(Tooltip::text(reason.to_owned()));
        notice.into_any_element()
    }

    /// The verdict banner.
    ///
    /// It carries the state and the attention count, and nothing else: the
    /// banner sits inside the section group, so it adds no second border. Its
    /// trailing edge carries the observation answer, which is a different
    /// question and gets its own shape family so the two cannot merge.
    fn render_health(&self, overview: &Overview, cx: &Context<Self>) -> AnyElement {
        let data_state = overview_data_state(overview);
        let severity = health_severity(overview);
        let (title, detail) = health_copy(overview);
        let confidence =
            snapshot_confidence(&self.state, self.refreshing, self.refresh_error.is_some());
        let typography = settings::data_typography(cx);
        let background = health_wash(severity, cx);
        // The rail is solved against the wash it sits on, so the caution reads
        // at 3:1 on the composited background and not only on the canvas.
        let rail = severity.marker_on(cx, background);
        // A verdict the app could not reach is the one case a screen reader must
        // be told about in words, because the severity colour and the glyph
        // beside it are both speaking about health.
        let mut aria = format!("{title}. {detail}");
        if !confidence.is_definite() {
            aria.push_str(". ");
            aria.push_str(design::confidence_label(confidence));
        }
        let mut health = h_flex()
            .id("overview-health")
            .debug_selector(|| "overview-health".to_owned())
            // Full content width, so the banner, the vitals, and the capacity
            // table share one left edge and one right edge. Capping the banner
            // alone left it reading as a clipped fragment floating above a
            // full-width table.
            .w_full()
            .items_stretch()
            .rounded_md()
            .overflow_hidden()
            .bg(background.alpha(1.0))
            .role(Role::Status)
            .aria_label(aria)
            .child(
                div()
                    .flex_none()
                    .w(design::border::TABLE_FOCUS_RAIL)
                    .h_full()
                    .bg(rail),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_start()
                    .gap(space::SM)
                    .py(space::MD)
                    // The same inset the vitals and the table below use, so the
                    // three sections of the group start on one vertical line. At
                    // `MD` the banner text sat 4px left of everything else and
                    // the difference read as a misalignment.
                    .px(space::LG)
                    .child(
                        // The glyph belongs to the title line, so it is offset
                        // by half a line rather than floating between the lines.
                        div().flex_none().mt(space::XS).child(
                            Icon::new(if data_state == OverviewDataState::Forbidden {
                                IconName::Lock
                            } else {
                                health_banner_icon(severity)
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Custom(rail)),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(space::XS)
                            // A section title is a real step above the data
                            // line, so the two no longer read as one sentence.
                            .child(label_section(title))
                            .child(
                                data_text(detail, &typography, Color::Muted, cx)
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .text_ellipsis(),
                            ),
                    ),
            );
        if !confidence.is_definite() {
            health = health.child(confidence_marker(confidence, cx));
        }
        if data_state != OverviewDataState::Complete && overview.access_denied() {
            // The raw API text, including any 403 body, stays in the tooltip.
            let detail = source_failure_detail(overview);
            if !detail.is_empty() {
                health.interactivity().tooltip(Tooltip::text(detail));
            }
        }
        health.into_any_element()
    }

    /// The page's one hero figure, then the ratios under it.
    ///
    /// The panel used to draw three identical cards, which gave the reader three
    /// equal-weight numbers and no answer to "is this cluster fine". It now leads
    /// with the ratio the page exists to communicate at the display size, keeps
    /// the rest in a fixed order, and itemises the workload kinds in a small grid
    /// instead of a `·`-separated run. Nothing here carries a fill, a border, or
    /// a colour of its own: boldness is spent on the health channel above.
    fn render_vitals(&self, overview: &Overview, compact: bool, cx: &Context<Self>) -> AnyElement {
        let typography = settings::data_typography(cx);
        let (hero, signals) = vital_figures(overview);
        let breakdown = workload_breakdown(overview);
        let hero_color = caption_color(hero.severity, cx);
        let hero_block = v_flex()
            .id("overview-hero")
            .debug_selector(|| "overview-hero".to_owned())
            .w_full()
            .min_w(px(0.))
            .gap(space::SM)
            // The label that governs the page's largest number gets the same
            // weight as the other two section headings. It used to be
            // `label_section` *and* muted, so the most important label on the page
            // measured 9:1 while two labels that matter less measured 16:1.
            .child(label_section(hero.label))
            .child(
                display_text(hero.value.clone(), &typography, Color::Default, cx)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis(),
            )
            .child(self.render_pod_caption(&hero, hero_color, cx));
        let mut column = v_flex()
            .id("overview-vitals")
            .debug_selector(|| "overview-vitals".to_owned())
            .w_full()
            .min_w(px(0.))
            // The group inset. The banner and the table below carry the same
            // one, so the panel's text starts on a single vertical line instead
            // of resting on the group's 1px border.
            .px(space::LG)
            .gap(space::LG)
            .role(Role::Group)
            .aria_label(format!("{}: {}", hero.label, hero.detail))
            .child(hero_block);
        for signal in signals {
            column = column.child(signal_row(signal, &typography, cx));
        }
        if !breakdown.is_empty() {
            column = column.child(workload_grid(
                &breakdown,
                if compact {
                    WORKLOAD_COLUMNS_COMPACT
                } else {
                    WORKLOAD_COLUMNS_WIDE
                },
                &typography,
                cx,
            ));
        }
        column.into_any_element()
    }

    /// The hero caption, as the route to the pods it is counting.
    ///
    /// A panel that reports 9,900 pending pods and offers no way to look at them
    /// has reported a problem it cannot act on, and the caption is the one piece
    /// of text on the page that names the bucket. With no host callback it stays
    /// quiet text, and a caption with nothing to follow (`all running`, `No pods
    /// reported`) is never dressed up as a control.
    fn render_pod_caption(
        &self,
        hero: &VitalFigure,
        color: Color,
        cx: &Context<Self>,
    ) -> AnyElement {
        if self.show_problems.is_some() && hero.actionable {
            return div()
                .flex_none()
                .debug_selector(|| "overview-show-problems".to_owned())
                .child(
                    Button::new("overview-show-problems", hero.caption.clone())
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Medium)
                        // The severity stays the caption's own colour: the button
                        // is a way to follow the number, not a different claim.
                        .color(color)
                        // The caption names a bucket, so it is a noun phrase and
                        // cannot be the thing that says what the control does.
                        // `buttons.md > Content` asks a button to communicate its
                        // purpose from what it carries, and `lists-and-tables.md`
                        // asks for a disclosure indicator where a control drills
                        // into a list, so the arrow is the affordance and the
                        // caption keeps the words the page's own figures use. A
                        // verb in the label would have had to be a second string,
                        // printed by the no-callback case where there is nothing
                        // to open.
                        .end_icon(
                            Icon::new(IconName::ArrowRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tab_index(0isize)
                        .aria_label(format!(
                            "Show the pods that need attention: {}",
                            hero.caption
                        ))
                        .tooltip(Tooltip::text("Show these pods in the Pods table"))
                        .on_click(cx.listener(|view, _, window, cx| {
                            if let Some(show_problems) = view.show_problems.clone() {
                                show_problems(window, cx);
                            }
                        })),
                )
                .into_any_element();
        }
        // A caption long enough to be cut keeps its full text in a tooltip, which
        // is where the last of the bucket breakdown lives.
        let mut plain = div()
            .id("overview-pod-caption")
            .min_w(px(0.))
            .overflow_hidden()
            .child(label_small(hero.caption.clone()).color(color).truncate());
        if let Some(tooltip) = caption_tooltip(&hero.caption) {
            plain.interactivity().tooltip(Tooltip::text(tooltip));
        }
        plain.into_any_element()
    }

    fn render_capacity(
        &self,
        overview: &Overview,
        usage: Option<&[NodeUsage]>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let typography_for_rows = settings::data_typography(cx);
        let metrics_available = usage.is_some();
        let (node_status, _) = node_context(overview);
        let mut section = v_flex()
            .id("overview-capacity")
            .role(Role::Group)
            // The group's own accessible name carries the node readiness, so the
            // heading does not repeat it: `1 / 1 ready` appeared three times on
            // one screen, 120px and 260px apart, and every row's accessible name
            // opened with the same cluster-level sentence.
            .aria_label(format!("Node Capacity. {node_status}"))
            .w_full()
            // The group inset the banner and the vitals use. The table's own
            // `space::SM` row padding sits inside this one.
            .px(space::LG)
            .gap(space::SM)
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.))
                    .gap(space::SM)
                    .items_center()
                    .child(label_section("Node Capacity").flex_none())
                    .when(!compact, |this| {
                        this.child(
                            label_small("Requests and limits from scheduled pods")
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            );
        if overview.capacities.is_empty() {
            return section
                .child(
                    label_small(if overview.nodes.count == 0 {
                        "The cluster returned no node data. Make sure the cluster connection works. Refresh the overview."
                    } else {
                        "Node capacity data is unavailable. Refresh, or make sure the cluster connection works."
                    })
                    .color(Color::Muted),
                )
                .into_any_element();
        }
        let column_count = if metrics_available {
            CAPACITY_COLUMNS.len()
        } else {
            CAPACITY_COLUMN_COUNT
        };
        let chrome = capacity_row_chrome(column_count);
        let widths = capacity_column_widths(
            (self.table_available_width(f32::from(window.viewport_size().width)) - chrome).max(0.0),
            metrics_available,
        );
        let table_width = capacity_table_width(&widths) + chrome;
        let (selected_row, selected_column) =
            self.clamped_cursor(overview.capacities.len(), column_count);
        let focused = self.table_focus.is_focused(window);
        let sort = self.capacity_sort;
        let mut header = h_flex()
            .id("overview-capacity-header")
            .role(Role::Row)
            .aria_row_index(1)
            .w(px(table_width))
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(capacity_table_surface(cx))
            .border_b_1()
            .border_color(colors.border_variant);
        for column in 0..column_count {
            header = header.child(self.capacity_header_cell(column, &widths, sort, focused, cx));
        }
        // The table owns its rows, so the virtualised list can build one row
        // without reaching back into the snapshot.
        let rows: Vec<(String, NodeCapacity, Option<NodeUsage>)> = overview
            .capacities
            .iter()
            .map(|capacity| {
                let sample = usage
                    .and_then(|usage| usage.iter().find(|sample| sample.name == capacity.name))
                    .cloned();
                (capacity.name.clone(), capacity.clone(), sample)
            })
            .collect();
        let rows = sorted_capacity_rows(rows, sort);
        let row_backgrounds = (
            design::row_stripe_bg(cx),
            design::row_selected_bg(cx),
            design::row_hover_bg(cx),
        );
        let hover_background = row_backgrounds.2;
        let stripe_background = row_backgrounds.0;
        let selected_background = row_backgrounds.1;
        let focus_border = design::focus::border(cx);
        let count = rows.len();
        let body = uniform_list("overview-capacity-rows", count, {
            let cursor = (selected_row, selected_column);
            move |range, _window, cx| {
                range
                    .filter_map(|row| {
                        let (name, capacity, sample) = rows.get(row)?.clone();
                        Some(capacity_row(
                            row,
                            name,
                            &capacity,
                            sample.as_ref(),
                            &widths,
                            &typography_for_rows,
                            row == cursor.0,
                            focused && row == cursor.0,
                            cursor.1,
                            focused,
                            row % 2 != 0,
                            stripe_background,
                            selected_background,
                            hover_background,
                            focus_border,
                            metrics_available,
                            cx,
                        ))
                    })
                    .collect::<Vec<_>>()
            }
        })
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(0))
        .track_scroll(&self.capacity_scroll)
        .w(px(table_width))
        .h(capacity_body_height(count));
        let table = v_flex()
            .id("overview-capacity-table")
            .debug_selector(|| "overview-capacity-table".to_owned())
            .role(Role::Table)
            .aria_label("Node Capacity")
            .aria_description(CAPACITY_TABLE_DESCRIPTION)
            .aria_keyshortcuts(CAPACITY_TABLE_KEYS)
            .aria_row_count(count + 1)
            .aria_column_count(column_count)
            .w(px(table_width))
            .tab_group()
            .tab_index(CAPACITY_TABLE_TAB_INDEX)
            .track_focus(&self.table_focus)
            .on_key_down(cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                view.move_capacity_cursor(event, count, column_count, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    window.focus(&view.table_focus, cx);
                    cx.stop_propagation();
                }),
            )
            .child(header)
            .child(body);
        section = section.child(
            div()
                .id("overview-capacity-scroll")
                .debug_selector(|| "overview-capacity-scroll".to_owned())
                .w_full()
                .min_w(px(0.))
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .custom_scrollbars(
                    Scrollbars::always_visible(ScrollAxes::Horizontal),
                    window,
                    cx,
                )
                .child(table),
        );
        section.into_any_element()
    }

    /// The three sections inside one group.
    ///
    /// The three sections inside one group.
    ///
    /// The frame and the two rules are two different strengths of the same
    /// structure: `colors.border_variant` draws the outline of one panel, and
    /// `colors.border` separates the sections inside it. They used to be the same
    /// token in all five places, so a 1.12:1 line in light mode was asked to do
    /// both jobs, and the section spacing around it was 8px — smaller than the
    /// 8.5px between a label and its own value, which is how a section boundary
    /// ends up louder than the thing it contains.
    fn render_sections(
        &self,
        overview: &Overview,
        usage: Option<&[NodeUsage]>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        v_flex()
            .id("overview-body")
            .debug_selector(|| "overview-body".to_owned())
            .w_full()
            .gap(space::SM)
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .overflow_hidden()
            .child(self.render_health(overview, cx))
            .child(hairline(cx, "overview-section-rule-1"))
            .child(self.render_vitals(overview, compact, cx))
            .child(hairline(cx, "overview-section-rule-2"))
            .child(self.render_capacity(overview, usage, compact, window, cx))
            .into_any_element()
    }

    /// Width the capacity grid may use.
    ///
    /// The measurement covers the whole panel, so the scroll padding and the
    /// group border come off before the columns are laid out.
    fn table_available_width(&self, window_width: f32) -> f32 {
        let measured = self.content_width.get();
        if !measured.is_finite() || measured <= 0.0 {
            // Before the first measurement the columns keep their minimums.
            return 0.0;
        }
        capacity_available_width(measured, window_width)
    }

    /// Keeps the cursor inside the table after a resize or a new snapshot.
    fn clamped_cursor(&self, row_count: usize, column_count: usize) -> (usize, usize) {
        clamp_cursor(self.capacity_cursor, row_count, column_count)
    }

    fn capacity_header_cell(
        &self,
        column: usize,
        widths: &[f32],
        sort: Sort,
        table_focused: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        capacity_header_cell(
            column,
            *widths.get(column).unwrap_or(&0.0),
            sort,
            table_focused && self.capacity_cursor.1 == column,
            cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.sort_by_column(column, cx);
            }),
            cx,
        )
    }

    /// Sorts by one column, or reverses an already sorted column.
    ///
    /// A third click returns to the node-name order, the order the table opens
    /// in, so the user always lands on a real sort.
    fn sort_by_column(&mut self, column: usize, cx: &mut Context<Self>) {
        self.capacity_sort = next_capacity_sort(self.capacity_sort, column);
        self.capacity_cursor.1 = column;
        cx.notify();
    }

    fn move_capacity_cursor(
        &mut self,
        event: &KeyDownEvent,
        row_count: usize,
        column_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let whole_table = event.keystroke.modifiers.control;
        if event.keystroke.modifiers.alt || event.keystroke.modifiers.platform {
            return;
        }
        let from = self.capacity_cursor;
        let Some(next) = next_cursor(
            from,
            event.keystroke.key.as_str(),
            row_count,
            column_count,
            whole_table,
        ) else {
            return;
        };
        if next == from {
            return;
        }
        self.capacity_cursor = next;
        // The list holds data rows only, so the cursor index is the item index.
        self.capacity_scroll
            .scroll_to_item(next.0, ScrollStrategy::Nearest);
        window.focus(&self.table_focus, cx);
        cx.notify();
    }
}

/// One ratio below the hero: label and caption on the leading side, figure on the
/// trailing side.
///
/// The figure is the only thing the row is read for, so it sits on the row's
/// trailing edge where a column of figures lines up, and the label and caption
/// share the leading side. An empty caption takes no slot at all: the ratio
/// already reads `1 / 1`, and `all ready` beside it repeated the number in words
/// without adding anything. A caption long enough to be truncated keeps a
/// tooltip.
fn signal_row(
    signal: SignalFigure,
    typography: &DataTypography,
    cx: &gpui::App,
) -> gpui::Stateful<gpui::Div> {
    let SignalFigure {
        label,
        value,
        caption,
        detail,
        severity,
    } = signal;
    let color = caption_color(severity, cx);
    let tooltip = caption_tooltip(&caption);
    let mut row = h_flex()
        .id(SharedString::from(label))
        .debug_selector(move || format!("overview-signal-{label}"))
        .flex_none()
        .max_w_full()
        .min_h(design::size::ROW)
        .gap(space::SM)
        .items_center()
        .role(Role::Group)
        .aria_label(format!("{label}: {detail}"))
        // Label, caption, and value read as one line. Letting the value sit at
        // the far edge put `Nodes ready` and `1 / 1` roughly 850px apart on a
        // wide window, which is two unconnected fragments rather than one
        // reading. `layout.md > Visual hierarchy` asks that alignment carry the
        // relationship between a label and its figure, so the row is only as
        // wide as what it says.
        .child(
            h_flex()
                .flex_none()
                .max_w_full()
                .gap(space::SM)
                .items_baseline()
                .child(label_text(label).flex_none())
                // The caption needs a parent that can shrink. Its wrapper used
                // to be `flex_none`, so `text-overflow: ellipsis` had nothing to
                // shrink against: the caption either stretched to its content
                // width or was cut by `max_w_full` with no ellipsis at all.
                .when(!caption.is_empty(), |this| {
                    this.child(
                        div().flex_1().min_w(px(0.)).child(
                            label_small(caption)
                                .color(color)
                                .truncate()
                                .into_any_element(),
                        ),
                    )
                }),
        )
        .child(
            data_text(value, typography, Color::Default, cx)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .flex_none()
                .min_w(px(0.)),
        );
    if let Some(tooltip) = tooltip {
        row.interactivity().tooltip(Tooltip::text(tooltip));
    }
    row
}

/// The per-kind workload figures as a small grid.
///
/// A cell is as wide as what it says. `flex_1` gave each of the five cells a
/// third of a 1594px panel to hold about 65px of text, so two adjacent numbers
/// sat 600px apart and the eye had to travel to compare them. The left rail marks
/// the grid as the itemisation of the `Workloads ready` row above it, which the
/// shared left edge alone did not say.
fn workload_grid(
    cells: &[(&'static str, Option<String>)],
    columns: usize,
    typography: &DataTypography,
    cx: &gpui::App,
) -> AnyElement {
    let mut slots = cells
        .iter()
        .cloned()
        .map(Some)
        .chain(std::iter::repeat(None));
    let rows = cells.len().div_ceil(columns);
    let mut grid = v_flex()
        .id("overview-workload-grid")
        .debug_selector(|| "overview-workload-grid".to_owned())
        .w_full()
        .min_w(px(0.))
        .gap(space::SM)
        .border_l_1()
        .border_color(cx.theme().colors().border)
        .pl(space::SM);
    for _ in 0..rows {
        let row = (0..columns)
            .map(|_| slots.next().flatten())
            .collect::<Vec<_>>();
        grid = grid.child(h_flex().w_full().gap(space::SM).items_stretch().children(
            row.into_iter().map(|cell| {
                match cell {
                    Some((label, figure)) => {
                        // A kind the cluster refused has no figure at all, and
                        // that is a different statement from a kind with nothing
                        // in it. The dash says both are absent and the hollow
                        // mark says which one the app is responsible for; the
                        // whole-strength ink is reserved for a real reading.
                        let unknown = figure.is_none();
                        let figure = figure.unwrap_or_else(|| NO_ANSWER.to_owned());
                        let color = if unknown {
                            Color::Custom(design::confidence::foreground(
                                design::Confidence::Unknown,
                                cx,
                            ))
                        } else if is_no_answer(&figure) {
                            Color::Muted
                        } else {
                            Color::Default
                        };
                        v_flex()
                            .min_w(px(0.))
                            .gap(space::XS)
                            .debug_selector(move || {
                                format!("overview-workload-cell-{}", label.replace(' ', "-"))
                            })
                            .child(label_small(label).color(Color::Muted))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(space::XS)
                                    .min_w(px(0.))
                                    .when(unknown, |this| {
                                        this.child(
                                            div()
                                                .flex_none()
                                                .debug_selector(move || {
                                                    format!(
                                                        "overview-workload-unknown-{}",
                                                        label.replace(' ', "-")
                                                    )
                                                })
                                                .child(
                                                    Icon::new(design::confidence::icon(
                                                        design::Confidence::Unknown,
                                                    ))
                                                    .size(IconSize::XSmall)
                                                    .color(color),
                                                ),
                                        )
                                    })
                                    .child(
                                        data_text(figure, typography, color, cx)
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap(),
                                    ),
                            )
                    }
                    None => div().min_w(px(0.)),
                }
            }),
        ));
    }
    grid.into_any_element()
}

/// A single structural line between two sections of the group.
///
/// `colors.border`, not `border_variant`: the frame around the group already
/// uses the quieter role, and one token asked to draw both left the two
/// indistinguishable — 1.12:1 in light mode, where the frame was effectively not
/// there.
fn hairline(cx: &gpui::App, id: &'static str) -> gpui::Div {
    div()
        .debug_selector(move || id.to_owned())
        .flex_none()
        .w_full()
        .h(design::border::LINE)
        .bg(cx.theme().colors().border)
}

/// Sort order after a click on a column heading.
///
/// The node column is the one the table opens on, so a third click lands
/// there and the table always comes back to a real sort.
fn next_capacity_sort(current: Sort, column: usize) -> Sort {
    match (current.column == column, current.descending) {
        (true, false) => Sort::descending(column),
        (true, true) => Sort::ascending(0),
        (false, _) => Sort::ascending(column),
    }
}

/// The value a column sorts by, or `None` when the value is unknown.
fn capacity_sort_value(
    capacity: &NodeCapacity,
    sample: Option<&NodeUsage>,
    column: usize,
) -> Option<f64> {
    match column {
        1 => Some(capacity.requested_cpu),
        2 => Some(capacity.requested_memory),
        3 => Some(capacity.limits_cpu),
        4 => Some(capacity.limits_memory),
        // The live columns sort by the reading they show, which is the value a
        // user compares when looking for the busiest node.
        5 => sample.and_then(|sample| sample.cpu_millicores),
        6 => sample.and_then(|sample| sample.memory_bytes),
        _ => None,
    }
}

/// Orders the table rows by the sorted column.
///
/// The node column orders by name. Every other column orders by its value, and
/// an unknown value sorts last, so real data leads.
fn sorted_capacity_rows(
    rows: Vec<(String, NodeCapacity, Option<NodeUsage>)>,
    sort: Sort,
) -> Vec<(String, NodeCapacity, Option<NodeUsage>)> {
    let column = sort.column;
    let mut rows = rows;
    if column == 0 {
        // The node column sorts by name. The snapshot arrives in the order the
        // API listed the nodes, so leaving that order in place would show an
        // ascending arrow over rows it is not ascending.
        rows.sort_by(|left, right| {
            let ordering = left.0.cmp(&right.0);
            if sort.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        return rows;
    }
    rows.sort_by(|left, right| {
        let ordering = match (
            capacity_sort_value(&left.1, left.2.as_ref(), column),
            capacity_sort_value(&right.1, right.2.as_ref(), column),
        ) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        };
        // Equal values keep the node-name order, so a sort is stable.
        let ordering = ordering.then_with(|| left.0.cmp(&right.0));
        if sort.descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    rows
}

fn clamp_cursor(cursor: (usize, usize), row_count: usize, column_count: usize) -> (usize, usize) {
    if row_count == 0 || column_count == 0 {
        return (0, 0);
    }
    (cursor.0.min(row_count - 1), cursor.1.min(column_count - 1))
}

/// The next cell for a key press, or `None` when the key is not ours.
fn next_cursor(
    cursor: (usize, usize),
    key: &str,
    row_count: usize,
    column_count: usize,
    whole_table: bool,
) -> Option<(usize, usize)> {
    if row_count == 0 || column_count == 0 {
        return None;
    }
    let (row, column) = cursor;
    let next = match key {
        "left" => (row, column.saturating_sub(1)),
        "right" => (row, (column + 1).min(column_count - 1)),
        "up" => (row.saturating_sub(1), column),
        "down" => ((row + 1).min(row_count - 1), column),
        "home" if whole_table => (0, 0),
        "home" => (row, 0),
        "end" if whole_table => (row_count - 1, column_count - 1),
        "end" => (row, column_count - 1),
        "pageup" => (row.saturating_sub(10), column),
        "pagedown" => ((row + 10).min(row_count - 1), column),
        _ => return None,
    };
    Some(next)
}

/// Viewport height for the virtualised rows: the table grows until it would
/// push the rest of the page off a 640px-tall window, then it scrolls.
fn capacity_body_height(row_count: usize) -> gpui::Pixels {
    let row_height = f32::from(design::size::ROW);
    let visible = (row_count as f32).min(CAPACITY_TABLE_MAX_ROWS);
    px(visible * row_height)
}

fn capacity_header_cell(
    column: usize,
    width: f32,
    sort: Sort,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    cx: &gpui::App,
) -> AnyElement {
    let label = CAPACITY_COLUMNS[column];
    let sorted = sort.column == column;
    let direction = if sort.descending {
        "descending"
    } else {
        "ascending"
    };
    let aria = if sorted {
        format!("{label}, sorted {direction}")
    } else {
        label.to_owned()
    };
    let description = if sorted {
        format!("{label} is sorted {direction}. Activate to reverse it.")
    } else {
        format!("{label}. Activate to sort by it.")
    };
    let mut cell = h_flex()
        .id(("overview-capacity-header-cell", column))
        .debug_selector(move || format!("overview-capacity-header-cell-{column}"))
        .role(Role::ColumnHeader)
        .aria_label(aria)
        .aria_description(description.clone())
        .aria_column_index(column + 1)
        .flex_none()
        .w(px(width))
        .min_w(px(0.))
        .h(design::size::ROW)
        .gap(space::XS)
        .items_center()
        // Header alignment follows the data it names. Every numeric column is
        // right-aligned below, and the headings used to hug the left edge of
        // their own cells, which left 36px to 88px of dead space between a
        // heading and the number it describes.
        .when(column > 0, |this| this.justify_end())
        .cursor_pointer()
        .font_ui(cx)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        // One step above the muted it used to be: at metadata size the muted
        // header read as disabled beside the data. The cursor column takes the
        // accent, so the two states stay distinct without a second size.
        .text_color(if active {
            cx.theme().colors().text_accent
        } else {
            cx.theme().colors().text
        })
        .overflow_hidden()
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .debug_selector(move || format!("overview-capacity-header-label-{column}"))
                .child(label),
        );
    if sorted {
        cell = cell.child(
            Icon::new(if sort.descending {
                IconName::ArrowDown
            } else {
                IconName::ArrowUp
            })
            .size(IconSize::XSmall)
            .color(Color::Accent),
        );
    }
    if active {
        cell = cell.aria_selected(true);
    }
    cell.tooltip(Tooltip::text(description))
        .on_click(on_click)
        .into_any_element()
}

/// One node row of the capacity table.
#[allow(clippy::too_many_arguments)]
fn capacity_row(
    row: usize,
    name: String,
    capacity: &NodeCapacity,
    sample: Option<&NodeUsage>,
    widths: &[f32],
    typography: &DataTypography,
    cursor_row: bool,
    focused_cursor: bool,
    cursor_column: usize,
    focused: bool,
    striped: bool,
    stripe_background: gpui::Hsla,
    selected_background: gpui::Hsla,
    hover_background: gpui::Hsla,
    focus_border: gpui::Hsla,
    metrics_available: bool,
    cx: &gpui::App,
) -> AnyElement {
    let colors = cx.theme().colors();
    let selected = focused_cursor;
    let background = if selected {
        selected_background
    } else if striped {
        stripe_background
    } else {
        capacity_table_surface(cx)
    };
    let table_width = capacity_table_width(widths) + capacity_row_chrome(widths.len());
    let (cpu_color, cpu_severity) =
        oversubscribed(capacity.requested_cpu, capacity.allocatable_cpu);
    let (memory_color, memory_severity) =
        oversubscribed(capacity.requested_memory, capacity.allocatable_memory);
    let summary = capacity_summary(capacity, sample);
    let mut element = h_flex()
        .id(("overview-capacity-row", row))
        .role(Role::Row)
        .aria_row_index(row + 2)
        .aria_selected(focused && cursor_row)
        // The row's own name leads. It used to be prefixed with the cluster-level
        // `99/100 ready · 1 not ready`, so on a 100-node cluster all 100 rows
        // announced the same sentence and not one of them said anything about
        // its own node. The group's accessible name already carries that.
        .aria_label(summary.clone())
        .w(px(table_width))
        .h(design::size::ROW)
        .px(space::SM)
        .gap(space::SM)
        .items_center()
        .relative()
        .bg(background)
        .border_b_1()
        .border_color(colors.border_variant)
        .child(plain_capacity_cell(
            name,
            widths.first().copied().unwrap_or_default(),
            row,
            0,
            typography,
            if focused_cursor {
                Color::Custom(colors.text)
            } else {
                Color::Default
            },
            false,
            focused && cursor_row && cursor_column == 0,
            cx,
        ))
        .child(capacity_cell(
            request_figure(
                capacity.requested_cpu,
                capacity.allocatable_cpu,
                format_cores,
                |cores| format!("{} cores", format_cores(cores)),
            ),
            // The cell prints the pair, so the share of allocatable and the word
            // "oversubscribed" have to be somewhere a reader and a screen reader
            // can reach. Both live in the cell's spoken label, which the tooltip
            // repeats, instead of pushing the visible value past its column.
            Some(request_text(
                capacity.requested_cpu,
                capacity.allocatable_cpu,
                format_cores,
                |cores| format!("{} cores", format_cores(cores)),
            )),
            cpu_severity,
            widths.get(1).copied().unwrap_or_default(),
            row,
            1,
            typography,
            cpu_color,
            true,
            focused && cursor_row && cursor_column == 1,
            cx,
        ))
        .child(capacity_cell(
            request_figure(
                capacity.requested_memory,
                capacity.allocatable_memory,
                format_bytes,
                format_bytes,
            ),
            Some(request_text(
                capacity.requested_memory,
                capacity.allocatable_memory,
                format_bytes,
                format_bytes,
            )),
            memory_severity,
            widths.get(2).copied().unwrap_or_default(),
            row,
            2,
            typography,
            memory_color,
            true,
            focused && cursor_row && cursor_column == 2,
            cx,
        ))
        .child(plain_capacity_cell(
            format_limit(capacity.limits_cpu, format_cores),
            widths.get(3).copied().unwrap_or_default(),
            row,
            3,
            typography,
            Color::Muted,
            true,
            focused && cursor_row && cursor_column == 3,
            cx,
        ))
        .child(plain_capacity_cell(
            format_limit(capacity.limits_memory, format_bytes),
            widths.get(4).copied().unwrap_or_default(),
            row,
            4,
            typography,
            Color::Muted,
            true,
            focused && cursor_row && cursor_column == 4,
            cx,
        ));
    if metrics_available {
        element = element
            .child(plain_capacity_cell(
                format_cpu_now(sample, capacity, metrics_available),
                widths.get(5).copied().unwrap_or_default(),
                row,
                5,
                typography,
                Color::Muted,
                true,
                focused && cursor_row && cursor_column == 5,
                cx,
            ))
            .child(plain_capacity_cell(
                format_memory_now(sample, capacity, metrics_available),
                widths.get(6).copied().unwrap_or_default(),
                row,
                6,
                typography,
                Color::Muted,
                true,
                focused && cursor_row && cursor_column == 6,
                cx,
            ));
    }
    if focused_cursor {
        element = element.child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(design::border::TABLE_FOCUS_RAIL)
                .bg(focus_border),
        );
    }
    let mut row = if selected {
        element
    } else {
        element.hover(move |style| style.bg(hover_background))
    };
    // The row is the only element that knows the blended share of allocatable:
    // the two request cells each carry one axis, and the per-axis numbers do not
    // add up to it. So the sentence a screen reader gets is also the sentence a
    // pointer gets. A cell's own tooltip still wins over this one — it is the
    // inner element — which is the right order: the cell is the narrower question.
    // `interactivity()` hands back the element and `tooltip` mutates in place, so
    // this is a statement rather than a link in a chain.
    row.interactivity().tooltip(Tooltip::text(summary));
    row.into_any_element()
}

/// The colour and the severity of a request figure above allocatable.
///
/// The two travel together because a state that lives only in a colour is a
/// state a screen reader never receives and a greyscale print never shows. With
/// the severity, the cell draws a glyph from the shared health vocabulary; the
/// words come from [`request_text`], which is what the cell's accessible name
/// and tooltip carry.
fn oversubscribed(requested: f64, allocatable: Option<f64>) -> (Color, Option<Severity>) {
    if request_ratio(requested, allocatable).is_some_and(|pct| pct > 100.0) {
        (Color::Warning, Some(Severity::Warning))
    } else {
        (Color::Muted, None)
    }
}

/// One capacity cell that carries no state of its own.
#[allow(clippy::too_many_arguments)]
fn plain_capacity_cell(
    text: String,
    width: f32,
    row: usize,
    column: usize,
    typography: &DataTypography,
    color: Color,
    numeric: bool,
    cursor: bool,
    cx: &gpui::App,
) -> AnyElement {
    capacity_cell(
        text, None, None, width, row, column, typography, color, numeric, cursor, cx,
    )
}

/// One capacity cell: the value, and the state it is in.
///
/// `spoken` is what a screen reader and the tooltip hear. It is the same text as
/// the cell shows unless the cell is stating something the figure cannot, which
/// is what makes `1.15 / 20 cores` answer "is this node oversubscribed" for
/// everyone and not only for the reader who can see amber.
#[allow(clippy::too_many_arguments)]
fn capacity_cell(
    text: String,
    spoken: Option<String>,
    severity: Option<Severity>,
    width: f32,
    row: usize,
    column: usize,
    typography: &DataTypography,
    color: Color,
    numeric: bool,
    cursor: bool,
    cx: &gpui::App,
) -> AnyElement {
    let spoken = spoken.unwrap_or_else(|| text.clone());
    let mut cell = div()
        .id((
            "overview-capacity-cell",
            ((row as u64) << 16) | column as u64,
        ))
        .debug_selector(move || format!("overview-capacity-cell-{row}-{column}"))
        .role(Role::Cell)
        .aria_label(spoken.clone())
        .aria_column_index(column + 1)
        .flex_none()
        .w(px(width))
        .min_w(px(0.))
        .items_center()
        .gap(space::XS)
        .when(numeric, |this| this.justify_end());
    if let Some(severity) = severity {
        // The glyph is the half of the state that survives greyscale. Amber ink
        // alone is not a verdict: `color.md > Inclusive color` asks for a second
        // channel, and the shape is the one this app already owns.
        cell = cell.child(
            div()
                .flex_none()
                .debug_selector(move || format!("overview-capacity-severity-{row}-{column}"))
                .child(
                    Icon::new(design::health_icon(severity))
                        .size(IconSize::XSmall)
                        .color(Color::Custom(severity.marker(cx))),
                ),
        );
    }
    let mut cell = cell.child(
        data_text(text, typography, color, cx)
            .min_w(px(0.))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis(),
    );
    if cursor {
        // The cursor cell steps up to the selected surface, the same way the
        // sample table marks its cursor column.
        cell = cell.bg(design::surface::selected(cx));
    }
    cell.tooltip(Tooltip::text(spoken)).into_any_element()
}

impl Render for OverviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let measured_width = self.content_width.get();
        let window_width = f32::from(window.viewport_size().width);
        // The measurement lands a frame after it is taken, so a window that has
        // just shrunk still reports the width it had. The window bounds it, or
        // the panel would lay out for a size it no longer has.
        let available_width = if measured_width.is_finite() && measured_width > 0.0 {
            measured_width.min(window_width)
        } else {
            window_width
        };
        let compact = overview_is_compact(available_width);
        let body: AnyElement = match &self.state {
            OverviewState::Loading => self.render_loading(),
            OverviewState::Failed(reason) => self.render_error(reason, cx),
            OverviewState::Ready(overview) => match overview_data_state(overview) {
                OverviewDataState::Forbidden => self.render_forbidden(overview, cx),
                OverviewDataState::Error => self.render_error(&source_failure_detail(overview), cx),
                _ => {
                    let usage = overview.usage.clone();
                    let content =
                        self.render_sections(overview, usage.as_deref(), compact, window, cx);
                    match self.refresh_error.as_deref() {
                        Some(reason) => v_flex()
                            .id("overview-ready-with-error")
                            .w_full()
                            .gap(space::LG)
                            .child(self.render_refresh_error(reason, cx))
                            .child(content)
                            .into_any_element(),
                        None => content,
                    }
                }
            },
        };
        let measured_width = self.content_width.clone();
        let panel = cx.entity().downgrade();
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .on_children_prepainted(move |children, window, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let width = f32::from(bounds.size.width);
                if !width.is_finite() || width <= 0.0 {
                    return;
                }
                let previous = measured_width.get();
                // A drag moves the edge a few pixels a frame, and a change
                // that small is not worth a layout. A bigger change is a
                // resize or a new window, and it is measured at once: a width
                // the panel no longer has is what makes the capacity table
                // overflow at the minimum window size, and a dropped
                // measurement is never taken again, so the table would keep
                // that width until the next resize.
                if previous > 0.0 && (previous - width).abs() <= WIDTH_MEASURE_STEP {
                    return;
                }
                measured_width.set(width);
                let panel = panel.clone();
                window.defer(cx, move |_, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |_, cx| cx.notify());
                    }
                });
            })
            .id("overview-content-bounds")
            .debug_selector(|| "overview-content-bounds".to_owned())
            .child(
                div()
                    .id("overview-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(space::LG)
                    .child(body),
            );
        let colors = cx.theme().colors();
        v_flex()
            .id("overview-view")
            .role(Role::Region)
            .aria_label("Cluster Overview")
            .size_full()
            .min_w(px(0.))
            .bg(design::surface::canvas(cx).alpha(1.0))
            .text_color(colors.text)
            .font_ui(cx)
            .child(self.render_toolbar(cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use k8s_core::overview::{HealthSummary, NodeSummary, UnavailableSource};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use theme::LoadThemes;

    struct PendingUntilDropped(Arc<AtomicBool>);

    impl Future for PendingUntilDropped {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    struct SizedOverview {
        view: gpui::Entity<OverviewView>,
        width: gpui::Pixels,
        height: gpui::Pixels,
    }

    impl gpui::Render for SizedOverview {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            div().w(self.width).h(self.height).child(self.view.clone())
        }
    }

    #[gpui::test]
    fn default_focus_targets_refresh(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();

        let refresh = view.read_with(cx, |view, _| view.focus_default_control());
        assert!(refresh.tab_stop);
        assert_eq!(refresh, view.read_with(cx, |view, _| view.focus_handle()));
        cx.update(|window, cx| window.focus(&refresh, cx));
        assert!(cx.update(|window, _| refresh.is_focused(window)));

        let bounds = cx
            .debug_bounds("overview-refresh")
            .expect("refresh control");
        assert_eq!(f32::from(bounds.size.height), 28.0);

        // The toolbar sits on the shared 40px rhythm, and the table is the
        // next tab stop after the refresh control.
        let toolbar = cx
            .debug_bounds("overview-toolbar")
            .expect("overview toolbar");
        assert_eq!(f32::from(toolbar.size.height), 40.0);
        let table = view.read_with(cx, |view, _| view.table_focus.clone());
        assert!(table.tab_stop);
        assert!(table.tab_index > refresh.tab_index);
    }

    /// The minimum supported window is 960x640, and the centre panel is narrower
    /// than that once the sidebar takes its share.
    ///
    /// The guard used to assert "the table fits the panel", on a fixture whose
    /// `usage` was `None`: the panel then computes five columns, whose minimum
    /// grid is 652px plus 64 of chrome, and the assertion passed at 960 without
    /// ever building the seven-column grid a real cluster with metrics-server
    /// installed produces. Seven columns of real figures do not fit a 960px
    /// window, so the promise this test keeps is the one that can be true: the
    /// real grid renders, and its last column is reachable through a visible
    /// horizontal scroll rather than clipped away.
    #[gpui::test]
    fn the_capacity_grid_reaches_every_column_at_the_minimum_window_and_takes_the_keyboard(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, true, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(overview_with_metrics(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 3,
                    ready: 3,
                    not_ready: 0,
                },
            ));
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        cx.run_until_parked();

        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("overview panel");
        let table = cx
            .debug_bounds("overview-capacity-table")
            .expect("capacity table");
        let scroll = cx
            .debug_bounds("overview-capacity-scroll")
            .expect("capacity scroll container");
        let minimum: f32 = CAPACITY_COLUMN_MIN_WIDTHS.iter().sum();
        let grid = minimum + capacity_row_chrome(CAPACITY_COLUMN_MIN_WIDTHS.len());
        assert!(
            (f32::from(table.size.width) - grid).abs() < 1.0,
            "metrics are available, so all seven columns render: table {}px, minimum grid {grid}px",
            f32::from(table.size.width)
        );
        assert!(
            f32::from(scroll.size.width) <= f32::from(panel.size.width) + 0.5,
            "the scroll container stays inside the panel: scroll {:?} panel {:?}",
            scroll.size.width,
            panel.size.width
        );
        assert!(
            right_edge(table) > right_edge(scroll),
            "the last column is past the panel edge, so the always-visible scrollbar is what reaches it"
        );
        assert!(
            right_edge(scroll) <= right_edge(panel) + 0.5,
            "the scroll container is the panel's own width, not something wider"
        );
        // The narrowest centre panel the shell can build keeps the same grid, so
        // the four columns that say whether a node is in trouble are never
        // dropped: they scroll.
        assert!(
            grid > f32::from(design::size::CENTER_MIN),
            "a centre panel at its floor is narrower than the grid, which is why the grid scrolls"
        );
        // The guard above is only worth anything because this fixture carries node
        // metrics. A snapshot without them computes five columns, so the same
        // assertion would be measuring a table that no cluster with
        // metrics-server produces.
        let available =
            capacity_available_width(f32::from(panel.size.width), f32::from(panel.size.width));
        let without_metrics = capacity_column_widths(
            (available - capacity_row_chrome(CAPACITY_COLUMN_COUNT)).max(0.0),
            false,
        );
        assert_eq!(without_metrics.len(), CAPACITY_COLUMN_COUNT);
        let short_grid =
            capacity_table_width(&without_metrics) + capacity_row_chrome(CAPACITY_COLUMN_COUNT);
        assert!(
            (short_grid - f32::from(table.size.width)).abs() > 1.0,
            "a metrics-free fixture measures {short_grid}px against the real {grid}px, so the \
             seven-column assertion above fails on it: {}px",
            f32::from(table.size.width)
        );

        let focus = view.read_with(cx, |view, _| view.table_focus.clone());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.capacity_cursor), (0, 0));
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.capacity_cursor),
            (2, 0),
            "the cursor stops at the last node"
        );
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.capacity_cursor), (2, 1));
    }

    #[gpui::test]
    fn compact_layout_uses_the_overview_panel_width(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, false, cx));
            let data = overview(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            );
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(data);
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(480.),
                height: px(640.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("overview content bounds");
        let vitals = cx.debug_bounds("overview-vitals").expect("overview vitals");
        assert!((f32::from(panel.size.width) - 480.).abs() <= 1.);
        assert!(f32::from(vitals.size.height) > 80.);
    }

    #[tokio::test]
    async fn dropping_overview_request_aborts_inner_task() {
        let dropped = Arc::new(AtomicBool::new(false));
        let handle = Handle::current();
        let request = join_abortable(&handle, PendingUntilDropped(Arc::clone(&dropped)));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), request)
                .await
                .is_err()
        );
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn formats_cores_and_bytes_for_capacity_rows() {
        assert_eq!(format_cores(2.0), "2");
        assert_eq!(format_cores(0.76), "0.76");
        assert_eq!(format_bytes(1024.0), "1 KiB");
        assert_eq!(format_bytes(1536.0 * 1024.0), "1.5 MiB");
        assert_eq!(format_bytes(2.0 * 1024.0 * 1024.0 * 1024.0), "2 GiB");
        assert_eq!(format_bytes(512.0), "512 B");
    }

    #[test]
    fn stale_overview_update_does_not_replace_newer_result() {
        let mut state = OverviewState::Loading;
        let mut refreshing = true;
        let mut refresh_error = None;
        let mut current = Overview::default();
        current.nodes.ready = 2;

        assert!(
            apply_refresh_result(
                &mut state,
                &mut refreshing,
                &mut refresh_error,
                2,
                2,
                Ok(current),
            )
            .is_some()
        );
        assert!(
            apply_refresh_result(
                &mut state,
                &mut refreshing,
                &mut refresh_error,
                2,
                1,
                Err("stale session".to_owned()),
            )
            .is_none()
        );
        assert!(!refreshing);

        match state {
            OverviewState::Ready(overview) => assert_eq!(overview.nodes.ready, 2),
            _ => panic!("newer overview must remain ready"),
        }
    }

    fn overview(health: HealthSummary, nodes: NodeSummary, level: HealthLevel) -> Overview {
        let mut overview = Overview {
            health,
            nodes,
            ..Overview::default()
        };
        overview.capacities = (0..overview.nodes.count)
            .map(|index| NodeCapacity {
                name: format!("worker-{index}"),
                allocatable_cpu: Some(4.0),
                allocatable_memory: Some(8.0 * 1024.0 * 1024.0 * 1024.0),
                requested_cpu: 0.0,
                requested_memory: 0.0,
                limits_cpu: 0.0,
                limits_memory: 0.0,
                utilization_pct: Some(0.0),
            })
            .collect();
        if level == HealthLevel::Error {
            overview.health.failed = 1;
        }
        overview
    }

    /// A snapshot from a cluster with metrics-server installed.
    ///
    /// `usage: None` makes the panel compute five columns, which is the state
    /// every layout test used to measure and the state a real cluster is never
    /// in once metrics have been probed.
    fn overview_with_metrics(health: HealthSummary, nodes: NodeSummary) -> Overview {
        let mut overview = overview(health, nodes, HealthLevel::Healthy);
        overview.usage = Some(
            overview
                .capacities
                .iter()
                .map(|capacity| NodeUsage {
                    name: capacity.name.clone(),
                    cpu_millicores: Some(161.0),
                    memory_bytes: Some(3.2 * 1024.0 * 1024.0 * 1024.0),
                })
                .collect(),
        );
        overview
    }

    /// One node, oversubscribed on CPU and comfortable on memory.
    fn oversubscribed_node() -> Overview {
        let mut overview = overview_with_metrics(
            HealthSummary {
                total_pods: 1,
                running: 1,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
        );
        overview.capacities[0].requested_cpu = 4.8;
        overview.capacities[0].requested_memory = 1.0 * 1024.0 * 1024.0 * 1024.0;
        overview.capacities[0].utilization_pct = Some(120.0);
        overview
    }

    fn left_edge(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
        f32::from(bounds.origin.x)
    }

    fn right_edge(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
        f32::from(bounds.origin.x + bounds.size.width)
    }

    /// The columns add up to the width the panel handed them.
    ///
    /// The spare width is shared out in `f32`, so the sum is exact only in real
    /// arithmetic: 308px of spare over five weighted columns leaves the measured
    /// total 0.00006px off the width it came out of. That is a rounding artefact
    /// rather than a gap in the row, so the bound is a hundredth of a pixel.
    #[track_caller]
    fn assert_columns_fill(widths: &[f32], expected: f32) {
        let total = capacity_table_width(widths);
        assert!(
            (total - expected).abs() < 0.01,
            "the columns fill the width they were given: {total}px of {expected}px"
        );
    }

    /// Column-heading boxes, in column order.
    const HEADER_CELLS: [&str; 7] = [
        "overview-capacity-header-cell-0",
        "overview-capacity-header-cell-1",
        "overview-capacity-header-cell-2",
        "overview-capacity-header-cell-3",
        "overview-capacity-header-cell-4",
        "overview-capacity-header-cell-5",
        "overview-capacity-header-cell-6",
    ];

    /// The label inside each heading, which is what text alignment moves.
    const HEADER_LABELS: [&str; 7] = [
        "overview-capacity-header-label-0",
        "overview-capacity-header-label-1",
        "overview-capacity-header-label-2",
        "overview-capacity-header-label-3",
        "overview-capacity-header-label-4",
        "overview-capacity-header-label-5",
        "overview-capacity-header-label-6",
    ];

    /// The first data row's cells, in column order.
    const ROW_CELLS: [&str; 7] = [
        "overview-capacity-cell-0-0",
        "overview-capacity-cell-0-1",
        "overview-capacity-cell-0-2",
        "overview-capacity-cell-0-3",
        "overview-capacity-cell-0-4",
        "overview-capacity-cell-0-5",
        "overview-capacity-cell-0-6",
    ];

    /// One glance has to answer "is this cluster healthy and what needs
    /// attention". That means the banner and the figures must not print the same
    /// number twice, and the visible caption has to be one phrase rather than a
    /// run of counts joined by dots.
    #[test]
    fn the_banner_gives_the_verdict_and_the_figures_give_the_numbers() {
        let mut healthy = overview(
            HealthSummary {
                total_pods: 12,
                running: 12,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 3,
                ready: 3,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        healthy.workloads.deployments = ReplicaSummary {
            desired: 6,
            available: 6,
        };
        let (title, detail) = health_copy(&healthy);
        assert_eq!(title, "Cluster healthy");
        assert_eq!(detail, "Nothing needs attention.");
        assert_eq!(health_severity(&healthy), Severity::Success);

        let (hero, signals) = vital_figures(&healthy);
        // The hero is the ratio the page exists to communicate, and the rest keep
        // a fixed order under it.
        assert_eq!(hero.label, "Pods ready");
        assert_eq!(hero.value, "12 / 12");
        assert_eq!(hero.caption, "all running");
        assert_eq!(
            signals
                .iter()
                .map(|signal| (signal.label, signal.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("Nodes ready", "3 / 3"), ("Workloads ready", "6 / 6")],
            "the secondary ratios never reorder between clusters"
        );
        assert_eq!(signals[0].caption, "");
        assert_eq!(signals[1].caption, "all replicas ready");
        let printed: Vec<(&str, &str, &str)> =
            std::iter::once((hero.label, hero.value.as_str(), hero.caption.as_str()))
                .chain(
                    signals.iter().map(|signal| {
                        (signal.label, signal.value.as_str(), signal.caption.as_str())
                    }),
                )
                .collect();
        for (label, value, caption) in printed {
            assert!(
                !detail.contains(value),
                "the banner repeats the {label} figure {value:?}"
            );
            // An empty caption is the row saying nothing, and every string
            // contains it.
            if !caption.is_empty() {
                assert!(
                    !detail.contains(caption),
                    "the banner repeats the {label} caption {caption:?}"
                );
            }
        }

        // With problems the banner counts them and the figure itemises them.
        let mut degraded = healthy.clone();
        degraded.nodes.not_ready = 1;
        degraded.nodes.ready = 2;
        degraded.health.pending = 1;
        degraded.health.running = 11;
        let (title, detail) = health_copy(&degraded);
        assert_eq!(title, "Cluster needs attention");
        assert_eq!(detail, "1 node not ready, 1 pod not running.");
        let (hero, signals) = vital_figures(&degraded);
        assert_eq!(signals[0].value, "2 / 3");
        assert_eq!(signals[0].caption, "1 not ready");
        assert_eq!(hero.value, "11 / 12");
        assert_eq!(
            hero.caption, "1 pending",
            "the caption names the bucket the ratio does not"
        );
        assert!(
            !hero.caption.contains('·'),
            "a run of counts joined by dots is a sentence: {:?}",
            hero.caption
        );
        assert_eq!(
            hero.detail, "Pods running 11 / 12, 1 pending, 0 failed",
            "the full breakdown is still available in words"
        );
        assert_eq!(signals[0].severity, Severity::Warning);
        assert_eq!(hero.severity, Severity::Warning);
        for caption in [hero.caption.as_str()].into_iter().chain(
            signals
                .iter()
                .map(|signal| signal.caption.as_str())
                .filter(|caption| !caption.is_empty()),
        ) {
            assert!(
                !detail.contains(caption),
                "the banner repeats the caption {caption:?}"
            );
        }

        // A failed pod is an error, and the state says so in words.
        degraded.health.pending = 0;
        degraded.health.failed = 1;
        degraded.health.running = 10;
        let (hero, _) = vital_figures(&degraded);
        assert_eq!(hero.value, "10 / 12");
        assert_eq!(hero.caption, "1 failed");
        assert_eq!(hero.severity, Severity::Error);
        assert_eq!(health_copy(&degraded).0, "Failed pods detected");
    }

    /// A cluster that reported nothing shows a dash, because a zero is a reading
    /// the app never took.
    #[test]
    fn an_empty_cluster_reports_a_dash_instead_of_a_zero_ratio() {
        let (hero, signals) = vital_figures(&Overview::default());
        assert_eq!(hero.value, "—");
        assert_eq!(hero.caption, "No pods reported");
        for signal in &signals {
            assert_eq!(signal.value, "—");
        }
        assert_eq!(signals[0].caption, "No nodes reported");
        assert_eq!(signals[1].caption, "No workloads reported");
        assert!(
            workload_breakdown(&Overview::default()).is_empty(),
            "no per-kind figures are drawn from data the cluster never sent"
        );
    }

    /// A pod the kubelet lost is not a pod that needs attention.
    ///
    /// The table draws it as a dash with no verdict and a muted mark, because the
    /// app has no reading on it. The hero caption a screenful above used to spend
    /// the whole caution set on it — the banner's own amber — so a cluster whose
    /// only problem was one unreadable pod read as a warning event over a row that
    /// said "no verdict". The cluster verdict above it is a separate question and
    /// belongs to `k8s_core::overview::level`, which still counts this bucket.
    #[test]
    fn an_unknown_pod_is_not_a_warning_on_the_hero() {
        // One snapshot per bucket, with the totals reconciled so the ratio beside
        // the caption and the caption itself describe the same ten pods.
        let snapshot = |pending: usize, failed: usize, unknown: usize, running: usize| {
            let mut built = overview(
                HealthSummary {
                    total_pods: running + pending + failed + unknown,
                    running,
                    pending,
                    failed,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 2,
                    ready: 2,
                    not_ready: 0,
                },
                HealthLevel::Warning,
            );
            built.unknown_pods = unknown;
            built
        };

        // One unreadable pod and nothing else: no warning anywhere on the hero.
        let unreadable = snapshot(0, 0, 1, 9);
        let (hero, _) = vital_figures(&unreadable);
        assert_eq!(hero.value, "9 / 10");
        assert_eq!(hero.caption, "1 unknown");
        assert_ne!(
            hero.severity,
            Severity::Warning,
            "a pod the app could not read is not a problem it can report"
        );
        assert_eq!(
            hero.severity,
            Severity::Muted,
            "and it says so in the same muted the table's dash is drawn in"
        );
        assert!(
            hero.actionable,
            "the count still routes: the table's problems filter keeps a pod with no verdict"
        );
        // The page's own table vocabulary agrees, which is the whole point: one
        // fact, one verdict, on both sides of the same screen.
        assert_ne!(
            design::pod_severity("Unknown"),
            design::pod_severity("Pending"),
            "the table and the hero disagree if Unknown is a warning here and neutral there"
        );
        assert_eq!(
            design::health_label(design::pod_severity("Unknown")),
            "No verdict"
        );

        // A real problem beside an unreadable one still speaks, and the caption
        // leads with the bucket that is one.
        let mixed = snapshot(5, 0, 3, 2);
        let (hero, _) = vital_figures(&mixed);
        assert_eq!(hero.value, "2 / 10");
        assert_eq!(hero.severity, Severity::Warning);
        assert_eq!(
            hero.caption, "5 pending · 3",
            "the bucket with a verdict leads, and the unreadable one is counted"
        );

        // A tie is reported as the bucket that is a problem. `unknown` is listed
        // first precisely so it loses: `max_by_key` keeps the *last* of several
        // equal maxima, and the reverse order would print an unreadable pod as the
        // headline every time it tied with a real one.
        let tied = snapshot(3, 0, 3, 4);
        let (hero, _) = vital_figures(&tied);
        assert_eq!(
            hero.caption, "3 pending · 3",
            "a tie names the worse of the two"
        );

        // A failed pod is still the loudest thing on the page, and wins a
        // three-way tie.
        let failing = snapshot(3, 3, 3, 1);
        let (hero, _) = vital_figures(&failing);
        assert_eq!(hero.value, "1 / 10");
        assert_eq!(hero.severity, Severity::Error);
        assert_eq!(hero.caption, "3 failed · 6");
    }

    #[test]
    fn failed_pods_raise_the_banner_to_error() {
        let overview = overview(
            HealthSummary {
                total_pods: 3,
                running: 2,
                failed: 1,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Error,
        );
        let (title, detail) = health_copy(&overview);
        assert_eq!(title, "Failed pods detected");
        assert_eq!(detail, "1 pod not running.");
        assert_eq!(health_severity(&overview), Severity::Error);
    }

    #[test]
    fn health_semantics_share_one_mapping() {
        for level in [
            HealthLevel::Healthy,
            HealthLevel::Warning,
            HealthLevel::Error,
        ] {
            let (title, severity) = health_level_semantics(level);
            let mut actual = overview(
                HealthSummary::default(),
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                level,
            );
            if level == HealthLevel::Warning {
                actual.nodes.not_ready = 1;
                actual.nodes.ready = 0;
            }
            assert_eq!(health_copy(&actual).0, title);
            assert_eq!(health_severity(&actual), severity);
        }
    }

    #[test]
    fn node_context_keeps_status_without_a_health_column() {
        let empty = Overview::default();
        assert_eq!(
            node_context(&empty),
            ("No nodes reported".to_owned(), Severity::Muted)
        );

        let ready = Overview {
            nodes: NodeSummary {
                count: 2,
                ready: 2,
                not_ready: 0,
            },
            ..Overview::default()
        };
        assert_eq!(
            node_context(&ready),
            ("2 / 2 ready".to_owned(), Severity::Success)
        );

        let not_ready = Overview {
            nodes: NodeSummary {
                count: 3,
                ready: 2,
                not_ready: 1,
            },
            ..Overview::default()
        };
        assert_eq!(
            node_context(&not_ready),
            ("2 / 3 ready · 1 not ready".to_owned(), Severity::Warning)
        );
    }

    /// A cluster the app could not read completely is not an unhealthy cluster.
    ///
    /// The banner used to answer `Partial` with amber, a caution wash, and a
    /// warning glyph — the whole set of symbols that means "this object is in bad
    /// shape" — while the actual problem was the app's own RBAC. The confidence
    /// channel is where that belongs, and it has to actually speak: a partial
    /// snapshot is `Unknown`, so the hollow `?` is drawn.
    #[test]
    fn a_partial_snapshot_is_not_painted_as_a_health_warning() {
        let empty = Overview::default();
        assert_eq!(health_copy(&empty).0, "No cluster data");
        assert_eq!(health_severity(&empty), Severity::Muted);

        let partial = overview(
            HealthSummary {
                total_pods: 4,
                running: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        assert_eq!(overview_data_state(&partial), OverviewDataState::Complete);
        assert_eq!(health_severity(&partial), Severity::Success);

        let mut denied = partial.clone();
        denied.unavailable_sources = vec![UnavailableSource {
            source: "statefulsets",
            reason: "403 Forbidden".to_owned(),
        }];
        assert_eq!(overview_data_state(&denied), OverviewDataState::Partial);
        assert_eq!(health_copy(&denied).0, "Partial cluster data");
        assert_eq!(
            health_severity(&denied),
            Severity::Muted,
            "amber here would blame the cluster for the app's own missing permission"
        );
        assert_eq!(
            snapshot_confidence(&OverviewState::Ready(denied.clone()), false, false),
            design::Confidence::Unknown,
            "the health channel is muted, so the confidence channel has to speak"
        );
        // A cluster that is actually unhealthy and also unreadable keeps the
        // muted banner: the app has no verdict either way.
        let mut unhealthy = denied.clone();
        unhealthy.health.pending = 2;
        assert_eq!(health_severity(&unhealthy), Severity::Muted);
        assert_eq!(
            snapshot_confidence(&OverviewState::Ready(unhealthy), false, false),
            design::Confidence::Unknown
        );
    }

    /// The banner is the only place in the app that claims a cluster is healthy,
    /// and the claim has to come from the cluster rather than from the socket.
    #[test]
    fn the_banner_verdict_follows_cluster_facts_and_nothing_else() {
        let ready = |nodes: NodeSummary, health: HealthSummary| {
            overview(health, nodes, HealthLevel::Healthy)
        };
        let healthy = ready(
            NodeSummary {
                count: 2,
                ready: 2,
                not_ready: 0,
            },
            HealthSummary {
                total_pods: 6,
                running: 6,
                ..HealthSummary::default()
            },
        );
        assert_eq!(health_copy(&healthy).0, "Cluster healthy");
        assert_eq!(health_severity(&healthy), Severity::Success);

        // Same connection, different cluster: a node that is not ready is a
        // verdict, and it moves the banner.
        let mut not_ready = healthy.clone();
        not_ready.nodes.not_ready = 1;
        not_ready.nodes.ready = 1;
        assert_eq!(health_copy(&not_ready).0, "Cluster needs attention");
        assert_eq!(health_severity(&not_ready), Severity::Warning);

        // A workload below target is a verdict too.
        let mut behind = healthy.clone();
        behind.workloads.deployments = ReplicaSummary {
            desired: 4,
            available: 1,
        };
        behind.unavailable_workloads = 1;
        assert_eq!(health_copy(&behind).0, "Cluster needs attention");

        // A failed pod is an error, and it says so in words.
        let mut failing = healthy.clone();
        failing.health.failed = 1;
        assert_eq!(health_copy(&failing).0, "Failed pods detected");
        assert_eq!(health_severity(&failing), Severity::Error);

        // The banner sentence names a unit per clause. One total across pods,
        // nodes, and workload objects cannot be reconciled with the replica
        // figure printed 100px below it.
        let mut mixed = healthy.clone();
        mixed.nodes.not_ready = 1;
        mixed.nodes.ready = 1;
        mixed.health.pending = 2;
        mixed.health.running = 4;
        mixed.unavailable_workloads = 201;
        let (_, copy) = health_copy(&mixed);
        assert_eq!(
            copy,
            "1 node not ready, 2 pods not running, 201 workloads below target."
        );
        assert!(
            !copy.to_lowercase().contains("problem"),
            "no grand total mixes three counting units: {copy}"
        );
    }

    #[test]
    fn unavailable_sources_distinguish_empty_partial_and_total_failure() {
        let source = |name: &'static str, reason: &str| UnavailableSource {
            source: name,
            reason: reason.to_owned(),
        };
        let empty = Overview {
            unavailable_sources: (0..OVERVIEW_SOURCE_COUNT)
                .map(|_| source("source", SOURCE_NOT_LOADED))
                .collect(),
            ..Overview::default()
        };
        assert_eq!(overview_data_state(&empty), OverviewDataState::Empty);

        let mut partial = overview(
            HealthSummary::default(),
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        partial.unavailable_sources = vec![source("pods", "403 Forbidden")];
        assert_eq!(overview_data_state(&partial), OverviewDataState::Partial);
        assert_eq!(health_copy(&partial).0, "Partial cluster data");
        let copy = health_copy(&partial).1;
        assert!(
            !copy.contains("403 Forbidden"),
            "the raw API text stays in the tooltip: {copy}"
        );
        assert!(
            copy.contains("list pods"),
            "a denial names the permission in the body: {copy}"
        );

        let failed = Overview {
            unavailable_sources: (0..OVERVIEW_SOURCE_COUNT)
                .map(|_| source("source", "request failed"))
                .collect(),
            ..Overview::default()
        };
        assert_eq!(overview_data_state(&failed), OverviewDataState::Error);
        assert_eq!(health_copy(&failed).0, "Failed to load cluster data");
        assert!(source_failure_detail(&failed).contains("request failed"));
    }
    /// A denied cluster is not an unreachable cluster. The two states need
    /// different titles, different advice, and different focus targets.
    #[test]
    fn a_denied_cluster_is_not_reported_as_a_connection_failure() {
        let names = [
            "pods",
            "nodes",
            "deployments",
            "statefulsets",
            "daemonsets",
            "jobs",
            "cronjobs",
        ];
        let denied = Overview {
            unavailable_sources: names
                .into_iter()
                .map(|name| UnavailableSource {
                    source: name,
                    reason: format!("Failed to list {name}: (Status {{ code: 403 }})"),
                })
                .collect(),
            ..Overview::default()
        };
        assert_eq!(overview_data_state(&denied), OverviewDataState::Forbidden);
        let (title, copy) = health_copy(&denied);
        assert_eq!(title, "Access denied");
        assert!(copy.contains("grant"), "{copy}");
        assert!(copy.contains("list pods"));
        assert!(!copy.contains("403"));
        assert!(
            !copy.contains("cluster connection"),
            "connection advice is the wrong fix for a denial: {copy}"
        );
        assert!(
            source_failure_detail(&denied).contains("403"),
            "the raw cause stays available in the tooltip"
        );

        let unreachable = Overview {
            unavailable_sources: names
                .into_iter()
                .map(|name| UnavailableSource {
                    source: name,
                    reason: "request timed out after 8s".to_owned(),
                })
                .collect(),
            ..Overview::default()
        };
        assert_eq!(
            overview_data_state(&unreachable),
            OverviewDataState::Error,
            "an unreachable cluster is still a connection failure"
        );
        assert_eq!(health_copy(&unreachable).0, "Failed to load cluster data");
        assert_ne!(
            overview_data_state(&denied),
            overview_data_state(&unreachable)
        );
    }

    #[gpui::test]
    fn total_source_failure_renders_user_error_and_retry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(Overview {
                unavailable_sources: (0..OVERVIEW_SOURCE_COUNT)
                    .map(|index| UnavailableSource {
                        source: "source",
                        reason: format!("request {index} failed"),
                    })
                    .collect(),
                ..Overview::default()
            });
            cx.notify();
        });
        cx.run_until_parked();
        let retry = view.read_with(cx, |view, _| view.retry_focus.clone());
        assert_eq!(
            view.read_with(cx, |view, _| view.focus_default_control()),
            retry
        );
        assert!(cx.debug_bounds("overview-error").is_some());
    }

    /// The panel takes the width it is given, and a caption the panel has to cut
    /// keeps its whole text.
    ///
    /// The compact rule was 1200px, which gave up the section explanations on
    /// every panel narrower than a comfortable window — including the 480 to
    /// 800px centre panel a 960px window leaves — and gave them up at exactly
    /// the widths where the capacity table has already pushed four of its
    /// columns out of view. It is now tied to the supported window instead: the
    /// rule is read against the panel's own width, which is never wider than the
    /// window, so 960px is not compact and the sentence is there at the floor.
    #[test]
    fn overview_uses_available_width_and_preserves_long_caption_text() {
        assert!(!overview_is_compact(design::size::WINDOW_MIN.0));
        assert!(!overview_is_compact(1_200.0));
        // The section headings keep their explanation down to the breakpoint,
        // because that is where columns start leaving the viewport and the
        // reader needs to be told what the ones they can see hold.
        assert_eq!(OVERVIEW_COMPACT_WIDTH, 900.0);
        assert!(!overview_is_compact(OVERVIEW_COMPACT_WIDTH));
        assert!(
            overview_is_compact(OVERVIEW_COMPACT_WIDTH - 1.0),
            "below the breakpoint the heading keeps only its name"
        );
        assert!(
            OVERVIEW_COMPACT_WIDTH <= design::size::WINDOW_MIN.0,
            "the compact breakpoint may not sit above the minimum window: {} vs {}",
            OVERVIEW_COMPACT_WIDTH,
            design::size::WINDOW_MIN.0
        );
        let caption = "9,900 of 10,004 below target across five workload kinds";
        assert_eq!(caption_tooltip(caption).as_deref(), Some(caption));
        assert_eq!(caption_tooltip("No workload data"), None);
    }

    #[test]
    fn refresh_error_keeps_the_last_ready_snapshot() {
        let snapshot = overview(
            HealthSummary {
                total_pods: 1,
                running: 1,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        let mut state = OverviewState::Ready(snapshot);
        let mut refreshing = true;
        let mut refresh_error = None;
        assert!(
            apply_refresh_result(
                &mut state,
                &mut refreshing,
                &mut refresh_error,
                4,
                4,
                Err("stderr: secret".to_owned()),
            )
            .is_none()
        );
        assert!(!refreshing);
        assert_eq!(refresh_error.as_deref(), Some("stderr: secret"));
        assert!(matches!(state, OverviewState::Ready(_)));
    }

    #[test]
    fn capacity_columns_fill_the_width_and_keep_a_minimum() {
        let without_usage = capacity_column_widths(960.0, false);
        assert_eq!(without_usage.len(), CAPACITY_COLUMN_COUNT);
        assert_columns_fill(&without_usage, 960.0);
        for (column, width) in without_usage.iter().enumerate() {
            assert!(
                *width >= CAPACITY_COLUMN_MIN_WIDTHS[column],
                "column {column} shrank below its minimum"
            );
        }
        assert!(
            without_usage[0] > CAPACITY_NODE_WIDTH,
            "the spare width goes to the columns, not to empty space"
        );

        let with_usage = capacity_column_widths(960.0, true);
        assert_eq!(with_usage.len(), CAPACITY_COLUMN_MIN_WIDTHS.len());
        assert_columns_fill(&with_usage, 960.0);

        // A narrow panel keeps the minimum grid and scrolls, instead of
        // squashing the numbers into unreadable widths.
        let narrow = capacity_column_widths(320.0, true);
        assert_eq!(narrow, CAPACITY_COLUMN_MIN_WIDTHS.to_vec());
        let minimum_grid: f32 = CAPACITY_COLUMN_MIN_WIDTHS.iter().sum();
        assert_eq!(capacity_table_width(&narrow), minimum_grid);
        assert!(
            minimum_grid > f32::from(design::size::CENTER_MIN),
            "the minimum grid is wider than the narrowest centre panel, so that panel scrolls"
        );
        assert_eq!(
            capacity_column_widths(0.0, false),
            CAPACITY_COLUMN_MIN_WIDTHS[..CAPACITY_COLUMN_COUNT].to_vec(),
            "before the first measurement the columns keep their minimums"
        );

        // A panel measured before the window shrank is wider than the window. The
        // window bounds the width the columns may take, so a stale measurement
        // cannot make the grid grow past what the panel is entitled to.
        //
        // The padding the columns lose is the three boxes the grid sits inside, and
        // it is written out here because the constant is a guess otherwise: the
        // scroll's own `space::LG` padding, the group's 1px border, and the
        // section's `space::LG` inset, on both sides. It used to be 34px, which is
        // the first two terms only, so every column was handed 16px per side that
        // the section had already taken.
        assert_eq!(
            capacity_panel_padding(),
            2.0 * (f32::from(space::LG) + f32::from(design::border::LINE) + f32::from(space::LG)),
            "the grid's padding is the scroll inset, the group frame and the section inset"
        );
        assert_eq!(capacity_available_width(1_920.0, 960.0), 894.0);
        let gap = f32::from(space::SM);
        let chrome = capacity_row_chrome(CAPACITY_COLUMN_MIN_WIDTHS.len());
        let stale = capacity_column_widths(
            (capacity_available_width(1_920.0, 960.0) - chrome).max(0.0),
            true,
        );
        let stale_table = capacity_table_width(&stale) + chrome;
        assert_eq!(
            stale_table,
            minimum_grid + chrome,
            "seven columns of real figures do not fit 960px, so the grid keeps its minimums"
        );
        assert_eq!(
            capacity_available_width(960.0, 960.0),
            capacity_available_width(1_920.0, 960.0),
            "a settled measurement is the panel width"
        );
        // A panel that can afford the grid gives it every pixel it has.
        let available = capacity_available_width(1_920.0, 1_920.0);
        let wide = capacity_column_widths(available - chrome, true);
        assert_columns_fill(&wide, available - chrome);

        // The cells and their gaps fit inside the table box. A row that holds
        // less than its cells need pushes the last column outside the table,
        // and the scroll container clips a value the user has to read.
        assert!(
            capacity_table_width(&stale)
                <= stale_table - 2.0 * gap - (stale.len() - 1) as f32 * gap,
            "the columns and their gaps have to fit the row they are laid out in"
        );
    }

    /// The table has to be reachable and readable without a pointer: the
    /// minimum window is 960x640, and every cell is a Tab stop away.
    #[test]
    fn the_capacity_table_is_keyboard_navigable() {
        assert_eq!(next_cursor((0, 0), "down", 3, 6, false), Some((1, 0)));
        assert_eq!(next_cursor((0, 0), "right", 3, 6, false), Some((0, 1)));
        assert_eq!(next_cursor((0, 0), "left", 3, 6, false), Some((0, 0)));
        assert_eq!(next_cursor((2, 5), "down", 3, 6, false), Some((2, 5)));
        assert_eq!(next_cursor((0, 0), "end", 3, 6, false), Some((0, 5)));
        assert_eq!(next_cursor((0, 0), "home", 3, 6, false), Some((0, 0)));
        assert_eq!(next_cursor((2, 3), "home", 3, 6, true), Some((0, 0)));
        assert_eq!(next_cursor((0, 0), "end", 3, 6, true), Some((2, 5)));
        assert_eq!(next_cursor((0, 0), "pagedown", 12, 6, false), Some((10, 0)));
        assert_eq!(
            next_cursor((11, 0), "pagedown", 12, 6, false),
            Some((11, 0))
        );
        assert_eq!(next_cursor((11, 0), "pageup", 12, 6, false), Some((1, 0)));
        assert_eq!(next_cursor((0, 0), "a", 3, 6, false), None);
        assert_eq!(next_cursor((0, 0), "down", 0, 6, false), None);
        assert_eq!(next_cursor((0, 0), "right", 3, 0, false), None);

        // The cursor stays inside the table after a resize or a new snapshot.
        assert_eq!(clamp_cursor((9, 5), 3, 5), (2, 4));
        assert_eq!(clamp_cursor((1, 1), 0, 5), (0, 0));
        assert_eq!(clamp_cursor((1, 1), 4, 0), (0, 0));
    }

    /// Clicking a heading sorts, clicking it again reverses, and a third click
    /// returns to the order the snapshot arrived in.
    #[test]
    fn the_capacity_table_sorts_on_every_column() {
        assert_eq!(
            next_capacity_sort(Sort::ascending(0), 2),
            Sort::ascending(2)
        );
        assert_eq!(
            next_capacity_sort(Sort::ascending(2), 2),
            Sort::descending(2)
        );
        assert_eq!(
            next_capacity_sort(Sort::descending(2), 2),
            Sort::ascending(0)
        );

        // The rows arrive in an order no sort returns, and every column has one
        // answer of its own, so a comparator that reads the wrong field, or
        // that leaves the row order alone, is caught.
        let gib = 1024.0 * 1024.0 * 1024.0;
        // requested cpu, requested memory, cpu limit, memory limit
        let values = |name: &str| -> (f64, f64, f64, f64) {
            match name {
                "a" => (0.0, 4.0 * gib, 2.0, 0.0),
                "b" => (2.0, 0.0, 4.0, 2.0 * gib),
                _ => (4.0, 2.0 * gib, 0.0, 4.0 * gib),
            }
        };
        let rows: Vec<(String, NodeCapacity, Option<NodeUsage>)> = ["c", "a", "b"]
            .into_iter()
            .map(|name| {
                let (requested_cpu, requested_memory, limits_cpu, limits_memory) = values(name);
                (
                    name.to_owned(),
                    NodeCapacity {
                        name: name.to_owned(),
                        allocatable_cpu: Some(4.0),
                        allocatable_memory: Some(8.0 * gib),
                        requested_cpu,
                        requested_memory,
                        limits_cpu,
                        limits_memory,
                        utilization_pct: None,
                    },
                    None,
                )
            })
            .collect();
        let names = |sort| {
            sorted_capacity_rows(rows.clone(), sort)
                .into_iter()
                .map(|(name, _, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Sort::ascending(0)), ["a", "b", "c"]);
        assert_eq!(names(Sort::descending(0)), ["c", "b", "a"]);
        assert_eq!(names(Sort::ascending(1)), ["a", "b", "c"]);
        assert_eq!(names(Sort::descending(1)), ["c", "b", "a"]);
        assert_eq!(names(Sort::ascending(2)), ["b", "c", "a"]);
        assert_eq!(names(Sort::descending(2)), ["a", "c", "b"]);
        assert_eq!(names(Sort::ascending(3)), ["c", "a", "b"]);
        assert_eq!(names(Sort::descending(3)), ["b", "a", "c"]);
        assert_eq!(names(Sort::ascending(4)), ["a", "b", "c"]);
        assert_eq!(names(Sort::descending(4)), ["c", "b", "a"]);

        // Each live column reads its own axis, so a comparator that borrowed the
        // other one is caught, and a node with no reading sorts last instead of
        // pretending to be zero.
        let live_names = |column: usize, sample: &dyn Fn(&str) -> NodeUsage| {
            sorted_capacity_rows(
                rows.iter()
                    .map(|(name, capacity, _)| (name.clone(), capacity.clone(), Some(sample(name))))
                    .collect(),
                Sort::ascending(column),
            )
            .into_iter()
            .map(|(name, _, _)| name)
            .collect::<Vec<_>>()
        };
        // The samples are handed out in the reverse of the node-name order, so a
        // sort that read the wrong axis or ignored the value lands somewhere else.
        let cpu_of = |name: &str| match name {
            "a" => 30.0,
            "b" => 20.0,
            _ => 10.0,
        };
        let memory_of = |name: &str| match name {
            "a" => 1.0 * gib,
            "b" => 2.0 * gib,
            _ => 3.0 * gib,
        };
        assert_eq!(
            live_names(5, &|name: &str| NodeUsage {
                name: name.to_owned(),
                cpu_millicores: Some(cpu_of(name)),
                memory_bytes: None,
            }),
            ["c", "b", "a"]
        );
        assert_eq!(
            live_names(6, &|name: &str| NodeUsage {
                name: name.to_owned(),
                cpu_millicores: None,
                memory_bytes: Some(memory_of(name)),
            }),
            ["a", "b", "c"]
        );
        assert_eq!(
            live_names(5, &|name: &str| NodeUsage {
                name: name.to_owned(),
                cpu_millicores: None,
                memory_bytes: Some(memory_of(name)),
            }),
            ["a", "b", "c"],
            "a silent live column falls back to the node-name order"
        );
        assert_eq!(
            live_names(6, &|name: &str| NodeUsage {
                name: name.to_owned(),
                cpu_millicores: Some(cpu_of(name)),
                memory_bytes: None,
            }),
            ["a", "b", "c"],
            "a silent live column falls back to the node-name order"
        );
    }

    /// Every capacity cell has to hold the value the panel renders, at the
    /// product default data size.
    ///
    /// The audit found every numeric column ending in an ellipsis, and an ellipsis
    /// reports no value at all, so the minimum grid is measured against the text a
    /// real row produces rather than guessed.
    #[test]
    fn every_capacity_value_fits_its_column() {
        let gib = 1024.0 * 1024.0 * 1024.0;
        let capacity = NodeCapacity {
            name: "k8s-gpui-dev-control-plane".to_owned(),
            allocatable_cpu: Some(20.0),
            allocatable_memory: Some(31.1 * gib),
            requested_cpu: 1.15,
            requested_memory: 1.3 * gib,
            limits_cpu: 20.0,
            limits_memory: 31.1 * gib,
            utilization_pct: Some(6.0),
        };
        let usage = NodeUsage {
            name: capacity.name.clone(),
            cpu_millicores: Some(244.0),
            memory_bytes: Some(3.2 * gib),
        };
        // The live columns print a reading *and* the capacity it is measured
        // against, so a reader can tell 10% of a node from 90% of one without
        // doing arithmetic on a bare number.
        assert_eq!(
            format_cpu_now(Some(&usage), &capacity, true),
            "244m / 20 cores"
        );
        assert_eq!(
            format_memory_now(Some(&usage), &capacity, true),
            "3.2 GiB / 31.1 GiB"
        );
        let values = [
            capacity.name.clone(),
            request_figure(
                capacity.requested_cpu,
                capacity.allocatable_cpu,
                format_cores,
                |cores| format!("{} cores", format_cores(cores)),
            ),
            request_figure(
                capacity.requested_memory,
                capacity.allocatable_memory,
                format_bytes,
                format_bytes,
            ),
            format_limit(capacity.limits_cpu, format_cores),
            format_limit(capacity.limits_memory, format_bytes),
            format_cpu_now(Some(&usage), &capacity, true),
            format_memory_now(Some(&usage), &capacity, true),
        ];
        assert_eq!(values.len(), CAPACITY_COLUMNS.len());
        for (column, value) in values.iter().enumerate() {
            let needed = f32::from(crate::settings::default_columns(
                value.chars().count() as f32
            ));
            // A request cell also reserves the slot its overcommit glyph takes, so
            // the widest value on a row still lands in front of a glyph rather
            // than behind an ellipsis.
            let marker = usize::from(matches!(column, 1 | 2)) * 16;
            assert!(
                needed + marker as f32 <= CAPACITY_COLUMN_MIN_WIDTHS[column],
                "{} needs {needed}px of data font (plus a {marker}px severity slot) and its column is {}px",
                CAPACITY_COLUMNS[column],
                CAPACITY_COLUMN_MIN_WIDTHS[column]
            );
        }

        // A node with no reading yet says so in a word a cell can hold, and never
        // claims a zero the app never measured.
        let mut nameless = usage.clone();
        nameless.cpu_millicores = None;
        nameless.memory_bytes = None;
        assert_eq!(format_cpu_now(Some(&nameless), &capacity, true), "Waiting");
        assert_eq!(format_memory_now(None, &capacity, false), "No metrics");
        assert_eq!(
            format_node_usage(Some(&nameless), false),
            "Metrics unavailable",
            "the full sentence stays in the row's spoken label"
        );
    }

    /// A zero limit means "no limit declared", and a bare `0` reads as the
    /// opposite claim.
    #[test]
    fn an_undeclared_limit_is_not_a_zero() {
        assert_eq!(format_limit(0.0, format_cores), NO_ANSWER);
        assert_eq!(format_limit(0.0, format_bytes), NO_ANSWER);
        assert_eq!(format_limit(2.0, format_cores), "2");
        assert_eq!(format_limit(2.0 * 1024.0 * 1024.0, format_bytes), "2 MiB");
    }

    #[test]
    fn request_summary_calls_out_cpu_oversubscription() {
        assert!((request_ratio(4.8, Some(4.0)).unwrap() - 120.0).abs() < 1e-9);
        assert_eq!(
            request_text(4.8, Some(4.0), format_cores, |cores| {
                format!("{} cores", format_cores(cores))
            }),
            "4.80 / 4 cores · 120% oversubscribed"
        );
    }

    #[test]
    fn refresh_copy_matches_the_manual_refresh_behaviour() {
        let at = UNIX_EPOCH + Duration::from_secs(3_661);
        assert_eq!(format_clock(at), "01:01:01 UTC");
        assert_eq!(
            refresh_status(&OverviewState::Ready(Overview::default()), true, Some(&at)),
            "Refreshing… · Manual refresh · updated 01:01:01 UTC"
        );
        assert_eq!(
            refresh_status(&OverviewState::Ready(Overview::default()), false, Some(&at)),
            "Manual refresh · updated 01:01:01 UTC"
        );
        assert_eq!(
            refresh_status(&OverviewState::Ready(Overview::default()), false, None),
            "Manual refresh"
        );
        assert_eq!(
            refresh_status(&OverviewState::Failed("stderr".to_owned()), false, None),
            "Manual refresh · refresh failed"
        );
        assert!(
            !refresh_status(&OverviewState::Ready(Overview::default()), false, Some(&at))
                .contains("Last refreshed"),
            "the old copy promised a timer the panel does not run"
        );
        assert_eq!(
            OverviewView::error_hint("stderr: secret"),
            "Retry, or make sure the cluster connection works."
        );
    }

    #[test]
    fn capacity_summary_mentions_every_metric_once() {
        let capacity = NodeCapacity {
            name: "worker-a".to_owned(),
            allocatable_cpu: Some(4.0),
            allocatable_memory: Some(8.0 * 1024.0 * 1024.0 * 1024.0),
            requested_cpu: 2.4,
            requested_memory: 1.2 * 1024.0 * 1024.0 * 1024.0,
            limits_cpu: 3.0,
            limits_memory: 2.0 * 1024.0 * 1024.0 * 1024.0,
            utilization_pct: Some(62.0),
        };
        let usage = NodeUsage {
            name: "worker-a".to_owned(),
            cpu_millicores: Some(250.0),
            memory_bytes: Some(512.0 * 1024.0 * 1024.0),
        };
        let summary = capacity_summary(&capacity, Some(&usage));
        assert_eq!(
            format_node_usage(Some(&usage), true),
            "CPU 250m · Memory 512 MiB"
        );
        let memory_only = NodeUsage {
            name: "worker-a".to_owned(),
            cpu_millicores: None,
            memory_bytes: Some(512.0 * 1024.0 * 1024.0),
        };
        assert_eq!(
            format_node_usage(Some(&memory_only), true),
            "Memory 512 MiB"
        );
        for needle in [
            "worker-a",
            "Node utilization: 62% of allocatable",
            "CPU requests: 2.40 / 4 cores · 60% of allocatable",
            "Memory requests: 1.2 GiB / 8 GiB · 15% of allocatable",
            "CPU limits: 3",
            "Memory limits: 2 GiB",
            "Current usage: CPU 250m · Memory 512 MiB",
        ] {
            assert!(
                summary.contains(needle),
                "missing {needle:?} in {summary:?}"
            );
        }

        // An unknown allocatable has no share to state, so the row says so
        // rather than inventing one.
        let unknown = NodeCapacity {
            allocatable_cpu: None,
            utilization_pct: None,
            ..capacity
        };
        let summary = capacity_summary(&unknown, None);
        assert!(
            !summary.contains("utilization"),
            "no allocatable means no percentage: {summary}"
        );
        assert!(
            summary.contains("allocatable unavailable"),
            "the row says why: {summary}"
        );
    }

    /// The percentage of allocatable a node is asked for is computed in the data
    /// layer and used by the row's own name and tooltip. The two request cells
    /// each carry their own axis, and neither says the blended number.
    #[test]
    fn the_rows_own_words_carry_the_node_share() {
        let summary = capacity_summary(&oversubscribed_node().capacities[0], None);
        assert!(
            summary.starts_with("worker-0, Node utilization: 120% oversubscribed"),
            "{summary}"
        );
        assert!(
            summary.contains("CPU requests: 4.80 / 4 cores · 120% oversubscribed"),
            "the axis that is over says so in the cell's own words: {summary}"
        );
    }

    /// A count is the same number everywhere in the app, so every figure here
    /// goes through the shared formatter instead of a hand-rolled separator.
    #[test]
    fn every_count_uses_the_shared_thousands_separator() {
        let mut snapshot = overview(
            HealthSummary {
                total_pods: 10_010,
                running: 110,
                pending: 9_900,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 3,
                ready: 3,
                not_ready: 0,
            },
            HealthLevel::Warning,
        );
        snapshot.workloads.deployments = ReplicaSummary {
            desired: 10_004,
            available: 104,
        };

        assert_eq!(health_copy(&snapshot).1, "9,900 pods not running.");
        let (hero, signals) = vital_figures(&snapshot);
        assert_eq!(hero.value, "110 / 10,010");
        assert_eq!(hero.caption, "9,900 pending");
        assert_eq!(
            hero.detail,
            "Pods running 110 / 10,010, 9,900 pending, 0 failed"
        );
        assert_eq!(signals[1].value, "104 / 10,004");
        assert_eq!(signals[1].caption, "9,900 of 10,004 below target");
        assert!(
            signals[1].detail.contains("Deployments 104 / 10,004"),
            "the per-kind figures keep the separator: {:?}",
            signals[1].detail
        );
        assert_eq!(node_context(&snapshot).0, "3 / 3 ready");
        assert_eq!(
            workload_breakdown(&snapshot),
            vec![
                ("Deployments", Some("104 / 10,004".to_owned())),
                ("Stateful sets", Some(NO_ANSWER.to_owned())),
                ("Daemon sets", Some(NO_ANSWER.to_owned())),
                ("Jobs", Some(NO_ANSWER.to_owned())),
                ("Cron jobs", Some(NO_ANSWER.to_owned())),
            ],
            "the grid keeps the separator, names every kind in the words a person uses, and a kind with nothing in it is a dash"
        );
    }

    /// "The cluster reported nothing" is one answer, and a source the app was not
    /// allowed to read is a different one from a list that is genuinely empty.
    #[test]
    fn a_denied_source_is_not_drawn_as_a_zero_ratio() {
        let mut snapshot = overview(
            HealthSummary {
                total_pods: 2,
                running: 2,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        snapshot.workloads.deployments = ReplicaSummary {
            desired: 6,
            available: 6,
        };
        // No statefulsets at all, and the source was readable.
        assert_eq!(
            workload_breakdown(&snapshot),
            vec![
                ("Deployments", Some("6 / 6".to_owned())),
                ("Stateful sets", Some(NO_ANSWER.to_owned())),
                ("Daemon sets", Some(NO_ANSWER.to_owned())),
                ("Jobs", Some(NO_ANSWER.to_owned())),
                ("Cron jobs", Some(NO_ANSWER.to_owned())),
            ],
            "a kind with no replicas is a dash, in the same spelling as no answer"
        );

        snapshot.unavailable_sources = vec![
            UnavailableSource {
                source: "statefulsets",
                reason: "403 Forbidden".to_owned(),
            },
            UnavailableSource {
                source: "cronjobs",
                reason: "403 Forbidden".to_owned(),
            },
        ];
        let breakdown = workload_breakdown(&snapshot);
        assert_eq!(
            breakdown[1],
            ("Stateful sets", None),
            "a denied kind has no figure, so it cannot be confused with an empty one"
        );
        assert_eq!(breakdown[0], ("Deployments", Some("6 / 6".to_owned())));
        assert_eq!(breakdown[4], ("Cron jobs", None));
        assert!(
            breakdown.iter().any(|(_, figure)| figure.is_none()),
            "the grid has to be able to say 'I could not read this'"
        );
        assert!(
            workload_detail(&snapshot).contains("Stateful sets not read"),
            "the spoken label says which kinds are missing, not just how many: {:?}",
            workload_detail(&snapshot)
        );
        // The two dashes are told apart by the mark beside them, and the mark
        // exists: the confidence channel owns it.
        assert_ne!(
            design::confidence::icon(design::Confidence::Unknown),
            design::health_icon(Severity::Muted),
            "a missing figure and a healthy-looking dash must not share a shape"
        );
    }

    /// The cell shows the pair, so the number survives the column width. The
    /// share of allocatable stays in the row label and the tooltip.
    #[test]
    fn a_request_cell_shows_the_pair_and_not_the_share() {
        let cores = |cores: f64| format!("{} cores", format_cores(cores));
        assert_eq!(
            request_figure(1.15, Some(20.0), format_cores, cores),
            "1.15 / 20 cores"
        );
        assert_eq!(
            request_text(1.15, Some(20.0), format_cores, cores),
            "1.15 / 20 cores · 6% of allocatable"
        );
        assert_eq!(
            request_figure(1.3, None, format_cores, cores),
            "1.30 requested",
            "an unknown capacity still shows the requested value"
        );
    }

    /// The table, its zebra, and the header band are one surface.
    ///
    /// The row base was left on `canvas` while `design` composites every row
    /// wash onto `surface::input`, so the stripe was mixed against a base it is
    /// not drawn on. The gap between the two is a few percent of lightness, which
    /// is why no screenshot caught it and why this has to be asserted instead.
    ///
    /// The solved zebra is not compared here: `design` walks the wash away from
    /// its base until it clears `ROW_STATE_MIN_CONTRAST`, and in the light
    /// appearance the value it lands on can be a hair away from the wash the
    /// canvas would have produced. What has to hold is the base, and the base is
    /// this file's to choose.
    #[gpui::test]
    fn the_capacity_table_is_painted_on_the_table_surface(cx: &mut TestAppContext) {
        // The product theme, not the base one. `DESIGN.md §3.4` describes a
        // four-level ramp and only `k8s-studio` has one: the base theme's
        // `surface.background` *is* its `background`, so a test on the base theme
        // can only ever prove that two identical values are identical.
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
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
            theme::GlobalTheme::update_theme(cx, std::sync::Arc::new(theme));
        });
        cx.update(|cx| {
            let base = capacity_table_surface(cx);
            let canvas = design::surface::canvas(cx).alpha(1.0);
            assert_eq!(
                base,
                design::surface::input(cx).alpha(1.0),
                "DESIGN.md §3.4 gives `surface` to tables, and the header band and every row \
                 read this one value"
            );
            assert_ne!(
                base, canvas,
                "the table must not be drawn on the canvas the panel behind it uses: a row base \
                 on the canvas mixes the zebra against a surface the zebra is not on"
            );
            // The overlay `design` composites for the zebra, over the base this
            // table is drawn on and over the one it is not. The two are far enough
            // apart that picking the wrong one is a visible step rather than a
            // rounding artefact, so the choice above is a real one.
            let overlay = cx.theme().colors().text.opacity(design::ROW_STRIPE_ALPHA);
            let stripe_over_this_table = design::composite_surface(base, overlay);
            let stripe_over_canvas = design::composite_surface(canvas, overlay);
            assert_ne!(
                stripe_over_this_table, stripe_over_canvas,
                "the two candidate bases give different zebras, so the base above decides what \
                 a striped row looks like"
            );
            assert_ne!(
                stripe_over_this_table, base,
                "and either way the zebra is a step away from the rows it alternates with"
            );
        });
    }

    /// The banner has to read as caution, not as a disabled slab: the fill is a
    /// wash on the canvas, and the glyph is the shape that means warning.
    #[gpui::test]
    fn the_banner_wash_and_glyph_follow_the_severity(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            let canvas = design::surface::canvas(cx);
            for severity in [Severity::Success, Severity::Warning, Severity::Error] {
                let wash = health_wash(severity, cx);
                assert_ne!(
                    wash, canvas,
                    "the wash has to be visible against the canvas it sits on"
                );
            }
            assert_ne!(
                health_wash(Severity::Warning, cx),
                health_wash(Severity::Error, cx)
            );
            // The banner used to hard-code a warning glyph to work around the
            // shared mapping. Assert the shared vocabulary now carries it, so
            // the workaround cannot be reintroduced.
            assert_eq!(health_banner_icon(Severity::Warning), IconName::Warning);
            assert_eq!(health_banner_icon(Severity::Success), IconName::Check);
            assert_eq!(health_banner_icon(Severity::Error), IconName::XCircleFilled);
            for severity in [Severity::Success, Severity::Warning, Severity::Error] {
                assert_eq!(
                    health_banner_icon(severity),
                    design::health_icon(severity),
                    "the banner must not diverge from the shared health vocabulary"
                );
            }
        });
    }

    /// Observation confidence is a second channel, not a second shade of the
    /// first one.
    ///
    /// A cluster the app could not read has no health verdict, so it has to be
    /// marked as undetermined rather than reported as fine; a snapshot that is
    /// being replaced is not yet wrong, so it is marked as stale. A current
    /// answer draws no marker at all, because the health hue beside it already
    /// spoke and a second mark would imply a second thing to read.
    #[gpui::test]
    fn the_confidence_channel_answers_separately_from_health(cx: &mut TestAppContext) {
        use design::Confidence;
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let sample = || {
            overview(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            )
        };
        let answered = OverviewState::Ready(sample());
        assert_eq!(
            snapshot_confidence(&answered, false, false),
            Confidence::Known
        );
        assert_eq!(
            snapshot_confidence(&answered, true, false),
            Confidence::Stale,
            "a refresh in flight leaves the previous answer on screen"
        );
        assert_eq!(
            snapshot_confidence(&answered, false, true),
            Confidence::Stale,
            "a failed refresh leaves the last good answer on screen"
        );
        assert_eq!(
            snapshot_confidence(&OverviewState::Loading, false, false),
            Confidence::Unknown
        );
        assert_eq!(
            snapshot_confidence(
                &OverviewState::Failed("no route to host".to_owned()),
                false,
                false
            ),
            Confidence::Unknown
        );

        // A source the app could not read leaves health undetermined whatever the
        // reason was: a denial is not a connection failure, but neither of them is
        // a healthy cluster.
        for reason in [
            "request timed out after 8s",
            "Failed to list pods: (Status { code: 403 })",
        ] {
            let unreadable = Overview {
                unavailable_sources: (0..OVERVIEW_SOURCE_COUNT)
                    .map(|_| UnavailableSource {
                        source: "source",
                        reason: reason.to_owned(),
                    })
                    .collect(),
                ..Overview::default()
            };
            assert_eq!(
                snapshot_confidence(&OverviewState::Ready(unreadable), false, false),
                Confidence::Unknown,
                "{reason}"
            );
        }

        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(sample());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-confidence").is_none(),
            "a current answer needs no confidence mark"
        );

        // An empty snapshot is a cluster the app learned nothing about, which is
        // not the same claim as a healthy cluster.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(Overview::default());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            snapshot_confidence(&OverviewState::Ready(Overview::default()), false, false),
            Confidence::Unknown
        );
        assert!(
            cx.debug_bounds("overview-confidence").is_some(),
            "a cluster the app could not read says so on the banner"
        );

        // A partial snapshot is the case that used to be silent: the banner was
        // amber about a permission problem, and the confidence channel reported
        // `Known` because the sources that did answer gave a current answer.
        let mut partial = sample();
        partial.unavailable_sources = vec![UnavailableSource {
            source: "statefulsets",
            reason: "403 Forbidden".to_owned(),
        }];
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(partial.clone());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            snapshot_confidence(&OverviewState::Ready(partial), false, false),
            Confidence::Unknown,
            "the app has no verdict about the source it was denied"
        );
        assert!(
            cx.debug_bounds("overview-confidence").is_some(),
            "a partial snapshot draws the hollow mark the health channel no longer draws for it"
        );
    }

    /// The overcommit state is three channels, not one: amber ink, a glyph, and
    /// the words.
    ///
    /// The cell used to return a `Color::Warning` and nothing else, and the cell
    /// handed that colour to its own `aria_label` value, so the cell announced
    /// `1.15 / 20 cores` and the fact that the node is oversubscribed did not
    /// exist for anyone who cannot see amber.
    #[gpui::test]
    fn an_oversubscribed_request_has_a_glyph_and_words(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, true, cx));
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(oversubscribed_node());
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(1_280.),
                height: px(800.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();

        // The CPU axis is over: the cell carries a health glyph, from the shared
        // vocabulary, in front of its figure.
        let glyph = cx
            .debug_bounds("overview-capacity-severity-0-1")
            .expect("the oversubscribed CPU request draws a severity glyph");
        let cell = cx
            .debug_bounds("overview-capacity-cell-0-1")
            .expect("the CPU request cell");
        assert!(
            left_edge(glyph) >= left_edge(cell) - 0.5 && left_edge(glyph) < left_edge(cell) + 20.,
            "the glyph leads the figure: glyph {glyph:?} cell {cell:?}"
        );
        assert!(
            cx.debug_bounds("overview-capacity-severity-0-2").is_none(),
            "the memory axis is not over, so its cell states nothing"
        );
    }

    /// Header alignment follows the data it names, and the panel's sections share
    /// one left edge that clears the group frame.
    #[gpui::test]
    fn the_capacity_header_follows_its_data_and_the_sections_share_one_edge(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (_root, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| OverviewView::new(None, true, cx));
            view.update(cx, |view, cx| {
                view.state = OverviewState::Ready(overview_with_metrics(
                    HealthSummary {
                        total_pods: 2,
                        running: 2,
                        ..HealthSummary::default()
                    },
                    NodeSummary {
                        count: 2,
                        ready: 2,
                        not_ready: 0,
                    },
                ));
                cx.notify();
            });
            SizedOverview {
                view,
                width: px(1_280.),
                height: px(800.),
            }
        });
        cx.run_until_parked();
        cx.run_until_parked();

        // Every numeric heading ends where its column's data ends. The selectors
        // are literals because the test context looks them up by `&'static str`.
        for column in 1..CAPACITY_COLUMNS.len() {
            let header = cx
                .debug_bounds(HEADER_CELLS[column])
                .unwrap_or_else(|| panic!("the heading for column {column}"));
            let label = cx
                .debug_bounds(HEADER_LABELS[column])
                .unwrap_or_else(|| panic!("the heading label for column {column}"));
            let cell = cx
                .debug_bounds(ROW_CELLS[column])
                .unwrap_or_else(|| panic!("the cell for column {column}"));
            assert!(
                (right_edge(label) - right_edge(cell)).abs() < 1.5,
                "{} is right-aligned in its data and left-aligned in its heading: label {:?} cell {:?}",
                CAPACITY_COLUMNS[column],
                label,
                cell
            );
            assert!(
                (right_edge(header) - right_edge(cell)).abs() < 0.5,
                "the heading and the data share the column's trailing edge"
            );
        }
        // The node column stays left-aligned: the name is text, not a number.
        let node_label = cx.debug_bounds(HEADER_LABELS[0]).expect("the node heading");
        let node_cell = cx.debug_bounds(ROW_CELLS[0]).expect("the node cell");
        assert!((left_edge(node_label) - left_edge(node_cell)).abs() < 1.5);

        // The three sections of the group start on one line, and clear the frame.
        let group = cx.debug_bounds("overview-body").expect("the group");
        let hero = cx.debug_bounds("overview-hero").expect("the hero");
        let table = cx
            .debug_bounds("overview-capacity-table")
            .expect("the capacity table");
        assert!(
            (left_edge(hero) - left_edge(table)).abs() < 1.0,
            "the vitals and the table start on one line: hero {hero:?} table {table:?}"
        );
        let inset = left_edge(hero) - left_edge(group);
        assert!(
            inset >= f32::from(space::LG),
            "the text clears the group frame by the LG inset, not by a pixel: {inset}px"
        );
        assert!(
            inset <= f32::from(space::LG) + 2.0,
            "and by no more than that either: {inset}px"
        );

        // The sections are separated by more than the line that separates them.
        let rule = cx
            .debug_bounds("overview-section-rule-1")
            .expect("the first section rule");
        let banner = cx.debug_bounds("overview-health").expect("the banner");
        let vitals = cx.debug_bounds("overview-vitals").expect("the vitals");
        let above = f32::from(rule.origin.y) - (f32::from(banner.origin.y + banner.size.height));
        let below = f32::from(vitals.origin.y) - f32::from(rule.origin.y + rule.size.height);
        assert!(
            (above - f32::from(space::SM)).abs() < 1.0
                && (below - f32::from(space::SM)).abs() < 1.0,
            "a section boundary is more than the line that draws it: {above}px above, {below}px below"
        );
    }

    /// The five workload figures are one itemisation, not five numbers spread
    /// across a wide panel, and they fit on one row at the narrowest panel.
    #[gpui::test]
    fn the_workload_grid_is_compact(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let mut snapshot = overview_with_metrics(
            HealthSummary {
                total_pods: 12,
                running: 12,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
        );
        snapshot.workloads.deployments = ReplicaSummary {
            desired: 10_004,
            available: 104,
        };
        // The root view is built at a chosen width, so the panel is measured at
        // that width rather than at whatever the test window happens to be.
        macro_rules! overview_window {
            ($cx:ident, $width:expr, $snapshot:expr) => {{
                let view = $cx.new(|inner| OverviewView::new(None, false, inner));
                view.update($cx, |view, cx| {
                    view.state = OverviewState::Ready($snapshot);
                    cx.notify();
                });
                SizedOverview {
                    view,
                    width: px($width),
                    height: px(800.),
                }
            }};
        }
        // A `104 / 10,004` figure is 12 characters of data font, which is the
        // width its own cell has to be able to hold before it ellipsises. The
        // cells are content-width, so each one is measured against the figure it
        // holds: the `—` cells are as narrow as their label and have nothing to
        // clip.
        //
        // The five kinds against five columns is the shape this change created, so
        // it is pinned: with the column count equal to the kind count the grid
        // builds one row and never reaches the filler branch, so the five cells
        // below are all the slots there are. Raising the column count is what
        // brings four empty `flex_none` slots back, and this fails first.
        assert_eq!(WORKLOAD_COLUMNS_COMPACT, WORKLOAD_LABELS.len());
        assert_eq!(WORKLOAD_COLUMNS_WIDE, WORKLOAD_LABELS.len());
        let cells = || {
            [
                ("overview-workload-cell-Deployments", "104 / 10,004"),
                ("overview-workload-cell-Stateful-sets", NO_ANSWER),
                ("overview-workload-cell-Daemon-sets", NO_ANSWER),
                ("overview-workload-cell-Jobs", NO_ANSWER),
                ("overview-workload-cell-Cron-jobs", NO_ANSWER),
            ]
        };
        let figure_width = |figure: &str| {
            f32::from(crate::settings::default_columns(
                figure.chars().count() as f32
            ))
        };
        let assert_cells_hold_their_figures = |cx: &mut gpui::VisualTestContext| {
            for (selector, figure) in cells() {
                let cell = cx
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("the {selector} cell"));
                assert!(
                    f32::from(cell.size.width) >= figure_width(figure),
                    "a cell holds its figure rather than an ellipsis: {}px cell, {}px figure {figure:?}",
                    f32::from(cell.size.width),
                    figure_width(figure)
                );
            }
        };
        let first_cell = || cells()[0].0;
        let last_cell = || cells()[cells().len() - 1].0;

        let wide = snapshot.clone();
        let (_root, cx) = cx.add_window_view(|_, cx| overview_window!(cx, 1_280., wide));
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let first = cx.debug_bounds(first_cell()).expect("the first kind");
        let last = cx.debug_bounds(last_cell()).expect("the last kind");
        let span = right_edge(last) - left_edge(first);
        assert!(
            span < f32::from(panel.size.width) * 0.5,
            "the five pairs span {span}px inside a {}px panel: they are itemised, not columns",
            f32::from(panel.size.width)
        );
        assert!(
            right_edge(last) < right_edge(panel),
            "and they stay inside the panel"
        );
        assert_cells_hold_their_figures(cx);

        // A centre panel at the minimum window still holds all five kinds on one
        // row, with room for the figure itself. Three columns was a full step too
        // conservative: it wrapped one kind onto a row of its own and left two
        // empty slots.
        let narrow = snapshot.clone();
        let (_root, cx) = cx.add_window_view(|_, cx| overview_window!(cx, 700., narrow));
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let first = cx.debug_bounds(first_cell()).expect("the first kind");
        let last = cx.debug_bounds(last_cell()).expect("the last kind");
        assert!(
            right_edge(last) <= right_edge(panel) + 0.5,
            "five kinds fit the narrow centre panel: {}px of cells in a {}px panel",
            right_edge(last) - left_edge(first),
            f32::from(panel.size.width)
        );
        assert!(
            (f32::from(last.origin.y) - f32::from(first.origin.y)).abs() < 1.0,
            "and they share one row rather than wrapping one kind onto a row of its own"
        );
        assert_cells_hold_their_figures(cx);

        // The floor of the shell's centre panel: the cells shrink, and the row
        // still fits inside the panel instead of spilling out of it.
        let floor = f32::from(design::size::CENTER_MIN);
        let (_root, cx) = cx.add_window_view(|_, cx| overview_window!(cx, floor, snapshot));
        cx.run_until_parked();
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let first = cx.debug_bounds(first_cell()).expect("the first kind");
        let last = cx.debug_bounds(last_cell()).expect("the last kind");
        assert!(
            right_edge(last) <= right_edge(panel) + 0.5,
            "the row shrinks to the panel rather than spilling out of it: {}px of cells in a {}px panel",
            right_edge(last) - left_edge(first),
            f32::from(panel.size.width)
        );
        assert!(
            (f32::from(last.origin.y) - f32::from(first.origin.y)).abs() < 1.0,
            "and the five kinds stay on one row"
        );
        assert_cells_hold_their_figures(cx);
    }

    /// The panel's biggest number is the one a reader can follow.
    #[gpui::test]
    fn the_pod_caption_is_a_route_to_the_pods_it_counts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let mut degraded = overview(
            HealthSummary {
                total_pods: 10_010,
                running: 110,
                pending: 9_900,
                failed: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Warning,
        );
        degraded.unknown_pods = 3;
        let (hero, _) = vital_figures(&degraded);
        assert_eq!(
            hero.caption, "9,900 pending · 7",
            "the caption names the largest bucket and counts the rest, so the 4 failed and 3 unknown pods are not invisible"
        );
        assert!(hero.actionable);

        // Without a host route the caption is quiet text.
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(degraded.clone());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-show-problems").is_none(),
            "a control with no route to the rows is a lie"
        );

        // With one, it is a control in the tab order.
        let called: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counter = called.clone();
        let show: ShowProblemsCallback = Rc::new(move |_window, _cx| {
            counter.set(counter.get() + 1);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(degraded);
            view.set_show_problems_callback(Some(show));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("overview-show-problems").is_some(),
            "the caption is the one piece of text that names the bucket, so it opens the pods in it"
        );
        assert_eq!(called.get(), 0);
        // It is a working control, not a painted one. The callback is the only
        // thing standing between a number the page chose to report and the rows
        // behind it, and a disabled or unwired control reads as a promise.
        let control = cx
            .debug_bounds("overview-show-problems")
            .expect("the caption control");
        cx.simulate_click(control.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(called.get(), 1, "the caption opens the pods it counts");

        // A cluster with nothing to follow does not get dressed up as a control.
        let healthy = overview(
            HealthSummary {
                total_pods: 4,
                running: 4,
                ..HealthSummary::default()
            },
            NodeSummary {
                count: 1,
                ready: 1,
                not_ready: 0,
            },
            HealthLevel::Healthy,
        );
        let (hero, _) = vital_figures(&healthy);
        assert_eq!(hero.caption, "all running");
        assert!(!hero.actionable);
        let show: ShowProblemsCallback = Rc::new(|_window, _cx| {});
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(healthy);
            view.set_show_problems_callback(Some(show));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("overview-show-problems").is_none());
    }

    /// The toolbar's own name is a title, and the refresh status is one statement
    /// with the button it belongs to.
    #[gpui::test]
    fn the_toolbar_title_and_refresh_status_belong_together(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| OverviewView::new(None, false, cx));
        view.update(cx, |view, cx| {
            view.state = OverviewState::Ready(overview(
                HealthSummary {
                    total_pods: 1,
                    running: 1,
                    ..HealthSummary::default()
                },
                NodeSummary {
                    count: 1,
                    ready: 1,
                    not_ready: 0,
                },
                HealthLevel::Healthy,
            ));
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1_440.), px(900.)));
        cx.run_until_parked();
        let panel = cx
            .debug_bounds("overview-content-bounds")
            .expect("the panel");
        let title = cx
            .debug_bounds("overview-toolbar-title")
            .expect("the toolbar title");
        let status = cx
            .debug_bounds("overview-refresh-status")
            .expect("the refresh status");
        let refresh = cx.debug_bounds("overview-refresh").expect("refresh");
        assert!(
            f32::from(title.size.height) > f32::from(status.size.height),
            "the toolbar title uses the panel-title role, not the body size: title {title:?} status {status:?}"
        );
        assert!(
            left_edge(refresh) - right_edge(status) <= f32::from(space::SM) + 0.5,
            "the timestamp sits against the button it describes: status {:?} refresh {:?}",
            status,
            refresh
        );
        assert!(
            right_edge(panel) - right_edge(refresh) <= f32::from(space::SM) + 0.5,
            "and the button keeps the toolbar's trailing edge"
        );
    }

    /// Every column heading is a sentence-case noun phrase, like everything else
    /// on the panel.
    ///
    /// Sentence case is a property of the words, not of the first letter: the
    /// first one is capitalised, the rest are lower case, and an acronym is
    /// either all capitals or it is not an acronym. `CPU Requests` fails on the
    /// second word, which is the mistake title case actually makes.
    #[test]
    fn the_capacity_headings_are_sentence_case() {
        assert_eq!(
            CAPACITY_COLUMNS,
            [
                "Node",
                "CPU requests",
                "Memory requests",
                "CPU limits",
                "Memory limits",
                "CPU now",
                "Memory now",
            ]
        );
        for label in CAPACITY_COLUMNS {
            let mut words = label.split(' ').peekable();
            let first = words.next().unwrap_or_default();
            assert!(
                first.starts_with(char::is_uppercase),
                "{label} starts with a lower-case word, and the panel is sentence case"
            );
            for word in words {
                assert!(
                    word.starts_with(char::is_lowercase) || word.chars().all(char::is_uppercase),
                    "{word} in {label:?} is title case, and the panel is sentence case"
                );
            }
        }
        assert!(
            CAPACITY_COLUMNS[1].starts_with("CPU"),
            "the acronym stays uppercase"
        );
    }
}
