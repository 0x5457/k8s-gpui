//! Helm release list, details, and actions.
//! The panel reads release data and coordinates Helm actions.

use std::cell::Cell;
use std::cmp::Ordering;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, ListHorizontalSizingBehavior, ParentElement, Pixels, Render, Role, ScrollHandle,
    ScrollStrategy, SharedString, Styled, Task, UniformListScrollHandle, WeakEntity, Window, div,
    px, uniform_list,
};
use k8s_core::fuzzy::{CasePolicy, Ranker};
use k8s_core::helm::{Helm, HelmError, Release, ReleaseDetail, ReleaseRevision, ReleaseStatus};
use ui::prelude::*;
use ui::{
    ContextMenu, ContextMenuEntry, DocumentationSide, PopoverMenu, PopoverMenuHandle, TintColor,
    Tooltip,
};

use crate::design::{self, Confidence, Severity, space};
use crate::panels::common::{self, empty_state_with_action, status_message};
use crate::session::TextInput;
use crate::settings::{self, DataTypography};

const NAME_COLUMN_WIDTH: f32 = 180.0;
const NAMESPACE_COLUMN_WIDTH: f32 = 120.0;
const CHART_COLUMN_WIDTH: f32 = 160.0;
const STATUS_COLUMN_WIDTH: f32 = 110.0;
const REVISION_COLUMN_WIDTH: f32 = 70.0;
const UPDATED_COLUMN_WIDTH: f32 = 170.0;
const RELEASE_TABLE_CONTENT_WIDTH: f32 = 880.0;
const DETAIL_WIDE_WIDTH: f32 = 320.0;
const DETAIL_MIN_VIEWPORT: f32 = 1180.0;
const ACTION_BUTTON_WIDTH: f32 = 96.0;
const ACTION_MENU_BUTTON_WIDTH: f32 = 88.0;
const RETRY_BUTTON_WIDTH: f32 = 72.0;
const HELM_LIST_TIMEOUT: Duration = Duration::from_secs(15);
const HELM_DETAIL_TIMEOUT: Duration = Duration::from_secs(15);
const HELM_ACTION_TIMEOUT: Duration = Duration::from_secs(30);
/// First tab stop of the detail section buttons. The retry and the action bar
/// follow the section count, so one number keeps them from colliding.
const DETAIL_SECTION_TAB_INDEX: isize = 6;
/// How often the running action refreshes its elapsed time.
const HELM_ELAPSED_TICK: Duration = Duration::from_millis(500);
/// Tab stop of the cancel button shown while an action runs.
const HELM_CANCEL_TAB_INDEX: isize = 20;

/// Message for a missing Helm CLI.
pub const HELM_NOT_INSTALLED: &str =
    "The Helm CLI is not on PATH. Install the Helm CLI, or add its directory to PATH.";
pub const HELM_CHECKING: &str = "Checking whether the Helm CLI is available…";
pub const HELM_PROBE_TIMED_OUT: &str =
    "The Helm CLI check timed out. Retry, or make sure the Helm CLI can start.";
pub const HELM_PROBE_FAILED: &str =
    "The Helm CLI check failed. Retry, or make sure the Helm CLI can start.";

pub const HELM_COMMAND_FAILED: &str =
    "The Helm command failed. Retry, or make sure the cluster connection works.";
pub const HELM_REQUEST_TIMED_OUT: &str =
    "The Helm request timed out. Retry, or make sure the cluster connection works.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelmCapability {
    Checking,
    Available,
    NotInstalled,
    Timeout,
    Error,
}

impl HelmCapability {
    pub fn from_error(error: &HelmError) -> Self {
        match error {
            HelmError::NotInstalled => Self::NotInstalled,
            HelmError::Timeout => Self::Timeout,
            _ => Self::Error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking…",
            Self::Available => "Available",
            Self::NotInstalled => "Not installed",
            Self::Timeout => "Timed out",
            Self::Error => "Error",
        }
    }

    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Checking => Some(HELM_CHECKING),
            Self::Available => None,
            Self::NotInstalled => Some(HELM_NOT_INSTALLED),
            Self::Timeout => Some(HELM_PROBE_TIMED_OUT),
            Self::Error => Some(HELM_PROBE_FAILED),
        }
    }
}

/// Approximate character width and margins for tooltip checks.
const CELL_CHAR_WIDTH: f32 = 7.2;
const CELL_TOOLTIP_MARGIN: f32 = 16.0;
const HISTORY_TIME_WIDTH: f32 = 140.0;

/// Keeps the user-facing message separate from the command detail.
#[derive(Clone, Debug)]
struct HelmFailure {
    message: String,
    detail: String,
}

impl HelmFailure {
    fn new(message: String, detail: String) -> Self {
        Self { message, detail }
    }

    fn not_installed() -> Self {
        Self::new(HELM_NOT_INSTALLED.to_owned(), HELM_NOT_INSTALLED.to_owned())
    }

    fn from_error(error: &HelmError) -> Self {
        if error.is_not_installed() {
            Self::not_installed()
        } else {
            Self::new(HELM_COMMAND_FAILED.to_owned(), error.to_string())
        }
    }

    fn timed_out() -> Self {
        Self::new(
            HELM_REQUEST_TIMED_OUT.to_owned(),
            "Helm request exceeded its timeout".to_owned(),
        )
    }

    fn from_capability(capability: HelmCapability, detail: Option<String>) -> Self {
        let message = capability
            .reason()
            .unwrap_or(HELM_COMMAND_FAILED)
            .to_owned();
        Self::new(message.clone(), detail.unwrap_or(message))
    }

    fn is_not_installed(&self) -> bool {
        self.message == HELM_NOT_INSTALLED
    }

    /// Writes command details to stderr.
    fn log(&self, context: &str) {
        if self.detail != self.message {
            eprintln!("k8s-gpui: {context}: {}", self.detail);
        }
    }
}

async fn with_helm_timeout<T>(
    duration: Duration,
    request: impl Future<Output = Result<T, HelmError>>,
) -> Result<T, HelmFailure> {
    match tokio::time::timeout(duration, request).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(HelmFailure::from_error(&error)),
        Err(_) => Err(HelmFailure::timed_out()),
    }
}

fn spawn_failure(error: tokio::task::JoinError) -> HelmFailure {
    HelmFailure::new(
        HELM_COMMAND_FAILED.to_owned(),
        format!("Helm request failed: {error}"),
    )
}

async fn run_helm_action(helm: &Helm, action: &HelmAction) -> Result<(), HelmFailure> {
    match action {
        HelmAction::Uninstall {
            name, namespace, ..
        } => with_helm_timeout(HELM_ACTION_TIMEOUT, helm.uninstall(name, namespace)).await,
        HelmAction::Rollback {
            name,
            namespace,
            revision,
            ..
        } => {
            let revision = u32::try_from(*revision).unwrap_or(0);
            with_helm_timeout(
                HELM_ACTION_TIMEOUT,
                helm.rollback(name, namespace, revision),
            )
            .await
        }
        HelmAction::Upgrade {
            name,
            namespace,
            chart,
            ..
        } => with_helm_timeout(HELM_ACTION_TIMEOUT, helm.upgrade(chart, name, namespace))
            .await
            .map(|_| ()),
    }
}

fn detail_result_is_current(
    selected: Option<&Release>,
    current_generation: u64,
    result_generation: u64,
    name: &str,
    namespace: &str,
) -> bool {
    current_generation == result_generation
        && selected.is_some_and(|release| release.name == name && release.namespace == namespace)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HelmLayout {
    show_detail: bool,
    stacked_detail: bool,
    detail_width: f32,
    list_width: f32,
}

impl HelmLayout {
    fn for_content_width(content_width: f32, has_selection: bool) -> Self {
        let show_detail = has_selection;
        let stacked_detail = show_detail && content_width < DETAIL_MIN_VIEWPORT;
        let detail_width = if show_detail && !stacked_detail {
            DETAIL_WIDE_WIDTH
        } else {
            0.0
        };
        let list_width = if stacked_detail {
            content_width.max(0.0)
        } else {
            (content_width - detail_width).max(0.0)
        };
        Self {
            show_detail,
            stacked_detail,
            detail_width,
            list_width,
        }
    }
}

fn release_table_overflow(list_width: f32, has_rows: bool) -> f32 {
    if has_rows {
        (RELEASE_TABLE_CONTENT_WIDTH - list_width).max(0.0)
    } else {
        0.0
    }
}

fn focused_release_index(
    selected: Option<usize>,
    list_focused: bool,
    release_count: usize,
) -> Option<usize> {
    if !list_focused || release_count == 0 {
        return None;
    }
    Some(
        selected
            .filter(|index| *index < release_count)
            .unwrap_or_default(),
    )
}

/// The release row's height at the reader's configured data font size.
///
/// The row is not a fixed `size::ROW`: the cells are set in the data role, so a
/// reader who raises "Data font size" would get 18px glyphs cropped to 28px. The
/// setting's own help text promises the row grows with the text, and
/// `design::row_height` keeps the default at the `row` rhythm.
///
/// The typography is an argument so a test can raise the size without writing
/// to the reader's settings file, which is the only way to see the raised half
/// of this promise.
fn release_row_height(typography: &DataTypography) -> Pixels {
    typography.row_height()
}

/// Builds the toolbar count and its spoken form, with the noun in one number.
///
/// Both numbers go through `design::format::count`, so a release list over a
/// thousand entries reads `1,204 releases` here and `1,204` in every other count
/// in the app, and the noun is pluralised by the shared rule rather than by a
/// second hand-rolled `if total == 1`.
fn release_count_text(total: usize, matching: usize, filtering: bool) -> (String, String) {
    let releases = design::format::count_with_noun(total, "release", "releases");
    let count = if filtering {
        format!("{} / {releases}", design::format::count(matching))
    } else {
        releases.clone()
    };
    let aria = format!(
        "Helm filter results: {} of {releases} match.",
        design::format::count(matching)
    );
    (count, aria)
}

type ActionRequestHandler = Rc<dyn Fn(HelmAction, &mut Window, &mut App)>;
/// Sends success and failure notices to the shell.
type NoticeHandler = Rc<dyn Fn(String, Severity, &mut App)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelmAction {
    Uninstall {
        name: String,
        namespace: String,
        cluster: Option<String>,
    },
    Rollback {
        name: String,
        namespace: String,
        revision: i64,
        cluster: Option<String>,
    },
    Upgrade {
        name: String,
        namespace: String,
        chart: String,
        revision: String,
        cluster: Option<String>,
    },
}

impl HelmAction {
    pub fn title(&self) -> String {
        match self {
            Self::Uninstall { name, .. } => format!("Uninstall {name}?"),
            Self::Rollback { name, revision, .. } => {
                format!("Roll back {name} to revision {revision}?")
            }
            Self::Upgrade { name, .. } => format!("Upgrade {name}?"),
        }
    }

    pub fn detail(&self) -> String {
        let (namespace, cluster) = match self {
            Self::Uninstall {
                namespace, cluster, ..
            }
            | Self::Rollback {
                namespace, cluster, ..
            }
            | Self::Upgrade {
                namespace, cluster, ..
            } => (namespace, cluster),
        };
        let cluster = cluster
            .as_deref()
            .filter(|cluster| !cluster.trim().is_empty())
            .unwrap_or("the current cluster");
        let release = format!("The release is in namespace {namespace} on {cluster}.");
        match self {
            Self::Uninstall { .. } => {
                format!("{release} Uninstall removes the release history. This cannot be undone.")
            }
            Self::Rollback { revision, .. } => format!(
                "{release} Helm restores the chart version and the values from revision {revision}, not the current values. Helm creates a new revision."
            ),
            Self::Upgrade {
                chart, revision, ..
            } => {
                let chart = chart.trim();
                if chart.is_empty() {
                    format!(
                        "{release} Enter an exact chart reference. Helm resolves its chart version and reuses the current release values."
                    )
                } else {
                    format!(
                        "{release} Helm resolves chart reference {chart} and its chart version. Helm reuses the current release values. Helm creates a new revision from revision {revision}."
                    )
                }
            }
        }
    }

    pub fn confirm_label(&self) -> &'static str {
        match self {
            Self::Uninstall { .. } => "Uninstall",
            Self::Rollback { .. } => "Roll back",
            Self::Upgrade { .. } => "Upgrade",
        }
    }

    pub fn success_text(&self) -> String {
        match self {
            Self::Uninstall { name, .. } => format!("Uninstalled {name}"),
            Self::Rollback { name, revision, .. } => {
                format!("Rolled back {name} to revision {revision}")
            }
            Self::Upgrade { name, .. } => format!("Upgraded {name}"),
        }
    }

    fn working_text(&self) -> String {
        match self {
            Self::Uninstall { name, .. } => format!("Uninstalling {name}…"),
            Self::Rollback { name, revision, .. } => {
                format!("Rolling back {name} to revision {revision}…")
            }
            Self::Upgrade { name, .. } => format!("Upgrading {name}…"),
        }
    }

    fn target(&self) -> (&str, &str) {
        match self {
            Self::Uninstall {
                name, namespace, ..
            }
            | Self::Rollback {
                name, namespace, ..
            }
            | Self::Upgrade {
                name, namespace, ..
            } => (name, namespace),
        }
    }

    fn retry_aria_label(&self) -> String {
        match self {
            Self::Uninstall { name, .. } => format!("Retry uninstall for {name}"),
            Self::Rollback { name, revision, .. } => {
                format!("Retry the roll back of {name} to revision {revision}")
            }
            Self::Upgrade { name, .. } => format!("Retry upgrade for {name}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingAction {
    generation: u64,
    epoch: u64,
    action: HelmAction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HelmDetailSection {
    #[default]
    Overview,
    Chart,
    Values,
    History,
    Notes,
}

impl HelmDetailSection {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Chart,
        Self::Values,
        Self::History,
        Self::Notes,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Chart => "Chart",
            Self::Values => "Values",
            Self::History => "History",
            Self::Notes => "Notes",
        }
    }
}

/// Release operations an entry point outside the panel can start, such as a
/// command-palette row. Each one still goes through the same confirmation dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelmReleaseAction {
    Upgrade,
    Rollback,
    Uninstall,
}

impl HelmReleaseAction {
    /// Command palette and menu wording.
    pub fn command_label(self) -> &'static str {
        match self {
            Self::Upgrade => "Upgrade Selected Release",
            Self::Rollback => "Roll Back Selected Release",
            Self::Uninstall => "Uninstall Selected Release",
        }
    }

    /// The request this action builds for the selected release, or `None` when
    /// the release cannot take it. Roll back uses the newest earlier revision,
    /// and the confirmation dialog names that revision before anything runs.
    fn request(self, data: Option<DetailActionData>) -> Option<HelmAction> {
        let data = data?;
        if data.blocked.is_some() {
            return None;
        }
        match self {
            Self::Upgrade => data
                .can_upgrade
                .then(|| upgrade_request(data.name, data.namespace, data.revision, data.cluster)),
            Self::Rollback => {
                let newest = data.rollback_choices.first()?;
                Some(rollback_request(&data, newest.revision))
            }
            Self::Uninstall => Some(HelmAction::Uninstall {
                name: data.name,
                namespace: data.namespace,
                cluster: data.cluster,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseMenuTarget {
    Upgrade,
    Rollback { revision: i64 },
    Uninstall,
}

impl ReleaseMenuTarget {
    /// Menu label. Upgrade asks for a chart reference before it can run, so it carries
    /// the ellipsis that tells the user more input is needed.
    fn label(self) -> String {
        match self {
            Self::Upgrade => "Upgrade…".to_owned(),
            Self::Rollback { revision } => format!("Roll back to revision {revision}"),
            Self::Uninstall => "Uninstall".to_owned(),
        }
    }

    /// Only Uninstall removes the release, so only it uses the error semantics.
    fn is_destructive(self) -> bool {
        matches!(self, Self::Uninstall)
    }
}

/// Buttons in the release detail action bar, in display order.
///
/// The destructive action stays last and uses the error semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailAction {
    Upgrade,
    Rollback,
    Uninstall,
}

const DETAIL_ACTION_ORDER: [DetailAction; 3] = [
    DetailAction::Upgrade,
    DetailAction::Rollback,
    DetailAction::Uninstall,
];

/// One revision the release can roll back to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RollbackChoice {
    revision: i64,
    /// Chart the revision deployed, shown next to the revision in the picker.
    chart: String,
    updated: String,
}

impl RollbackChoice {
    /// Picker label. The chart stays in the label so two revisions of the same number
    /// stay apart, and the deployment date sits next to it.
    fn label(&self) -> String {
        let chart = self.chart.trim();
        if chart.is_empty() {
            format!("Revision {}", self.revision)
        } else {
            format!("Revision {} · {chart}", self.revision)
        }
    }
}

/// Everything the detail action buttons need to build one request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DetailActionData {
    name: String,
    namespace: String,
    cluster: Option<String>,
    revision: String,
    can_upgrade: bool,
    /// Newest first. The user picks the revision, so no action runs on a guess.
    rollback_choices: Vec<RollbackChoice>,
    /// Why every action is unavailable, if the release cannot take one right now.
    blocked: Option<&'static str>,
}

impl DetailAction {
    /// Menu label. Upgrade and Roll back need more input before they can run, so they
    /// carry the ellipsis that tells the user a picker or a field comes next.
    fn label(self) -> &'static str {
        match self {
            Self::Upgrade => "Upgrade…",
            Self::Rollback => "Roll back…",
            Self::Uninstall => "Uninstall",
        }
    }

    fn aria_label(self, name: &str) -> String {
        match self {
            Self::Upgrade => format!("Upgrade release {name}"),
            Self::Rollback => format!("Roll back release {name}"),
            Self::Uninstall => format!("Uninstall release {name}"),
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Upgrade => "Upgrade this release. Helm asks for the chart reference.",
            Self::Rollback => "Roll back this release. Select a revision to continue.",
            Self::Uninstall => {
                "Uninstall this release. The release history is removed and cannot be restored."
            }
        }
    }
}

/// Helm reports a release operation as pending until it settles, and Helm refuses a second
/// operation on the same release. Every action is masked while one is pending.
fn release_is_busy(status: ReleaseStatus) -> bool {
    matches!(
        status,
        ReleaseStatus::PendingInstall
            | ReleaseStatus::PendingUpgrade
            | ReleaseStatus::PendingRollback
            | ReleaseStatus::Uninstalling
    )
}

/// Why the release cannot take an action yet, or `None` when it can.
fn release_blocked_reason(status: ReleaseStatus) -> Option<&'static str> {
    if release_is_busy(status) {
        Some("Helm is still working on this release. Wait for the current operation to finish.")
    } else if status == ReleaseStatus::Uninstalled {
        Some("This release is uninstalled. Install the chart again to use it.")
    } else {
        None
    }
}

fn release_can_upgrade(release: &Release) -> bool {
    !release.revision.trim().is_empty() && release_blocked_reason(release.status).is_none()
}

fn upgrade_request(
    name: String,
    namespace: String,
    revision: String,
    cluster: Option<String>,
) -> HelmAction {
    HelmAction::Upgrade {
        name,
        namespace,
        chart: String::new(),
        revision,
        cluster,
    }
}

/// Request for one roll back picker entry. The entry keeps the release identity, so the
/// confirmation dialog names the release the user picked a revision for.
fn rollback_request(data: &DetailActionData, revision: i64) -> HelmAction {
    HelmAction::Rollback {
        name: data.name.clone(),
        namespace: data.namespace.clone(),
        revision,
        cluster: data.cluster.clone(),
    }
}

fn release_menu_targets(
    release: Option<&Release>,
    history: Option<&[ReleaseRevision]>,
) -> Vec<ReleaseMenuTarget> {
    let Some(release) = release else {
        return Vec::new();
    };
    // A pending operation already owns the release, so the menu offers nothing.
    if release_blocked_reason(release.status).is_some() {
        return Vec::new();
    }
    let current = release.revision.parse::<i64>().ok();
    let mut targets = Vec::new();
    if release_can_upgrade(release) {
        targets.push(ReleaseMenuTarget::Upgrade);
    }
    targets.extend(
        history
            .unwrap_or_default()
            .iter()
            .rev()
            .filter(|revision| revision.revision > 0 && Some(revision.revision) != current)
            .map(|revision| ReleaseMenuTarget::Rollback {
                revision: revision.revision,
            }),
    );
    targets.push(ReleaseMenuTarget::Uninstall);
    targets
}

/// Revisions the release can roll back to, newest first.
///
/// The current revision is never a target: Helm would create a new revision of itself.
fn rollback_choices(
    release: Option<&Release>,
    history: Option<&[ReleaseRevision]>,
) -> Vec<RollbackChoice> {
    let Some(release) = release else {
        return Vec::new();
    };
    if release_blocked_reason(release.status).is_some() {
        return Vec::new();
    }
    let current = release.revision.parse::<i64>().ok();
    history
        .unwrap_or_default()
        .iter()
        .rev()
        .filter(|revision| revision.revision > 0 && Some(revision.revision) != current)
        .map(|revision| RollbackChoice {
            revision: revision.revision,
            chart: revision.chart.clone(),
            updated: revision.updated.clone(),
        })
        .collect()
}

fn release_search_fields(release: &Release) -> [&str; 5] {
    [
        release.name.as_str(),
        release.namespace.as_str(),
        release.chart.as_str(),
        release.app_version.as_str(),
        release.status.as_str(),
    ]
}

fn release_matches_query(release: &Release, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut ranker = Ranker::with_case(query, CasePolicy::Ignore);
    release_search_fields(release)
        .into_iter()
        .any(|field| ranker.score(field).is_some())
}

fn ranked_release_indices(releases: &[Release], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..releases.len()).collect();
    }
    let mut ranker = Ranker::with_case(query, CasePolicy::Ignore);
    let mut matches = releases
        .iter()
        .enumerate()
        .filter_map(|(index, release)| {
            release_search_fields(release)
                .into_iter()
                .filter_map(|field| ranker.score(field))
                .min_by(|left, right| {
                    left.tier
                        .cmp(&right.tier)
                        .then_with(|| right.score.cmp(&left.score))
                })
                .map(|matched| (index, matched))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_index, left), (right_index, right)| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left_index.cmp(right_index))
    });
    matches.into_iter().map(|(index, _)| index).collect()
}

fn normalize_release_query(query: &str) -> String {
    query.trim().to_lowercase()
}

/// A column the release table can sort by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseColumn {
    Name,
    Namespace,
    Chart,
    Status,
    Revision,
    Updated,
}

impl ReleaseColumn {
    /// Columns in display order. The index is the ARIA column index.
    const ALL: [Self; 6] = [
        Self::Name,
        Self::Namespace,
        Self::Chart,
        Self::Status,
        Self::Revision,
        Self::Updated,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Namespace => "Namespace",
            Self::Chart => "Chart",
            Self::Status => "Status",
            Self::Revision => "Revision",
            Self::Updated => "Updated",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|column| *column == self)
            .unwrap_or(0)
            + 1
    }

    fn width(self) -> f32 {
        match self {
            Self::Name => NAME_COLUMN_WIDTH,
            Self::Namespace => NAMESPACE_COLUMN_WIDTH,
            Self::Chart => CHART_COLUMN_WIDTH,
            Self::Status => STATUS_COLUMN_WIDTH,
            Self::Revision => REVISION_COLUMN_WIDTH,
            Self::Updated => UPDATED_COLUMN_WIDTH,
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Revision)
    }

    /// Sort key of one release. Status sorts on the status text and the revision
    /// on its number, so the order a user sees is the order they get. A
    /// revision that is not a number is neither low nor high, so it is marked
    /// instead of standing in for the smallest one.
    fn key(self, release: &Release) -> ReleaseSortKey {
        match self {
            Self::Name => ReleaseSortKey::Text(release.name.to_lowercase()),
            Self::Namespace => ReleaseSortKey::Text(release.namespace.to_lowercase()),
            Self::Chart => ReleaseSortKey::Text(release.chart.to_lowercase()),
            Self::Status => ReleaseSortKey::Text(release.status.as_str().to_owned()),
            Self::Revision => release
                .revision
                .trim()
                .parse::<i64>()
                .map_or(ReleaseSortKey::Unreadable, ReleaseSortKey::Number),
            Self::Updated => ReleaseSortKey::Text(release.updated.clone()),
        }
    }
}

/// One column order. `None` keeps the order Helm returned, which is also the
/// order the fuzzy filter ranked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseSort {
    pub column: ReleaseColumn,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReleaseSortKey {
    Text(String),
    Number(i64),
    /// A value the column could not read as a number, such as a revision Helm
    /// reported in a shape this build does not understand.
    Unreadable,
}

/// Clicking a sorted column again reverses it, and a third click returns to the
/// order Helm returned. A sort never changes which releases match the filter.
fn next_release_sort(current: Option<ReleaseSort>, column: ReleaseColumn) -> Option<ReleaseSort> {
    match current {
        Some(sort) if sort.column == column && !sort.descending => Some(ReleaseSort {
            column,
            descending: true,
        }),
        Some(sort) if sort.column == column => None,
        _ => Some(ReleaseSort {
            column,
            descending: false,
        }),
    }
}

/// Direction of a column in the current order, for the header indicator and its
/// spoken label. GPUI has no `aria-sort`, so the direction is part of the label
/// and the description, which is what the main resource table does.
fn column_affordance(
    sort: Option<ReleaseSort>,
    column: ReleaseColumn,
) -> (Option<IconName>, &'static str) {
    match sort {
        Some(active) if active.column == column && active.descending => {
            (Some(IconName::ArrowDown), "sorted from high to low")
        }
        Some(active) if active.column == column => {
            (Some(IconName::ArrowUp), "sorted from low to high")
        }
        _ => (None, "not sorted"),
    }
}

/// Next step a click on this header takes, spoken in the header description.
fn column_next_step(sort: Option<ReleaseSort>, column: ReleaseColumn) -> &'static str {
    match sort {
        Some(active) if active.column == column && !active.descending => {
            "Click or Control+Shift+Enter to sort from high to low."
        }
        Some(active) if active.column == column => {
            "Click or Control+Shift+Enter to return to the order Helm returned."
        }
        _ => "Click or Control+Shift+Enter to sort from low to high.",
    }
}

fn header_accessibility_label(column: ReleaseColumn, sort: Option<ReleaseSort>) -> String {
    let (_, direction) = column_affordance(sort, column);
    format!("{}, {direction}", column.label())
}

fn header_accessibility_description(column: ReleaseColumn, sort: Option<ReleaseSort>) -> String {
    let (_, direction) = column_affordance(sort, column);
    format!(
        "{}. {direction}. {} Use Control+Shift+Left or Control+Shift+Right to choose another column.",
        column.label(),
        column_next_step(sort, column)
    )
}

/// Spoken summary of the current order, so the grid itself says whether a sort
/// is active.
fn table_sort_description(sort: Option<ReleaseSort>) -> String {
    match sort {
        Some(active) => format!(
            "Sorted by {} {}.",
            active.column.label(),
            if active.descending {
                "from high to low"
            } else {
                "from low to high"
            }
        ),
        None => "In the order Helm returned them.".to_owned(),
    }
}

#[derive(Default)]
struct ReleaseIndexCache {
    key: Option<(u64, String, Option<ReleaseSort>)>,
    indices: Arc<Vec<usize>>,
}

impl ReleaseIndexCache {
    fn get_or_rank(
        &mut self,
        releases: &[Release],
        releases_generation: u64,
        normalized_query: &str,
        sort: Option<ReleaseSort>,
    ) -> Arc<Vec<usize>> {
        let is_current = self
            .key
            .as_ref()
            .is_some_and(|(generation, query, active_sort)| {
                *generation == releases_generation
                    && query == normalized_query
                    && *active_sort == sort
            });
        if !is_current {
            let mut indices = ranked_release_indices(releases, normalized_query);
            if let Some(sort) = sort {
                sort_release_indices(releases, &mut indices, sort);
            }
            self.indices = Arc::new(indices);
            self.key = Some((releases_generation, normalized_query.to_owned(), sort));
        }
        Arc::clone(&self.indices)
    }

    fn invalidate(&mut self) {
        self.key = None;
        self.indices = Arc::new(Vec::new());
    }
}

/// Sorts matched releases by one column, keeping the ranked order for rows that
/// compare equal so a sort never reorders equal rows at random.
///
/// A value the column could not read sinks to the bottom in both directions: it
/// is not the lowest number, and a descending sort must not promote it to the
/// top either.
fn sort_release_indices(releases: &[Release], indices: &mut [usize], sort: ReleaseSort) {
    indices.sort_by(|left, right| {
        let left_key = sort.column.key(&releases[*left]);
        let right_key = sort.column.key(&releases[*right]);
        let ordering = if left_key == right_key {
            Ordering::Equal
        } else if left_key == ReleaseSortKey::Unreadable {
            Ordering::Greater
        } else if right_key == ReleaseSortKey::Unreadable {
            Ordering::Less
        } else if sort.descending {
            right_key.cmp(&left_key)
        } else {
            left_key.cmp(&right_key)
        };
        ordering.then_with(|| left.cmp(right))
    });
}

#[derive(Clone, Debug)]
enum LoadState<T> {
    Loading,
    Ready(T),
    Failed(HelmFailure),
}

#[derive(Clone, Debug)]
struct DetailState {
    status: LoadState<ReleaseDetail>,
    history: LoadState<Vec<ReleaseRevision>>,
    /// Read-only YAML from `helm get values`.
    values: LoadState<String>,
}

pub struct HelmView {
    helm: Option<Helm>,
    handle: Option<tokio::runtime::Handle>,
    capability: HelmCapability,
    /// `None` means the capability has not been checked.
    available: bool,
    releases: LoadState<Arc<Vec<Release>>>,
    releases_generation: u64,
    release_index_cache: ReleaseIndexCache,
    /// Column order of the release table. `None` is the order Helm returned.
    sort: Option<ReleaseSort>,
    /// Column the keyboard sort cursor is on.
    sort_column: ReleaseColumn,
    filter_input: Entity<TextInput>,
    filter_query: String,
    normalized_filter_query: String,
    selected: Option<usize>,
    detail: Option<DetailState>,
    detail_generation: u64,
    action_error: Option<HelmFailure>,
    failed_action: Option<HelmAction>,
    pending_action: Option<PendingAction>,
    action_generation: u64,
    epoch: u64,
    list_task: Option<Task<()>>,
    detail_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    /// The Helm child task, so Cancel can end it instead of only hiding it.
    /// A `JoinHandle` cannot be cloned, so this keeps the `AbortHandle` and the
    /// task that awaits the join handle owns the handle itself.
    action_handle: Option<tokio::task::AbortHandle>,
    /// Repaints the elapsed time while an action runs.
    elapsed_task: Option<Task<()>>,
    started_at: Option<Instant>,
    toolbar_scroll: ScrollHandle,
    list_scroll: UniformListScrollHandle,
    list_horizontal_scroll: ScrollHandle,
    content_width: Rc<Cell<f32>>,
    detail_scroll: ScrollHandle,
    detail_action_scroll: ScrollHandle,
    detail_section: HelmDetailSection,
    cluster: Option<String>,
    focus_handle: FocusHandle,
    action_focus: FocusHandle,
    action_menu: PopoverMenuHandle<ContextMenu>,
    rollback_menu: PopoverMenuHandle<ContextMenu>,
    on_action_requested: Option<ActionRequestHandler>,
    on_notice: Option<NoticeHandler>,
}

impl HelmView {
    pub fn new(
        helm: Option<Helm>,
        handle: Option<tokio::runtime::Handle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = cx.weak_entity();
        let filter_input = cx.new(|cx| {
            let panel = panel.clone();
            TextInput::new("Filter releases…", cx, move |text, cx| {
                let panel = panel.clone();
                let text = text.to_owned();
                cx.defer(move |cx| {
                    let _ = panel.update(cx, |view, cx| view.set_filter_query(&text, cx));
                });
            })
            .with_accessibility(
                "Filter Helm releases",
                "Match a release name, namespace, chart, version, or status. Press Escape to clear the filter.",
                "Clear Helm release filter",
            )
        });
        let mut view = Self {
            helm,
            handle,
            capability: HelmCapability::Available,
            available: true,
            releases: LoadState::Loading,
            releases_generation: 0,
            release_index_cache: ReleaseIndexCache::default(),
            sort: None,
            sort_column: ReleaseColumn::Name,
            filter_input,
            filter_query: String::new(),
            normalized_filter_query: String::new(),
            selected: None,
            detail: None,
            detail_generation: 0,
            action_error: None,
            failed_action: None,
            pending_action: None,
            action_generation: 0,
            epoch: 0,
            list_task: None,
            detail_task: None,
            action_task: None,
            action_handle: None,
            elapsed_task: None,
            started_at: None,
            toolbar_scroll: ScrollHandle::new(),
            list_scroll: UniformListScrollHandle::new(),
            list_horizontal_scroll: ScrollHandle::new(),
            content_width: Rc::new(Cell::new(0.0)),
            detail_scroll: ScrollHandle::new(),
            detail_action_scroll: ScrollHandle::new(),
            detail_section: HelmDetailSection::default(),
            cluster: None,
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(1),
            action_focus: cx.focus_handle().tab_stop(true).tab_index(2),
            action_menu: PopoverMenuHandle::default(),
            rollback_menu: PopoverMenuHandle::default(),
            on_action_requested: None,
            on_notice: None,
        };
        if view.helm.is_none() {
            view.capability = HelmCapability::NotInstalled;
            view.available = false;
            view.replace_releases(LoadState::Failed(HelmFailure::not_installed()));
        } else {
            view.refresh(cx);
        }
        view
    }

    /// Opens the confirmation dialog and starts the selected action.
    pub fn on_action_requested(
        &mut self,
        handler: impl Fn(HelmAction, &mut Window, &mut App) + 'static,
    ) {
        self.on_action_requested = Some(Rc::new(handler));
    }

    /// Sends action results to the shell.
    pub fn set_notice_handler(&mut self, handler: impl Fn(String, Severity, &mut App) + 'static) {
        self.on_notice = Some(Rc::new(handler));
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_cluster_name(&mut self, cluster: Option<String>) {
        self.cluster = cluster.filter(|cluster| !cluster.trim().is_empty());
    }

    fn notice(&self, message: String, severity: Severity, cx: &mut App) {
        if let Some(handler) = &self.on_notice {
            handler(message, severity, cx);
        }
    }

    pub fn set_capability(
        &mut self,
        capability: HelmCapability,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.capability = capability;
        if capability != HelmCapability::Available {
            self.available = false;
            self.replace_releases(LoadState::Failed(HelmFailure::from_capability(
                capability, detail,
            )));
            self.clear_selection();
        }
        cx.notify();
    }

    /// Updates the client and reloads data when availability changes.
    pub fn set_client(
        &mut self,
        helm: Option<Helm>,
        handle: Option<tokio::runtime::Handle>,
        cx: &mut Context<Self>,
    ) {
        self.epoch = self.epoch.wrapping_add(1);
        self.list_task = None;
        self.detail_task = None;
        self.action_task = None;
        self.action_handle = None;
        self.elapsed_task = None;
        self.started_at = None;
        self.pending_action = None;
        self.clear_selection();
        self.helm = helm;
        self.handle = handle;
        if self.helm.is_some() {
            self.capability = HelmCapability::Available;
            self.available = true;
            self.refresh(cx);
            return;
        }
        if self.capability == HelmCapability::Available {
            self.capability = HelmCapability::NotInstalled;
        }
        self.available = false;
        self.replace_releases(LoadState::Failed(HelmFailure::from_capability(
            self.capability,
            None,
        )));
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn request_epoch(&self) -> u64 {
        self.epoch
    }

    /// Reloads releases after opening, refreshing, or an action.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (Some(helm), Some(handle)) = (self.helm.clone(), self.handle.clone()) else {
            if self.capability == HelmCapability::Available {
                self.capability = HelmCapability::NotInstalled;
            }
            self.available = false;
            self.replace_releases(LoadState::Failed(HelmFailure::from_capability(
                self.capability,
                None,
            )));
            self.clear_selection();
            cx.notify();
            return;
        };
        let selected = self
            .selected_release()
            .map(|release| (release.name.clone(), release.namespace.clone()));
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        self.replace_releases(LoadState::Loading);
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let task = handle.spawn(async move {
                with_helm_timeout(HELM_LIST_TIMEOUT, helm.list_releases(None)).await
            });
            let result = match task.await {
                Ok(result) => result,
                Err(error) => Err(spawn_failure(error)),
            };
            if let Err(failure) = &result {
                failure.log("helm list failed");
            }
            let _ = this.update(cx, |view, cx| {
                if view.epoch != epoch {
                    return;
                }
                view.on_releases(result, selected, cx);
            });
        }));
        cx.notify();
    }

    fn on_releases(
        &mut self,
        result: Result<Vec<Release>, HelmFailure>,
        selected: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(releases) => {
                self.available = true;
                self.replace_releases(LoadState::Ready(Arc::new(releases)));
                self.selected = selected.and_then(|(name, namespace)| {
                    self.release_list()
                        .iter()
                        .position(|release| release.name == name && release.namespace == namespace)
                });
                if self.selected.is_none() {
                    self.clear_selection();
                } else {
                    self.reconcile_selection();
                }
                if self.selected.is_some() {
                    self.load_detail(cx);
                }
            }
            Err(failure) => {
                if failure.is_not_installed() {
                    self.capability = HelmCapability::NotInstalled;
                    self.available = false;
                }
                self.replace_releases(LoadState::Failed(failure));
                self.clear_selection();
            }
        }
        cx.notify();
    }

    fn replace_releases(&mut self, releases: LoadState<Arc<Vec<Release>>>) {
        self.releases_generation = self.releases_generation.wrapping_add(1);
        self.release_index_cache.invalidate();
        self.releases = releases;
    }

    fn release_list(&self) -> &[Release] {
        match &self.releases {
            LoadState::Ready(releases) => releases.as_slice(),
            _ => &[],
        }
    }

    fn visible_release_indices(&mut self) -> Arc<Vec<usize>> {
        let releases = match &self.releases {
            LoadState::Ready(releases) => releases.as_slice(),
            _ => &[],
        };
        let releases_generation = self.releases_generation;
        let normalized_query = &self.normalized_filter_query;
        let sort = self.sort;
        self.release_index_cache
            .get_or_rank(releases, releases_generation, normalized_query, sort)
    }

    fn set_filter_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.filter_query == query {
            return;
        }
        self.filter_query = query.to_owned();
        let normalized_query = normalize_release_query(query);
        if self.normalized_filter_query != normalized_query {
            self.normalized_filter_query = normalized_query;
            self.release_index_cache.invalidate();
        }
        self.reconcile_selection();
        cx.notify();
    }

    fn invalidate_detail(&mut self) {
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.detail = None;
        self.detail_section = HelmDetailSection::default();
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.invalidate_detail();
        self.action_error = None;
        self.failed_action = None;
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn reconcile_selection(&mut self) {
        if self.selected.is_none() {
            if self.detail.is_some() || self.action_error.is_some() || self.failed_action.is_some()
            {
                self.clear_selection();
            }
            return;
        }
        let visible = self.visible_release_indices();
        let row = self.selected_visible_index(&visible);
        let Some(row) = row else {
            self.clear_selection();
            return;
        };
        self.list_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
    }

    fn selected_visible_index(&self, visible: &[usize]) -> Option<usize> {
        let selected = self.selected?;
        visible.iter().position(|index| *index == selected)
    }

    fn selected_release(&self) -> Option<&Release> {
        self.selected
            .and_then(|index| self.release_list().get(index))
    }

    fn selected_visible_release(&self) -> Option<&Release> {
        let release = self.selected_release()?;
        release_matches_query(release, &self.normalized_filter_query).then_some(release)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected == Some(index) {
            return;
        }
        if !self
            .release_list()
            .get(index)
            .is_some_and(|release| release_matches_query(release, &self.normalized_filter_query))
        {
            return;
        }
        self.action_error = None;
        self.failed_action = None;
        self.selected = Some(index);
        self.invalidate_detail();
        self.load_detail(cx);
        cx.notify();
    }

    fn select_visible_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let visible = self.visible_release_indices();
        let index = visible.get(row).copied();
        if let Some(index) = index {
            self.select(index, cx);
        }
    }

    fn activate_visible_row(&mut self, cx: &mut Context<Self>) {
        let row = {
            let visible = self.visible_release_indices();
            if visible.is_empty() {
                return;
            }
            self.selected_visible_index(&visible).unwrap_or_default()
        };
        self.list_scroll
            .scroll_to_item(row, ScrollStrategy::Nearest);
        self.select_visible_row(row, cx);
    }

    /// Moves the selection with the arrow, Home, and End keys.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = {
            let visible = self.visible_release_indices();
            if visible.is_empty() {
                return;
            }
            let current = self.selected_visible_index(&visible);
            match current {
                Some(current) => current.saturating_add_signed(delta).min(visible.len() - 1),
                None if delta >= 0 => 0,
                None => visible.len() - 1,
            }
        };
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
        self.select_visible_row(next, cx);
    }

    /// Moves the sort cursor and applies the sort. The chords are Control+Shift
    /// plus an arrow or Enter: free in every keymap, and they echo the main
    /// resource table's Shift+Enter sort.
    fn on_sort_keystroke(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !event.keystroke.modifiers.control || !event.keystroke.modifiers.shift {
            return false;
        }
        match event.keystroke.key.as_str() {
            "arrowleft" | "left" => {
                let index = ReleaseColumn::ALL
                    .iter()
                    .position(|column| *column == self.sort_column)
                    .unwrap_or(0);
                let next = (index + ReleaseColumn::ALL.len() - 1) % ReleaseColumn::ALL.len();
                self.sort_column = ReleaseColumn::ALL[next];
                cx.notify();
                true
            }
            "arrowright" | "right" => {
                let index = ReleaseColumn::ALL
                    .iter()
                    .position(|column| *column == self.sort_column)
                    .unwrap_or(0);
                self.sort_column = ReleaseColumn::ALL[(index + 1) % ReleaseColumn::ALL.len()];
                cx.notify();
                true
            }
            "enter" | "return" | "space" => {
                self.toggle_sort(self.sort_column, cx);
                true
            }
            _ => false,
        }
    }

    /// Applies a new column order and keeps the selection on a visible release.
    fn toggle_sort(&mut self, column: ReleaseColumn, cx: &mut Context<Self>) {
        self.sort = next_release_sort(self.sort, column);
        self.sort_column = column;
        self.release_index_cache.invalidate();
        self.reconcile_selection();
        cx.notify();
    }

    fn sort(&self) -> Option<ReleaseSort> {
        self.sort
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_handle.is_focused(window) {
            return;
        }
        if self.on_sort_keystroke(event, cx) {
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
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
            "home" => {
                let index = self.visible_release_indices().first().copied();
                if let Some(index) = index {
                    self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    self.select(index, cx);
                    true
                } else {
                    false
                }
            }
            "end" => {
                let end = {
                    let visible = self.visible_release_indices();
                    visible
                        .len()
                        .checked_sub(1)
                        .and_then(|row| visible.get(row).copied().map(|index| (row, index)))
                };
                if let Some((row, index)) = end {
                    self.list_scroll.scroll_to_item(row, ScrollStrategy::Bottom);
                    self.select(index, cx);
                    true
                } else {
                    false
                }
            }
            "enter" | "return" => {
                self.activate_visible_row(cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn load_detail(&mut self, cx: &mut Context<Self>) {
        let (Some(helm), Some(handle)) = (self.helm.clone(), self.handle.clone()) else {
            return;
        };
        let Some(release) = self.selected_visible_release().cloned() else {
            return;
        };
        self.detail_generation = self.detail_generation.wrapping_add(1);
        let generation = self.detail_generation;
        let request_epoch = self.epoch;
        let name = release.name.clone();
        let namespace = release.namespace.clone();
        self.detail = Some(DetailState {
            status: LoadState::Loading,
            history: LoadState::Loading,
            values: LoadState::Loading,
        });
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let status_name = name.clone();
            let status_namespace = namespace.clone();
            let history_name = name.clone();
            let history_namespace = namespace.clone();
            let values_name = name.clone();
            let values_namespace = namespace.clone();
            let task = handle.spawn(async move {
                tokio::join!(
                    with_helm_timeout(
                        HELM_DETAIL_TIMEOUT,
                        helm.status(&status_name, &status_namespace)
                    ),
                    with_helm_timeout(
                        HELM_DETAIL_TIMEOUT,
                        helm.history(&history_name, &history_namespace)
                    ),
                    with_helm_timeout(
                        HELM_DETAIL_TIMEOUT,
                        helm.values(&values_name, &values_namespace, false)
                    )
                )
            });
            let (status, history, values) = match task.await {
                Ok(result) => result,
                Err(error) => {
                    let failure = spawn_failure(error);
                    (Err(failure.clone()), Err(failure.clone()), Err(failure))
                }
            };
            let _ = this.update(cx, |view, cx| {
                if view.epoch != request_epoch
                    || !detail_result_is_current(
                        view.selected_release(),
                        view.detail_generation,
                        generation,
                        &name,
                        &namespace,
                    )
                {
                    return;
                }
                view.detail = Some(DetailState {
                    status: status.map_or_else(
                        |failure| {
                            failure.log("helm status failed");
                            LoadState::Failed(failure)
                        },
                        LoadState::Ready,
                    ),
                    history: history.map_or_else(
                        |failure| {
                            failure.log("helm history failed");
                            LoadState::Failed(failure)
                        },
                        LoadState::Ready,
                    ),
                    values: values.map_or_else(
                        |failure| {
                            failure.log("helm get values failed");
                            LoadState::Failed(failure)
                        },
                        LoadState::Ready,
                    ),
                });
                cx.notify();
            });
        }));
    }

    fn action_matches_selection(&self, action: &HelmAction) -> bool {
        let Some(release) = self.selected_visible_release() else {
            return false;
        };
        // A release that Helm already reports as pending or removed cannot take any action,
        // whoever asks for it. The menu and the detail buttons are masked for this, and so is
        // every entry point, including a retry of a failed action.
        if release_blocked_reason(release.status).is_some() {
            return false;
        }
        let (name, namespace) = action.target();
        let cluster = match action {
            HelmAction::Uninstall { cluster, .. }
            | HelmAction::Rollback { cluster, .. }
            | HelmAction::Upgrade { cluster, .. } => cluster,
        };
        let action_cluster = cluster.as_deref().unwrap_or_default();
        let current_cluster = self.cluster.as_deref().unwrap_or_default();
        let revision_matches = match action {
            HelmAction::Upgrade { revision, .. } => {
                release.revision == *revision && release_can_upgrade(release)
            }
            _ => true,
        };
        release.name == name
            && release.namespace == namespace
            && action_cluster == current_cluster
            && revision_matches
    }

    fn retryable_action(&self) -> Option<HelmAction> {
        self.failed_action
            .clone()
            .filter(|action| self.action_matches_selection(action))
    }

    pub fn run_action(&mut self, action: HelmAction, cx: &mut Context<Self>) {
        let (Some(helm), Some(handle)) = (self.helm.clone(), self.handle.clone()) else {
            return;
        };
        if self.is_busy() || !self.available || !self.action_matches_selection(&action) {
            return;
        }
        self.action_generation = self.action_generation.wrapping_add(1);
        let pending = PendingAction {
            generation: self.action_generation,
            epoch: self.epoch,
            action,
        };
        self.pending_action = Some(pending.clone());
        self.action_error = None;
        self.failed_action = None;
        self.started_at = Some(Instant::now());
        let action_for_task = pending.action.clone();
        let action_handle =
            handle.spawn(async move { run_helm_action(&helm, &action_for_task).await });
        // The abort handle is kept so Cancel can end the child process, and the
        // task that awaits the result owns the join handle itself.
        self.action_handle = Some(action_handle.abort_handle());
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = match action_handle.await {
                Ok(result) => result,
                Err(error) => Err(spawn_failure(error)),
            };
            let _ = this.update(cx, |view, cx| view.on_action_finished(pending, result, cx));
        }));
        self.tick_elapsed(cx);
        cx.notify();
    }

    /// Repaints the elapsed time while an action runs, so a Helm command that
    /// takes thirty seconds does not look like one frozen spinner.
    fn tick_elapsed(&mut self, cx: &mut Context<Self>) {
        self.elapsed_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HELM_ELAPSED_TICK).await;
                let still_running = this.update(cx, |view, cx| {
                    if view.pending_action.is_some() {
                        cx.notify();
                        true
                    } else {
                        false
                    }
                });
                if !matches!(still_running, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Stops the running Helm command. The command runs with `kill_on_drop`, so
    /// dropping the task ends the child process instead of leaving it running.
    pub fn cancel_action(&mut self, cx: &mut Context<Self>) {
        if !self.is_busy() {
            return;
        }
        if let Some(task) = self.action_handle.take() {
            task.abort();
        }
        self.action_task = None;
        self.elapsed_task = None;
        self.pending_action = None;
        self.started_at = None;
        self.action_error = None;
        self.failed_action = None;
        self.notice("Helm action canceled".to_owned(), Severity::Warning, cx);
        cx.notify();
    }

    fn on_action_finished(
        &mut self,
        pending: PendingAction,
        result: Result<(), HelmFailure>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_action.as_ref() != Some(&pending) {
            return;
        }
        self.pending_action = None;
        self.action_handle = None;
        self.elapsed_task = None;
        self.started_at = None;
        let action = pending.action;
        match result {
            Ok(()) => {
                self.action_error = None;
                self.failed_action = None;
                self.notice(action.success_text(), Severity::Success, cx);
                self.refresh(cx);
            }
            Err(failure) => {
                failure.log("helm action failed");
                self.notice(failure.message.clone(), Severity::Error, cx);
                if failure.is_not_installed() {
                    self.capability = HelmCapability::NotInstalled;
                    self.available = false;
                    self.replace_releases(LoadState::Failed(failure));
                    self.clear_selection();
                } else {
                    self.action_error = Some(failure);
                    self.failed_action = Some(action);
                }
                cx.notify();
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.pending_action.is_some()
    }

    fn busy_label(&self) -> String {
        let label = self
            .pending_action
            .as_ref()
            .map(|pending| pending.action.working_text())
            .unwrap_or_else(|| "Action in progress".to_owned());
        match self.elapsed() {
            Some(elapsed) => format!("{label} {elapsed}s"),
            None => label,
        }
    }

    /// Whole seconds the running action has taken, once it has taken one.
    fn elapsed(&self) -> Option<u64> {
        self.started_at
            .map(|started| started.elapsed().as_secs())
            .filter(|seconds| *seconds > 0)
    }

    fn request(&self, action: HelmAction, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_busy()
            && self.available
            && self.action_matches_selection(&action)
            && let Some(handler) = &self.on_action_requested
        {
            handler(action, window, cx);
        }
    }

    /// True when this panel can start `action` on its selected release now.
    pub fn can_start(&self, action: HelmReleaseAction) -> bool {
        !self.is_busy() && self.available && action.request(self.detail_action_data()).is_some()
    }

    /// Starts a release operation from outside the panel, such as a command
    /// palette row. It opens the same confirmation dialog as the panel buttons,
    /// and reports whether the operation could start.
    pub fn request_release_action(
        &mut self,
        action: HelmReleaseAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(request) = action.request(self.detail_action_data()) else {
            return false;
        };
        self.request(request, window, cx);
        true
    }

    /// Why the panel cannot start `action`, for an entry point that stays
    /// searchable but explains itself. `None` means the operation can run now.
    pub fn blocked_reason(&self, action: HelmReleaseAction) -> Option<&'static str> {
        if !self.available {
            return self.capability.reason();
        }
        if self.is_busy() {
            return Some("A Helm action is already running. Wait for it to finish.");
        }
        let Some(data) = self.detail_action_data() else {
            return Some("Select a release in the Helm table first.");
        };
        if let Some(reason) = data.blocked {
            return Some(reason);
        }
        match action.request(Some(data)) {
            Some(_) => None,
            None => Some(match action {
                HelmReleaseAction::Upgrade => {
                    "This release cannot be upgraded. Reload the releases, then try again."
                }
                HelmReleaseAction::Rollback => {
                    "This release has no earlier revision to roll back to. Reload the history, then try again."
                }
                HelmReleaseAction::Uninstall => {
                    "This release cannot be uninstalled. Reload the releases, then try again."
                }
            }),
        }
    }

    // Rendering

    fn render_action_menu(&self, cx: &Context<Self>) -> AnyElement {
        let release = self.selected_visible_release();
        let history = self.detail.as_ref().and_then(|detail| {
            if let LoadState::Ready(history) = &detail.history {
                Some(history.as_slice())
            } else {
                None
            }
        });
        let targets = release_menu_targets(release, history);
        let busy = self.is_busy();
        let panel = cx.entity().downgrade();
        let name = release
            .map(|release| release.name.clone())
            .unwrap_or_default();
        let namespace = release
            .map(|release| release.namespace.clone())
            .unwrap_or_default();
        let revision = release
            .map(|release| release.revision.clone())
            .unwrap_or_default();
        let cluster = self.cluster.clone();
        let blocked = release.and_then(|release| release_blocked_reason(release.status));
        let action_label = release
            .map(|release| format!("Actions for {}", release.name))
            .unwrap_or_else(|| "No release selected".to_owned());
        let menu_label = match blocked {
            Some(reason) => reason.to_owned(),
            None => action_label.clone(),
        };
        PopoverMenu::new("helm-release-actions")
            .with_handle(self.action_menu.clone())
            .menu(move |window, cx| {
                let panel = panel.clone();
                let name = name.clone();
                let namespace = namespace.clone();
                let revision = revision.clone();
                let cluster = cluster.clone();
                let blocked = blocked.map(str::to_owned);
                let has_other_actions = targets.len() > 1;
                let menu_targets = targets.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    // A release that cannot take an action says why instead of offering
                    // a list of actions that Helm would reject.
                    if let Some(reason) = blocked.clone() {
                        return menu.item(ContextMenuEntry::new(reason).disabled(true));
                    }
                    if menu_targets.is_empty() {
                        return menu.item(ContextMenuEntry::new("Select a Release").disabled(true));
                    }
                    menu_targets.into_iter().fold(menu, |menu, target| {
                        let panel = panel.clone();
                        let name = name.clone();
                        let namespace = namespace.clone();
                        let revision = revision.clone();
                        let cluster = cluster.clone();
                        // The destructive action is separated and kept last.
                        let menu = if target.is_destructive() && has_other_actions {
                            menu.separator()
                        } else {
                            menu
                        };
                        let entry = ContextMenuEntry::new(target.label())
                            .icon(match target {
                                ReleaseMenuTarget::Upgrade => IconName::ArrowUp,
                                ReleaseMenuTarget::Rollback { .. } => IconName::RotateCcw,
                                ReleaseMenuTarget::Uninstall => IconName::Trash,
                            })
                            .disabled(busy);
                        let entry = if target.is_destructive() {
                            entry.icon_color(Color::Error)
                        } else {
                            entry
                        };
                        menu.item(entry.handler(move |window, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |view, cx| {
                                    let action = match target {
                                        ReleaseMenuTarget::Upgrade => upgrade_request(
                                            name.clone(),
                                            namespace.clone(),
                                            revision.clone(),
                                            cluster.clone(),
                                        ),
                                        ReleaseMenuTarget::Rollback { revision } => {
                                            HelmAction::Rollback {
                                                name: name.clone(),
                                                namespace: namespace.clone(),
                                                revision,
                                                cluster: cluster.clone(),
                                            }
                                        }
                                        ReleaseMenuTarget::Uninstall => HelmAction::Uninstall {
                                            name: name.clone(),
                                            namespace: namespace.clone(),
                                            cluster: cluster.clone(),
                                        },
                                    };
                                    view.request(action, window, cx);
                                });
                            }
                        }))
                    })
                }))
            })
            .trigger(
                Button::new("helm-actions", "Actions")
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_MENU_BUTTON_WIDTH))
                    .tab_index(2isize)
                    .track_focus(&self.action_focus)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(menu_label.clone()))
                    .aria_label(menu_label)
                    // A pending operation already owns the release.
                    .disabled(busy || blocked.is_some() || release.is_none()),
            )
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &Context<Self>, matching: usize) -> AnyElement {
        let colors = cx.theme().colors();
        let total = self.release_list().len();
        let (count, count_aria) =
            release_count_text(total, matching, !self.normalized_filter_query.is_empty());
        let retry_action = self
            .action_error
            .as_ref()
            .and_then(|_| self.retryable_action());
        let busy_label = self.busy_label();
        h_flex()
            .id("helm-toolbar-scroll")
            .flex_none()
            .w_full()
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.toolbar_scroll)
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                Icon::new(IconName::Archive)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Helm Releases").size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::SECTION,
                )))),
            )
            .child(div().flex_1())
            .when(self.available, |this| {
                this.child(self.filter_input.clone()).child(
                    h_flex()
                        .id("helm-filter-status")
                        .flex_none()
                        .role(Role::Status)
                        .aria_label(count_aria)
                        .child(
                            Label::new(count)
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        ),
                )
            })
            .when_some(self.action_error.clone(), |this, failure| {
                this.child(status_message(
                    Severity::Error,
                    failure.message,
                    Some(failure.detail),
                    cx,
                ))
            })
            .when_some(retry_action, |this, action| {
                let aria_label = action.retry_aria_label();
                let busy = self.is_busy();
                let tooltip = if busy {
                    "Wait for the running action to finish."
                } else {
                    "Run this action again."
                };
                this.child(
                    Button::new("helm-action-retry", "Retry")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .size(ButtonSize::Medium)
                        .width(px(RETRY_BUTTON_WIDTH))
                        .tab_index(3isize)
                        // Retrying would start a second Helm command while one runs.
                        .disabled(busy)
                        .tooltip(Tooltip::text(tooltip))
                        .aria_label(aria_label)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.request(action.clone(), window, cx);
                        })),
                )
            })
            .when(self.is_busy(), |this| {
                this.child(
                    h_flex()
                        .id("helm-busy")
                        .gap(space::XS)
                        .items_center()
                        .role(Role::Status)
                        .aria_label(format!("Helm action in progress: {busy_label}"))
                        .child(common::spinner(
                            IconName::LoadCircle,
                            Color::Accent,
                            IconSize::XSmall,
                            cx,
                        ))
                        .child(
                            Label::new(busy_label)
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Default),
                        ),
                )
                .child(
                    // The wrapper carries the selector, because a Button exposes no debug hook.
                    div()
                        .flex_none()
                        .debug_selector(|| "helm-cancel".to_owned())
                        .child(
                            Button::new("helm-cancel", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Medium)
                                .width(px(RETRY_BUTTON_WIDTH))
                                .tab_index(HELM_CANCEL_TAB_INDEX)
                                .tooltip(Tooltip::text("Stop the running Helm command."))
                                .aria_label("Cancel the running Helm action")
                                .on_click(cx.listener(|view, _, _, cx| view.cancel_action(cx))),
                        ),
                )
            })
            .child(self.render_action_menu(cx))
            .child(
                IconButton::new("helm-refresh", IconName::RefreshTitle)
                    .size(ButtonSize::Medium)
                    .width(design::size::CONTROL)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Reload releases"))
                    .aria_label("Reload releases")
                    .tab_index(5isize)
                    .disabled(self.is_busy())
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.refresh(cx))),
            )
            .into_any_element()
    }

    fn render_list(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        list_width: f32,
        visible: Arc<Vec<usize>>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let release_count = visible.len();
        let has_rows = matches!(&self.releases, LoadState::Ready(_)) && !visible.is_empty();
        let list_focused = self.focus_handle.is_focused(window);
        let selected_visible = self.selected_visible_index(&visible);
        let focused_index = focused_release_index(selected_visible, list_focused, release_count);
        let sort = self.sort();
        let header = h_flex()
            .id("helm-release-header")
            .role(Role::Row)
            .aria_row_index(1)
            .flex_none()
            .w_full()
            .h(design::size::ROW)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .border_b_1()
            .border_color(colors.border_variant)
            .text_color(colors.text_muted)
            .children(ReleaseColumn::ALL.into_iter().map(|column| {
                let index = column.index();
                list_header(
                    column.label(),
                    column.width(),
                    column.numeric(),
                    index,
                    sort,
                    cx.listener(move |view, _, _, cx| view.toggle_sort(column, cx)),
                )
            }));

        let rows = match &self.releases {
            LoadState::Loading => vec![empty_state_with_action(
                IconName::LoadCircle,
                "Loading releases…",
                "Reading Helm releases from the cluster.",
                None,
            )],
            LoadState::Failed(failure) => {
                if !self.available {
                    vec![empty_state_with_action(
                        // The probe could not answer, which is the confidence channel and not a
                        // verdict about the cluster.
                        design::confidence::icon(load_failure_channel()),
                        self.capability.label(),
                        failure.message.clone(),
                        None,
                    )]
                } else {
                    let busy = self.is_busy();
                    let retry = Button::new("helm-retry", "Retry")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .size(ButtonSize::Medium)
                        .width(px(RETRY_BUTTON_WIDTH))
                        .tab_index(4isize)
                        // Reloading the list would replace the result of the running action.
                        .disabled(busy)
                        .tooltip(Tooltip::text(if busy {
                            "Wait for the running action to finish."
                        } else {
                            "Reload releases"
                        }))
                        .aria_label("Retry loading releases")
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.refresh(cx)))
                        .into_any_element();
                    vec![failure_state(
                        "Failed to load releases",
                        failure.clone(),
                        Some(retry),
                    )]
                }
            }
            LoadState::Ready(releases) if releases.is_empty() => vec![empty_state_with_action(
                IconName::Archive,
                "No releases",
                "No Helm releases exist in this cluster. Install a chart to create one.",
                None,
            )],
            LoadState::Ready(_) if visible.is_empty() => vec![empty_state_with_action(
                IconName::MagnifyingGlass,
                "No matching releases",
                "Clear the filter to show all releases.",
                None,
            )],
            LoadState::Ready(_) => Vec::new(),
        };
        let focus = self.focus_handle.clone();
        let mut table = v_flex()
            .id("helm-release-table")
            .role(Role::Grid)
            .aria_label("Helm releases")
            .aria_description(format!(
                "{}. Use Up and Down to select a release. Home and End move to the first and last release. \
                 Enter selects the focused release. Column widths are fixed, so a narrow panel scrolls sideways. \
                 Sort a column with Control+Shift+Enter, and choose the column with Control+Shift+Left or \
                 Control+Shift+Right.",
                table_sort_description(sort)
            ))
            .aria_keyshortcuts(
                "ArrowUp ArrowDown Home End Enter Control+Shift+Enter Control+Shift+ArrowLeft \
                 Control+Shift+ArrowRight",
            )
            .aria_row_count(release_count + if has_rows { 1 } else { 0 })
            .aria_column_count(6)
            .tab_group()
            .tab_index(1isize)
            .track_focus(&focus)
            .key_context("Helm releases")
            .on_key_down(cx.listener(Self::on_key_down))
            .h_full();
        if has_rows {
            let releases = match &self.releases {
                LoadState::Ready(releases) => releases.clone(),
                _ => Arc::new(Vec::new()),
            };
            let selected = self.selected;
            let focused_row = focused_index;
            let panel = cx.entity().downgrade();
            let rows = uniform_list(
                "helm-release-rows",
                release_count,
                move |range, _window, cx| {
                    range
                        .filter_map(|row| {
                            let index = *visible.get(row)?;
                            let release = releases.get(index)?;
                            Some(Self::render_release_row(
                                row,
                                index,
                                release,
                                selected == Some(index),
                                focused_row == Some(row),
                                &panel,
                                cx,
                            ))
                        })
                        .collect()
                },
            )
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(Some(0))
            .track_scroll(&self.list_scroll)
            .size_full();
            table = table
                .w(px(RELEASE_TABLE_CONTENT_WIDTH))
                .child(header)
                .child(
                    div()
                        .id("helm-list-scroll")
                        .flex_1()
                        .min_h(px(f32::from(design::size::ROW) * 2.))
                        .w_full()
                        .child(rows),
                );
        } else {
            table = table.w_full().min_w(px(0.)).children(rows);
        }
        let scroll_id = if release_table_overflow(list_width, has_rows) > 0.0 {
            "helm-h-scrollbar-overflow"
        } else {
            "helm-h-scrollbar-fit"
        };
        div()
            .id("helm-table-scroll")
            .debug_selector(move || format!("{scroll_id}-owner"))
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.list_horizontal_scroll)
            .child(table)
            .into_any_element()
    }

    fn render_release_row(
        row: usize,
        index: usize,
        release: &Release,
        selected: bool,
        focused: bool,
        panel: &WeakEntity<HelmView>,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let severity = release_severity(release.status);
        let row_height = release_row_height(&settings::data_typography(cx));
        h_flex()
            .id(("helm-release", index))
            .debug_selector(move || format!("helm-release-{index}"))
            .role(Role::Row)
            .aria_row_index(row + 2)
            .aria_label(format!(
                "Release {} in namespace {}. Chart {}. Status {}. Revision {}.",
                release.name,
                release.namespace,
                release.chart,
                release.status.as_str(),
                release.revision
            ))
            .aria_selected(selected)
            .when(focused, |this| this.aria_active_descendant())
            .relative()
            .w_full()
            // The row follows the data font, or a larger setting crops the glyphs
            // it just made larger. The setting's own help text promises the row
            // grows.
            .h(row_height)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .border_1()
            .border_color(if focused {
                colors.border_focused
            } else {
                colors.border_transparent
            })
            .cursor_pointer()
            .when(selected, |this| this.bg(colors.element_selected))
            .when(focused && !selected, |this| this.bg(colors.element_hover))
            .when(!selected && !focused, |this| {
                this.hover(|this| this.bg(colors.element_hover))
                    .active(|this| this.bg(colors.element_active))
            })
            .when(selected, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(design::border::FOCUS_RAIL)
                        .bg(colors.text_accent),
                )
            })
            .on_click({
                let panel = panel.clone();
                move |_, window, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |view, cx| {
                            window.focus(&view.focus_handle, cx);
                            view.select_visible_row(row, cx);
                        });
                    }
                }
            })
            .child(list_cell(
                index,
                release.name.clone(),
                NAME_COLUMN_WIDTH,
                false,
                1,
                cx,
            ))
            .child(list_cell(
                index,
                release.namespace.clone(),
                NAMESPACE_COLUMN_WIDTH,
                false,
                2,
                cx,
            ))
            .child(list_cell(
                index,
                release.chart.clone(),
                CHART_COLUMN_WIDTH,
                false,
                3,
                cx,
            ))
            .child(status_cell(
                index,
                release.status.as_str().to_owned(),
                severity,
                cx,
            ))
            .child(list_cell(
                index,
                release.revision.clone(),
                REVISION_COLUMN_WIDTH,
                true,
                5,
                cx,
            ))
            .child(list_cell(
                index,
                release.updated.clone(),
                UPDATED_COLUMN_WIDTH,
                false,
                6,
                cx,
            ))
            .into_any_element()
    }

    /// Data every detail action needs to build its request.
    fn detail_action_data(&self) -> Option<DetailActionData> {
        let release = self.selected_visible_release()?;
        let history = self.detail.as_ref().and_then(|detail| {
            if let LoadState::Ready(history) = &detail.history {
                Some(history.as_slice())
            } else {
                None
            }
        });
        Some(DetailActionData {
            name: release.name.clone(),
            namespace: release.namespace.clone(),
            cluster: self.cluster.clone(),
            revision: release.revision.clone(),
            can_upgrade: release_can_upgrade(release),
            rollback_choices: rollback_choices(Some(release), history),
            blocked: release_blocked_reason(release.status),
        })
    }

    /// Renders one detail action button. The destructive action uses the error tint and
    /// the release is named in the spoken label.
    fn render_detail_action(
        &self,
        action: DetailAction,
        data: &DetailActionData,
        cx: &Context<Self>,
    ) -> AnyElement {
        if action == DetailAction::Rollback {
            return self.render_rollback_picker(data, cx);
        }
        let busy = self.is_busy();
        let id = match action {
            DetailAction::Upgrade => "helm-detail-upgrade",
            DetailAction::Rollback => "helm-detail-rollback",
            DetailAction::Uninstall => "helm-detail-uninstall",
        };
        let tab_index = match action {
            DetailAction::Upgrade => 12isize,
            DetailAction::Rollback => 13isize,
            DetailAction::Uninstall => 14isize,
        };
        let available = match action {
            DetailAction::Upgrade => data.can_upgrade,
            DetailAction::Rollback => !data.rollback_choices.is_empty(),
            DetailAction::Uninstall => true,
        };
        let mut button = Button::new(id, action.label())
            .style(match action {
                DetailAction::Uninstall => ButtonStyle::Tinted(TintColor::Error),
                DetailAction::Upgrade => ButtonStyle::Tinted(TintColor::Accent),
                DetailAction::Rollback => ButtonStyle::Subtle,
            })
            .size(ButtonSize::Medium)
            .width(px(ACTION_BUTTON_WIDTH))
            .tab_index(tab_index)
            // A busy panel already runs one action. A pending release cannot take another.
            .disabled(busy || data.blocked.is_some() || !available)
            .tooltip(Tooltip::text(
                data.blocked.unwrap_or(action.tooltip()).to_owned(),
            ))
            .aria_label(action.aria_label(&data.name));
        button = match action {
            DetailAction::Upgrade => {
                let data = data.clone();
                button.on_click(cx.listener(move |view, _, window, cx| {
                    view.request(
                        upgrade_request(
                            data.name.clone(),
                            data.namespace.clone(),
                            data.revision.clone(),
                            data.cluster.clone(),
                        ),
                        window,
                        cx,
                    );
                }))
            }
            DetailAction::Uninstall => {
                let data = data.clone();
                button.on_click(cx.listener(move |view, _, window, cx| {
                    view.request(
                        HelmAction::Uninstall {
                            name: data.name.clone(),
                            namespace: data.namespace.clone(),
                            cluster: data.cluster.clone(),
                        },
                        window,
                        cx,
                    );
                }))
            }
            DetailAction::Rollback => button,
        };
        // The wrapper carries the selector, because a Button exposes no debug hook of its own.
        div()
            .flex_none()
            .debug_selector(move || id.to_owned())
            .child(button.into_any_element())
            .into_any_element()
    }

    /// Roll back needs a revision, so the button opens a picker instead of guessing one.
    /// Every entry still goes through the confirmation dialog.
    fn render_rollback_picker(&self, data: &DetailActionData, cx: &Context<Self>) -> AnyElement {
        let busy = self.is_busy();
        let action = DetailAction::Rollback;
        // The menu closure runs later, so it owns the data it needs.
        let data = data.clone();
        let choices = data.rollback_choices.clone();
        let has_choices = !choices.is_empty();
        let trigger_label = action.aria_label(&data.name);
        let trigger_tooltip = data.blocked.unwrap_or(action.tooltip()).to_owned();
        let trigger_blocked = data.blocked;
        let panel = cx.entity().downgrade();
        let unavailable = if data.blocked.is_some() {
            "This release has no earlier revision to roll back to."
        } else {
            "No earlier revision is available. Refresh the history, then try again."
        };
        let menu = PopoverMenu::new("helm-rollback-picker")
            .with_handle(self.rollback_menu.clone())
            .menu(move |window, cx| {
                let panel = panel.clone();
                let data = data.clone();
                let choices = choices.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    if choices.is_empty() {
                        return menu.item(ContextMenuEntry::new(unavailable).disabled(true));
                    }
                    choices.into_iter().fold(menu, |menu, choice| {
                        let panel = panel.clone();
                        let data = data.clone();
                        let updated = choice.updated.clone();
                        menu.item(
                            ContextMenuEntry::new(choice.label())
                                .icon(IconName::RotateCcw)
                                .documentation_aside(DocumentationSide::Right, move |_cx| {
                                    Label::new(if updated.is_empty() {
                                        "—".to_owned()
                                    } else {
                                        updated.clone()
                                    })
                                    .size(LabelSize::Custom(rems_from_px(f32::from(
                                        design::text::METADATA,
                                    ))))
                                    .color(Color::Muted)
                                    .into_any_element()
                                })
                                .disabled(busy || data.blocked.is_some())
                                .handler(move |window, cx| {
                                    if let Some(panel) = panel.upgrade() {
                                        panel.update(cx, |view, cx| {
                                            view.request(
                                                rollback_request(&data, choice.revision),
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                        )
                    })
                }))
            })
            .trigger(
                Button::new("helm-detail-rollback", action.label())
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Medium)
                    .width(px(ACTION_BUTTON_WIDTH))
                    .tab_index(13isize)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                    .disabled(busy || trigger_blocked.is_some() || !has_choices)
                    .tooltip(Tooltip::text(trigger_tooltip))
                    .aria_label(trigger_label),
            )
            .into_any_element();
        div()
            .flex_none()
            .debug_selector(|| "helm-detail-rollback".to_owned())
            .child(menu)
            .into_any_element()
    }

    fn render_detail_actions(&self, cx: &Context<Self>) -> AnyElement {
        let Some(data) = self.detail_action_data() else {
            return div()
                .flex_none()
                .h(design::size::ROW + design::border::HIT)
                .into_any_element();
        };
        h_flex()
            .id("helm-detail-actions")
            .debug_selector(|| "helm-detail-actions".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::ROW + design::border::HIT)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.detail_action_scroll)
            .children(
                DETAIL_ACTION_ORDER
                    .into_iter()
                    .map(|action| self.render_detail_action(action, &data, cx)),
            )
            .into_any_element()
    }

    fn render_detail(&self, width: f32, stacked: bool, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        if self.selected_visible_release().is_none() {
            return v_flex()
                .flex_none()
                .w(px(width))
                .h_full()
                .bg(colors.background.alpha(1.0))
                .when(!stacked, |this| {
                    this.border_l_1().border_color(colors.border_variant)
                })
                .child(empty_state_with_action(
                    IconName::Archive,
                    "No release selected",
                    "Select a release to see its details.",
                    None,
                ))
                .into_any_element();
        }
        let sections = HelmDetailSection::ALL
            .into_iter()
            .enumerate()
            .map(|(index, section)| {
                let selected = section == self.detail_section;
                Button::new(("helm-detail-section", index), section.label())
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .tab_index(DETAIL_SECTION_TAB_INDEX + index as isize)
                    .toggle_state(selected)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .aria_label(format!("Show release {}", section.label().to_lowercase()))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.detail_section = section;
                        view.detail_scroll.scroll_to_top_of_item(0);
                        cx.notify();
                    }))
            });
        let body = match self.detail_section {
            HelmDetailSection::Overview => self.render_overview_section(cx),
            HelmDetailSection::Chart => self.render_chart_section(cx),
            HelmDetailSection::Values => self.render_values_section(cx),
            HelmDetailSection::History => self.render_history_section(cx),
            HelmDetailSection::Notes => self.render_notes_section(cx),
        };
        v_flex()
            .flex_none()
            .w(px(width))
            .h_full()
            .min_h(px(0.))
            .bg(colors.background.alpha(1.0))
            .when(!stacked, |this| {
                this.border_l_1().border_color(colors.border_variant)
            })
            .child(
                h_flex()
                    .id("helm-detail-sections")
                    .flex_none()
                    .w_full()
                    .h(design::size::ROW)
                    .px(space::SM)
                    .gap(space::XS)
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .children(sections),
            )
            .child(self.render_detail_actions(cx))
            .child(
                div()
                    .id("helm-detail-scroll")
                    .debug_selector(|| "helm-detail-scroll".to_owned())
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.detail_scroll)
                    .p(space::SM)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_overview_section(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return section_placeholder("Overview", "—", cx);
        };
        match &detail.status {
            LoadState::Loading => section_placeholder("Loading overview…", "—", cx),
            LoadState::Failed(failure) => {
                detail_failure_state(failure, "helm-overview-failure", "helm-overview-retry", cx)
            }
            LoadState::Ready(status) => {
                let mut rows = vec![
                    detail_row("First Deployed", status.info.first_deployed.clone(), cx),
                    detail_row("Last Deployed", status.info.last_deployed.clone(), cx),
                ];
                if !status.info.description.is_empty() {
                    rows.push(detail_row(
                        "Description",
                        status.info.description.clone(),
                        cx,
                    ));
                }
                section("Overview", rows, cx)
            }
        }
    }

    fn render_chart_section(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return section_placeholder("Chart", "—", cx);
        };
        match &detail.status {
            LoadState::Loading => section_placeholder("Loading chart…", "—", cx),
            LoadState::Failed(failure) => {
                detail_failure_state(failure, "helm-chart-failure", "helm-chart-retry", cx)
            }
            LoadState::Ready(status) => {
                let selected = self.selected_release();
                let name = if status.chart.metadata.name.is_empty() {
                    selected
                        .map(|release| release.chart.clone())
                        .unwrap_or_else(|| "—".to_owned())
                } else {
                    status.chart.metadata.name.clone()
                };
                let version = if status.chart.metadata.version.is_empty() {
                    "—".to_owned()
                } else {
                    status.chart.metadata.version.clone()
                };
                let app_version = if status.chart.metadata.app_version.is_empty() {
                    selected
                        .map(|release| release.app_version.clone())
                        .unwrap_or_else(|| "—".to_owned())
                } else {
                    status.chart.metadata.app_version.clone()
                };
                section(
                    "Chart",
                    vec![
                        detail_row("Chart", name, cx),
                        detail_row("Version", version, cx),
                        detail_row("App Version", app_version, cx),
                    ],
                    cx,
                )
            }
        }
    }

    fn render_history_section(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return section_placeholder("History", "—", cx);
        };
        match &detail.history {
            LoadState::Loading => section_placeholder("Loading history…", "—", cx),
            LoadState::Failed(failure) => {
                detail_failure_state(failure, "helm-history-failure", "helm-history-retry", cx)
            }
            LoadState::Ready(history) if history.is_empty() => {
                section_placeholder("No revisions yet", "Refresh to check for updates.", cx)
            }
            LoadState::Ready(history) => {
                let rows = history
                    .iter()
                    .rev()
                    .map(|revision| self.render_revision_row(revision, cx))
                    .collect();
                section("History", rows, cx)
            }
        }
    }

    fn render_notes_section(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return section_placeholder("Notes", "—", cx);
        };
        match &detail.status {
            LoadState::Loading => section_placeholder("Loading notes…", "—", cx),
            LoadState::Failed(failure) => {
                detail_failure_state(failure, "helm-notes-failure", "helm-notes-retry", cx)
            }
            LoadState::Ready(status) if status.info.notes.trim().is_empty() => {
                section_placeholder("No release notes", "Refresh to check again.", cx)
            }
            LoadState::Ready(status) => {
                let notes = v_flex()
                    .id("helm-release-notes")
                    .w_full()
                    .gap(space::XS)
                    .text_size(rems_from_px(f32::from(design::text::METADATA)))
                    .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                    .children(status.info.notes.lines().map(|line| {
                        div()
                            .min_h(design::text::METADATA_LINE_HEIGHT)
                            .child(SharedString::from(if line.is_empty() {
                                " ".to_owned()
                            } else {
                                line.to_owned()
                            }))
                    }));
                section("Notes", vec![notes.into_any_element()], cx)
            }
        }
    }

    /// Read-only values from `helm get values`. A release with no user-supplied
    /// values says so instead of showing an empty box.
    fn render_values_section(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return section_placeholder("Values", "—", cx);
        };
        match &detail.values {
            LoadState::Loading => section_placeholder("Loading values…", "—", cx),
            LoadState::Failed(failure) => {
                detail_failure_state(failure, "helm-values-failure", "helm-values-retry", cx)
            }
            LoadState::Ready(values) if values_have_no_content(values) => section_placeholder(
                "No user-supplied values",
                "Upgrade to set values for this release.",
                cx,
            ),
            LoadState::Ready(values) => {
                let typography = settings::data_typography(cx);
                let lines = v_flex()
                    .id("helm-release-values")
                    .w_full()
                    .gap(space::XS)
                    .text_size(rems_from_px(f32::from(typography.size)))
                    .line_height(rems_from_px(f32::from(typography.line_height)))
                    .children(values.lines().map(|line| {
                        div()
                            .min_h(typography.line_height)
                            .whitespace_normal()
                            .child(SharedString::from(if line.is_empty() {
                                " ".to_owned()
                            } else {
                                line.to_owned()
                            }))
                    }));
                section("Values", vec![lines.into_any_element()], cx)
            }
        }
    }

    fn render_revision_row(&self, revision: &ReleaseRevision, cx: &Context<Self>) -> AnyElement {
        let severity = release_severity(revision.status);
        let typography = settings::data_typography(cx);
        h_flex()
            .w_full()
            .min_h(design::size::ROW)
            .gap(space::XS)
            .items_center()
            .child(
                Label::new(format!("v{}", revision.revision))
                    .size(LabelSize::Custom(rems_from_px(f32::from(typography.size))))
                    .color(Color::Muted),
            )
            .child(
                Icon::new(design::severity_icon(severity))
                    .size(IconSize::XSmall)
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Label::new(revision.status.as_str()).size(LabelSize::Custom(
                        rems_from_px(f32::from(design::text::METADATA)),
                    )))
                    .child(history_time(revision.revision, &revision.updated)),
            )
            .into_any_element()
    }
}

/// `helm get values` prints `null` for a release with no user-supplied values, so
/// an empty document is reported instead of rendered as a blank panel.
fn values_have_no_content(values: &str) -> bool {
    let trimmed = values.trim();
    trimmed.is_empty() || trimmed == "null" || trimmed == "{}"
}

/// Which channel a Helm load failure belongs to.
///
/// `DESIGN.md` §4 keeps observation confidence and object health apart because "could not reach
/// it" and "unwell" are different facts, and the glyph is the channel a reader reads first. A
/// list that would not load says what the app could not read, so it is the confidence channel;
/// naming it once means the next empty state cannot quietly reach for `IconName::Warning` and
/// put the cluster in a state nobody observed.
fn load_failure_channel() -> Confidence {
    Confidence::Unknown
}

/// Shows a user-facing load error and a Retry action.
///
/// The glyph comes from [`load_failure_channel`], and `empty_state_with_action` already keeps the
/// muted treatment, which is the confidence end of the scale.
fn failure_state(
    title: &'static str,
    failure: HelmFailure,
    action: Option<AnyElement>,
) -> AnyElement {
    div()
        .id("helm-load-failure")
        .size_full()
        .tooltip(Tooltip::text(failure.detail))
        .child(empty_state_with_action(
            design::confidence::icon(load_failure_channel()),
            title,
            failure.message,
            action,
        ))
        .into_any_element()
}

fn detail_failure_state(
    failure: &HelmFailure,
    state_id: &'static str,
    retry_id: &'static str,
    cx: &Context<HelmView>,
) -> AnyElement {
    let retry = Button::new(retry_id, "Retry")
        .style(ButtonStyle::Tinted(TintColor::Accent))
        .size(ButtonSize::Medium)
        .width(px(RETRY_BUTTON_WIDTH))
        .tab_index(11isize)
        .aria_label("Retry loading release details")
        .on_click(cx.listener(|view, _, _, cx| view.load_detail(cx)))
        .into_any_element();
    let mut status = h_flex()
        .id(state_id)
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .gap(space::XS)
        .items_center()
        .role(Role::Status)
        .aria_label(failure.message.clone());
    status
        .interactivity()
        .tooltip(Tooltip::text(failure.detail.clone()));
    let status = status
        .child(
            Icon::new(design::severity_icon(Severity::Error))
                .size(IconSize::XSmall)
                .color(Color::Custom(Severity::Error.marker(cx))),
        )
        .child(
            Label::new(failure.message.clone())
                .size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .color(Color::Default)
                .truncate(),
        );
    h_flex()
        .w_full()
        .min_h(design::size::ROW)
        .gap(space::SM)
        .items_center()
        .child(status)
        .child(retry)
        .into_any_element()
}

/// Adds a tooltip when a history timestamp is truncated.
fn history_time(revision: i64, value: &str) -> AnyElement {
    let label = Label::new(value.to_owned())
        .size(LabelSize::Custom(rems_from_px(f32::from(
            design::text::METADATA,
        ))))
        .color(Color::Muted)
        .truncate();
    let mut cell = div()
        .id(("helm-history-time", revision.max(0) as usize))
        .w_full()
        .min_w(px(0.));
    if cell_needs_tooltip(value, HISTORY_TIME_WIDTH) {
        cell = cell.tooltip(Tooltip::text(value.to_owned()));
    }
    cell.child(label).into_any_element()
}

/// Estimates whether text needs a tooltip at the given width.
fn cell_needs_tooltip(value: &str, width: f32) -> bool {
    if value.is_empty() || width <= CELL_TOOLTIP_MARGIN {
        return false;
    }
    value.chars().count() as f32 * CELL_CHAR_WIDTH > width - CELL_TOOLTIP_MARGIN
}

fn list_header(
    label: &'static str,
    width: f32,
    numeric: bool,
    column: usize,
    sort: Option<ReleaseSort>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let release_column = ReleaseColumn::ALL
        .iter()
        .copied()
        .find(|candidate| candidate.index() == column)
        .unwrap_or(ReleaseColumn::Name);
    let (indicator, _) = column_affordance(sort, release_column);
    div()
        .id(("helm-release-header-cell", column))
        .role(Role::ColumnHeader)
        .aria_label(header_accessibility_label(release_column, sort))
        .aria_description(header_accessibility_description(release_column, sort))
        .aria_column_index(column)
        .flex_none()
        .cursor_pointer()
        .on_click(on_click)
        .when(width > 0.0, |this| this.w(px(width)))
        .when(width == 0.0, |this| this.flex_1())
        .when(numeric, |this| this.text_right())
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(label)
        .when_some(indicator, |this, icon| {
            this.child(
                div()
                    .flex_none()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Accent)),
            )
        })
        .into_any_element()
}

fn list_cell(
    row: usize,
    value: String,
    width: f32,
    numeric: bool,
    column: usize,
    cx: &App,
) -> AnyElement {
    let mut cell = div()
        .id(("helm-release-cell", (row as u64) << 16 | column as u64))
        .role(Role::Cell)
        .aria_label(value.clone())
        .aria_column_index(column)
        .min_w(px(0.));
    if width > 0.0 {
        cell = cell.flex_none().w(px(width));
    } else {
        cell = cell.flex_1();
    }
    if numeric {
        cell = cell.text_right();
    }
    // Keep the full value available when the cell is truncated.
    if cell_needs_tooltip(&value, width) {
        cell = cell.tooltip(Tooltip::text(value.clone()));
    }
    cell.child(cell_text(value, cx)).into_any_element()
}

fn status_cell(row: usize, value: String, severity: Severity, cx: &App) -> AnyElement {
    h_flex()
        .id(("helm-release-cell", (row as u64) << 16 | 4))
        .role(Role::Cell)
        .aria_label(value.clone())
        .aria_column_index(4)
        .flex_none()
        .w(px(STATUS_COLUMN_WIDTH))
        .min_w(px(0.))
        .gap(space::XS)
        .items_center()
        .child(
            Icon::new(design::severity_icon(severity))
                .size(IconSize::XSmall)
                .color(Color::Custom(severity.marker(cx))),
        )
        .child(cell_text(value, cx))
        .into_any_element()
}

/// The data role at the size the reader configured.
///
/// `design::text::DATA` is the default. `settings::data_typography` is what the
/// "Data font size" setting actually moves, and a component that reads the
/// constant stays at 12px while the resource table grows, which is the gap
/// `DESIGN.md` §3.1 calls out: these are logical values, not numbers written into
/// components. Size and line height come from the same place, so raising the size
/// also raises the row instead of cropping the glyphs.
fn cell_text(value: String, cx: &App) -> AnyElement {
    let typography = settings::data_typography(cx);
    div()
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .text_size(rems_from_px(f32::from(typography.size)))
        .line_height(rems_from_px(f32::from(typography.line_height)))
        .child(SharedString::from(value))
        .into_any_element()
}

fn section(title: &'static str, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap(space::XS)
        .child(
            h_flex()
                .w_full()
                .gap(space::SM)
                .items_center()
                .child(
                    Label::new(title)
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .flex_1()
                        .h(design::border::LINE)
                        .bg(cx.theme().colors().border_variant),
                ),
        )
        .children(rows)
        .into_any_element()
}

fn section_placeholder(title: &'static str, hint: &'static str, cx: &App) -> AnyElement {
    let typography = settings::data_typography(cx);
    v_flex()
        .w_full()
        .gap(space::XS)
        .child(
            Label::new(title)
                .size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .color(Color::Muted),
        )
        .child(
            Label::new(hint)
                .size(LabelSize::Custom(rems_from_px(f32::from(typography.size))))
                .color(Color::Muted),
        )
        .into_any_element()
}

fn detail_row(label: &'static str, value: String, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .min_h(design::size::ROW)
        .gap(space::SM)
        .items_start()
        .child(
            div()
                .w(px(88.0))
                .flex_none()
                .text_size(rems_from_px(f32::from(design::text::METADATA)))
                .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                .text_color(cx.theme().colors().text_muted)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(rems_from_px(f32::from(design::text::METADATA)))
                .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
                .text_color(cx.theme().colors().text)
                .child(value),
        )
        .into_any_element()
}

/// Maps release status to severity.
pub fn release_severity(status: ReleaseStatus) -> Severity {
    match status {
        ReleaseStatus::Deployed => Severity::Success,
        ReleaseStatus::Failed => Severity::Error,
        ReleaseStatus::PendingInstall
        | ReleaseStatus::PendingUpgrade
        | ReleaseStatus::PendingRollback
        | ReleaseStatus::Uninstalling => Severity::Warning,
        ReleaseStatus::Superseded | ReleaseStatus::Uninstalled => Severity::Muted,
        ReleaseStatus::Unknown => Severity::Neutral,
    }
}

impl Render for HelmView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let measured_width = self.content_width.get();
        let content_width = if measured_width.is_finite() && measured_width > 0.0 {
            measured_width
        } else {
            f32::from(window.viewport_size().width)
        };
        let visible = self.visible_release_indices();
        let has_selection = self.selected.is_some_and(|index| visible.contains(&index));
        let layout = HelmLayout::for_content_width(content_width, has_selection);
        let measured_width = self.content_width.clone();
        let panel = cx.entity().downgrade();
        let body = if layout.stacked_detail {
            v_flex()
                .id("helm-compact-body")
                .debug_selector(|| "helm-compact-body".to_owned())
                .flex_1()
                .h_full()
                .min_h(px(0.))
                .min_w(px(0.))
                .child(
                    div()
                        .id("helm-compact-list")
                        .debug_selector(|| "helm-compact-list".to_owned())
                        .flex_1()
                        .min_h(px(0.))
                        .min_w(px(0.))
                        .overflow_hidden()
                        .child(self.render_list(
                            window,
                            cx,
                            layout.list_width,
                            Arc::clone(&visible),
                        )),
                )
                .child(
                    div()
                        .id("helm-compact-detail")
                        .debug_selector(|| "helm-compact-detail".to_owned())
                        .flex_1()
                        .min_h(px(0.))
                        .min_w(px(0.))
                        .overflow_hidden()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(self.render_detail(content_width, true, cx)),
                )
        } else {
            h_flex()
                .id("helm-wide-body")
                .flex_1()
                .h_full()
                .min_h(px(0.))
                .min_w(px(0.))
                .child(self.render_list(window, cx, layout.list_width, Arc::clone(&visible)))
                .when(layout.show_detail, |this| {
                    this.child(self.render_detail(layout.detail_width, false, cx))
                })
        };
        let content = div()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
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
            .id("helm-content-bounds")
            .child(body);
        v_flex()
            .id("helm-view")
            .role(Role::Region)
            .aria_label("Helm")
            .size_full()
            .min_w(px(0.))
            .bg(colors.background.alpha(1.0))
            .text_color(colors.text)
            .child(self.render_toolbar(cx, visible.len()))
            .child(content)
    }
}

/// Binds a Helm client to the current context.
pub fn bind_helm(helm: &Helm, context: Option<&str>, kubeconfig_sources: &[PathBuf]) -> Helm {
    let helm = match context {
        Some(context) => helm.clone().with_kube_context(context),
        None => helm.clone(),
    };
    helm.with_kubeconfig_sources(kubeconfig_sources.iter().cloned())
}

/// Helm services supplied by the shell.
pub struct HelmServices {
    pub helm: Option<Helm>,
    pub handle: Option<tokio::runtime::Handle>,
    pub context: Option<String>,
    pub kubeconfig_sources: Vec<PathBuf>,
}

impl HelmServices {
    pub fn bind(&self) -> Option<Helm> {
        self.helm
            .as_ref()
            .map(|helm| bind_helm(helm, self.context.as_deref(), &self.kubeconfig_sources))
    }
}

/// Shared Helm services for the shell.
pub type SharedHelmServices = Arc<HelmServices>;

#[cfg(test)]
mod tests {
    use gpui::{Focusable as _, TestAppContext};
    use theme::LoadThemes;

    use super::*;

    // `super::*` brings the app's own settings store into this module, so the upstream
    // initialiser these tests need is reached as `::settings`.

    /// A list that would not load is a statement about what the app could not read.
    ///
    /// The glyph led with `IconName::Warning`, which `DESIGN.md` §4 gives to an object in a bad
    /// state. Nothing here observed a release in a bad state: the app could not reach the cluster,
    /// so the only honest reading is the confidence channel. `empty_state_with_action` keeps the
    /// muted treatment, so the shape was the whole of the claim.
    #[test]
    fn a_load_failure_leads_with_the_confidence_glyph() {
        let glyph = design::confidence::icon(load_failure_channel());
        for severity in [
            Severity::Success,
            Severity::Warning,
            Severity::Error,
            Severity::Info,
            Severity::Neutral,
            Severity::Muted,
        ] {
            assert_ne!(
                glyph,
                design::health_icon(severity),
                "{severity:?} would put the cluster in a state nobody observed"
            );
        }
        assert_ne!(
            glyph,
            IconName::Warning,
            "the shape the audit found in this empty state"
        );
        assert_eq!(
            glyph,
            design::confidence::icon(Confidence::Unknown),
            "a load that could not answer is exactly the unknown marker"
        );
    }

    fn test_release(name: &str) -> Release {
        Release {
            name: name.to_owned(),
            namespace: "default".to_owned(),
            revision: "1".to_owned(),
            updated: "2026-09-24 10:00:00 +0800".to_owned(),
            status: ReleaseStatus::Deployed,
            chart: "demo-1.0.0".to_owned(),
            app_version: "1.0.0".to_owned(),
        }
    }

    fn test_revision(revision: i64) -> ReleaseRevision {
        ReleaseRevision {
            revision,
            updated: "2026-09-24 10:00:00 +0800".to_owned(),
            status: ReleaseStatus::Superseded,
            chart: "demo-1.0.0".to_owned(),
            app_version: "1.0.0".to_owned(),
            description: String::new(),
        }
    }

    #[cfg(all(test, unix))]
    #[tokio::test]
    async fn helm_binding_passes_only_the_known_kubeconfig_source() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir =
            std::env::temp_dir().join(format!("k8s-gpui-helm-binding-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create helm binding directory");
        let binary = dir.join("helm");
        std::fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\").args\"\nprintf '%s' \"${KUBECONFIG-}\" > \"$(dirname \"$0\").env\"\nprintf '[]'\n",
        )
        .expect("write fake helm");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make fake helm executable");
        let first = dir.join("first-kubeconfig.yaml");
        let second = dir.join("second-kubeconfig.yaml");
        let helm = Helm::with_binary(&binary);
        let services = HelmServices {
            helm: Some(helm.clone()),
            handle: None,
            context: Some("beta-ctx".to_owned()),
            kubeconfig_sources: vec![first.clone(), second.clone()],
        };

        services
            .bind()
            .expect("bound helm")
            .list_releases(None)
            .await
            .expect("list releases");
        let args = std::fs::read_to_string(dir.with_extension("args")).expect("read helm args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            ["--kube-context", "beta-ctx", "list", "-A", "-o", "json"]
        );
        assert_eq!(
            std::fs::read_to_string(dir.with_extension("env")).expect("read helm env"),
            std::env::join_paths([&first, &second])
                .expect("join paths")
                .to_string_lossy()
        );

        let services = HelmServices {
            kubeconfig_sources: Vec::new(),
            ..services
        };
        services
            .bind()
            .expect("bound helm")
            .list_releases(None)
            .await
            .expect("list releases");
        let args = std::fs::read_to_string(dir.with_extension("args")).expect("read helm args");
        assert!(!args.lines().any(|arg| arg == "--kubeconfig"));
        assert!(args.lines().any(|arg| arg == "--kube-context"));
        assert_eq!(
            std::fs::read_to_string(dir.with_extension("env")).expect("read helm env"),
            ""
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn release_severity_covers_all_states() {
        assert_eq!(release_severity(ReleaseStatus::Deployed), Severity::Success);
        assert_eq!(release_severity(ReleaseStatus::Failed), Severity::Error);
        assert_eq!(
            release_severity(ReleaseStatus::PendingUpgrade),
            Severity::Warning
        );
        assert_eq!(release_severity(ReleaseStatus::Superseded), Severity::Muted);
        assert_eq!(release_severity(ReleaseStatus::Unknown), Severity::Neutral);
    }

    #[test]
    fn compact_layout_stacks_detail_without_hiding_actions() {
        let compact = HelmLayout::for_content_width(960.0, true);
        assert!(compact.show_detail);
        assert!(compact.stacked_detail);
        assert_eq!(compact.detail_width, 0.0);
        assert_eq!(compact.list_width, 960.0);
        assert_eq!(release_table_overflow(compact.list_width, true), 0.0);

        let narrow = HelmLayout::for_content_width(DETAIL_MIN_VIEWPORT - 1.0, true);
        assert!(narrow.show_detail);
        assert!(narrow.stacked_detail);
        assert_eq!(narrow.list_width, DETAIL_MIN_VIEWPORT - 1.0);

        let empty = HelmLayout::for_content_width(960.0, false);
        assert!(!empty.show_detail);
        assert!(!empty.stacked_detail);
        assert_eq!(empty.list_width, 960.0);

        let breakpoint = HelmLayout::for_content_width(DETAIL_MIN_VIEWPORT, true);
        assert!(breakpoint.show_detail);
        assert!(!breakpoint.stacked_detail);
        assert_eq!(breakpoint.detail_width, DETAIL_WIDE_WIDTH);
        assert_eq!(
            breakpoint.list_width,
            DETAIL_MIN_VIEWPORT - DETAIL_WIDE_WIDTH
        );
        assert!(release_table_overflow(breakpoint.list_width, true) > 0.0);

        let wide = HelmLayout::for_content_width(1440.0, true);
        assert!(wide.show_detail);
        assert!(!wide.stacked_detail);
        assert_eq!(release_table_overflow(wide.list_width, true), 0.0);
        let columns = NAME_COLUMN_WIDTH
            + NAMESPACE_COLUMN_WIDTH
            + CHART_COLUMN_WIDTH
            + STATUS_COLUMN_WIDTH
            + REVISION_COLUMN_WIDTH
            + UPDATED_COLUMN_WIDTH;
        assert!(RELEASE_TABLE_CONTENT_WIDTH >= columns + 7.0 * f32::from(space::SM));
    }

    #[gpui::test]
    fn compact_bounds_keep_list_detail_and_actions_reachable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        cx.simulate_resize(gpui::size(px(960.), px(640.)));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("demo")])));
            view.selected = Some(0);
            view.detail = Some(DetailState {
                status: LoadState::Loading,
                history: LoadState::Loading,
                values: LoadState::Loading,
            });
            cx.notify();
        });
        cx.run_until_parked();
        let list = cx.debug_bounds("helm-compact-list").expect("compact list");
        let detail = cx
            .debug_bounds("helm-compact-detail")
            .expect("compact detail");
        let actions = cx
            .debug_bounds("helm-detail-actions")
            .expect("detail actions");
        let detail_scroll = cx
            .debug_bounds("helm-detail-scroll")
            .expect("detail scroll");
        assert!((f32::from(list.size.width) - 960.).abs() <= 1.);
        assert!((f32::from(detail.size.width) - 960.).abs() <= 1.);
        assert!(f32::from(list.size.height) > 0.);
        assert!(f32::from(detail.size.height) > 0.);
        assert!(f32::from(list.bottom()) <= f32::from(detail.top()) + 1.);
        assert!(f32::from(actions.top()) >= f32::from(detail.top()));
        assert!(f32::from(actions.bottom()) <= f32::from(detail.bottom()) + 1.);
        assert!(f32::from(actions.left()) >= f32::from(detail.left()) - 1.);
        assert!(f32::from(actions.right()) <= f32::from(detail.right()) + 1.);
        assert!(f32::from(detail_scroll.size.height) > 0.);
    }

    #[test]
    fn focus_visibility_does_not_replace_persistent_selection() {
        assert_eq!(focused_release_index(None, true, 3), Some(0));
        assert_eq!(focused_release_index(Some(2), true, 3), Some(2));
        assert_eq!(focused_release_index(Some(2), false, 3), None);
        assert_eq!(focused_release_index(Some(8), true, 3), Some(0));
        assert_eq!(focused_release_index(Some(0), true, 0), None);
    }

    /// The release row is set in the data role, so it follows the reader's data
    /// font size instead of cropping what that setting just made larger.
    ///
    /// The promise has two halves, and the sizes the setting offers only reach
    /// one of them: at 18px a 1.5 line box is 27px, which the shared 28px row
    /// already holds, so nothing is cropped and nothing has to grow. The row
    /// grows when the data line box is taller than the shared rhythm, which is
    /// what a reader who also asks for a looser line height gets, and
    /// `row_height` is the whole mechanism.
    ///
    /// The first half reads the live accessor. The rest feed the same function
    /// raised values, because `set_data_font_size` writes to the reader's
    /// settings file.
    #[gpui::test]
    fn the_release_row_height_follows_the_data_font_setting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        assert_eq!(
            cx.update(|cx| release_row_height(&settings::data_typography(cx))),
            design::size::ROW,
            "the default data size keeps the shared row rhythm"
        );
        // Every size the Data font size menu offers, none of which may be cut.
        for size in [11., 12., 13., 14., 16., 18.] {
            let line_height = size * settings::PRODUCT_DATA_LINE_HEIGHT;
            let row = release_row_height(&settings::test_data_typography(size, line_height));
            assert!(
                row >= px(line_height),
                "a {size}px data font has to fit its own line box: {row}px row, {line_height}px line"
            );
        }
        // A looser line height is the configuration that crosses the shared
        // rhythm: 12px of data font at 2.5 is a 30px line box, and a row held at
        // 28px would cut the top and the bottom of it.
        assert_eq!(
            release_row_height(&settings::test_data_typography(
                settings::PRODUCT_DATA_FONT_SIZE,
                settings::PRODUCT_DATA_FONT_SIZE * 2.5
            )),
            px(30.),
            "a data line box taller than the shared rhythm grows the row"
        );
    }

    #[test]
    fn release_count_uses_one_noun_for_the_number() {
        assert_eq!(
            release_count_text(1, 1, false),
            (
                "1 release".to_owned(),
                "Helm filter results: 1 of 1 release match.".to_owned()
            )
        );
        assert_eq!(
            release_count_text(3, 2, true),
            (
                "2 / 3 releases".to_owned(),
                "Helm filter results: 2 of 3 releases match.".to_owned()
            )
        );
        assert_eq!(
            release_count_text(1204, 1204, false),
            (
                "1,204 releases".to_owned(),
                "Helm filter results: 1,204 of 1,204 releases match.".to_owned()
            ),
            "a release list over a thousand must carry the separator"
        );
        assert_eq!(
            release_count_text(10_004, 1_204, true).0,
            "1,204 / 10,004 releases"
        );
    }

    #[test]
    fn detail_result_requires_current_generation_and_identity() {
        let release = test_release("first");
        let mut other = test_release("first");
        other.namespace = "other".to_owned();
        assert!(detail_result_is_current(
            Some(&release),
            7,
            7,
            "first",
            "default"
        ));
        assert!(!detail_result_is_current(
            Some(&release),
            8,
            7,
            "first",
            "default"
        ));
        assert!(!detail_result_is_current(
            Some(&release),
            7,
            7,
            "second",
            "default"
        ));
        assert!(!detail_result_is_current(
            Some(&other),
            7,
            7,
            "first",
            "default"
        ));
    }

    #[tokio::test]
    async fn helm_request_timeout_is_retryable() {
        let failure = with_helm_timeout(
            Duration::ZERO,
            std::future::pending::<Result<(), HelmError>>(),
        )
        .await
        .expect_err("pending request must time out");
        assert_eq!(failure.message, HELM_REQUEST_TIMED_OUT);
        assert!(failure.message.contains("Retry"));
    }

    #[gpui::test]
    fn release_rows_are_keyboard_reachable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![
                test_release("first"),
                test_release("second"),
            ])));
            cx.notify();
        });
        cx.run_until_parked();
        let focus = view.read_with(cx, |view, _| view.focus_handle());
        assert!(focus.tab_stop);
        assert_eq!(focus.tab_index, 1);
        let action_focus = view.read_with(cx, |view, _| view.action_focus.clone());
        assert!(action_focus.tab_stop);
        assert_eq!(action_focus.tab_index, 2);
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(0));
        assert!(cx.update(|window, _| focus.is_focused(window)));
        cx.run_until_parked();
        let second = cx
            .debug_bounds("helm-release-1")
            .expect("second release row");
        cx.simulate_click(second.center(), gpui::Modifiers::none());
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(1));
        view.update(cx, |view, _| view.selected = None);
        cx.simulate_keystrokes("enter");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(0));
    }

    #[test]
    fn release_filter_uses_shared_fuzzy_across_release_fields() {
        let mut release = test_release("cache");
        release.namespace = "payments".to_owned();
        release.chart = "redis-7.2.0".to_owned();
        release.app_version = "8.4.1".to_owned();
        release.status = ReleaseStatus::Failed;
        let releases = [release];
        for query in ["CACHE", "payments", "redis-7.2.0", "8.4.1", "FAILED", "r72"] {
            assert_eq!(ranked_release_indices(&releases, query), vec![0], "{query}");
        }
        assert!(ranked_release_indices(&releases, "missing").is_empty());
    }

    #[test]
    fn release_index_cache_tracks_normalized_query_changes() {
        let releases = [test_release("cache"), test_release("gateway")];
        let mut cache = ReleaseIndexCache::default();
        let cache_query = normalize_release_query("CACHE");
        let cached = cache.get_or_rank(&releases, 1, &cache_query, None);
        assert_eq!(cached.as_slice(), &[0]);
        let equivalent_query =
            cache.get_or_rank(&releases, 1, &normalize_release_query(" cache "), None);
        assert_eq!(equivalent_query.as_slice(), &[0]);
        assert!(Arc::ptr_eq(&cached, &equivalent_query));
        let changed_query =
            cache.get_or_rank(&releases, 1, &normalize_release_query("GATEWAY"), None);
        assert_eq!(changed_query.as_slice(), &[1]);
        assert!(!Arc::ptr_eq(&cached, &changed_query));
    }

    #[test]
    fn large_release_results_are_ranked_once_by_cache() {
        let releases = (0..10_000)
            .map(|index| test_release(&format!("release-{index}")))
            .collect::<Vec<_>>();
        let query = normalize_release_query("RELEASE-");
        let mut cache = ReleaseIndexCache::default();
        let ranked = cache.get_or_rank(&releases, 1, &query, None);
        assert_eq!(ranked.len(), 10_000);
        for _ in 0..4 {
            let visible = cache.get_or_rank(&releases, 1, &query, None);
            assert_eq!(visible.len(), 10_000);
            assert!(Arc::ptr_eq(&ranked, &visible));
        }
    }

    #[gpui::test]
    fn release_index_cache_invalidates_on_refresh(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        view.update(cx, |view, cx| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("cache")])));
            view.set_filter_query("CACHE", cx);
            let cached = view.visible_release_indices();
            assert_eq!(cached.as_slice(), &[0]);

            let generation = view.releases_generation;
            view.refresh(cx);
            assert_ne!(view.releases_generation, generation);
            assert!(matches!(&view.releases, LoadState::Failed(_)));
            let refreshed = view.visible_release_indices();
            assert!(refreshed.is_empty());
            assert!(!Arc::ptr_eq(&cached, &refreshed));
        });
    }

    #[gpui::test]
    fn filtering_out_selection_clears_detail_and_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let mut redis = test_release("cache");
        redis.chart = "redis-7.2.0".to_owned();
        let api = test_release("gateway");
        let mut worker = test_release("worker");
        worker.chart = "redis-7.2.0".to_owned();
        view.update(cx, |view, _| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![redis, api, worker])));
            view.selected = Some(0);
            view.detail = Some(DetailState {
                status: LoadState::Loading,
                history: LoadState::Loading,
                values: LoadState::Loading,
            });
            view.action_error = Some(HelmFailure::timed_out());
            view.failed_action = Some(HelmAction::Uninstall {
                name: "cache".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            });
        });

        let filter_focus =
            view.read_with(cx, |view, cx| view.filter_input.read(cx).focus_handle(cx));
        assert!(filter_focus.tab_stop);
        cx.update(|window, cx| window.focus(&filter_focus, cx));
        cx.simulate_input("REDIS");
        cx.run_until_parked();
        view.update(cx, |view, _| {
            assert_eq!(view.filter_query, "REDIS");
            assert_eq!(view.visible_release_indices().as_slice(), &[0, 2]);
            assert_eq!(view.selected, Some(0));
            let action = HelmAction::Uninstall {
                name: "cache".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            };
            assert!(view.action_matches_selection(&action));
            assert!(view.retryable_action().is_some());
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update(cx, |view, cx| view.set_filter_query("gateway", cx));
        view.update(cx, |view, _| {
            assert_eq!(view.visible_release_indices().as_slice(), &[1]);
            assert_eq!(view.selected, None);
            assert!(view.detail.is_none());
            assert!(view.action_error.is_none());
            assert!(view.failed_action.is_none());
            let action = HelmAction::Uninstall {
                name: "cache".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            };
            assert!(!view.action_matches_selection(&action));
        });
        view.update(cx, |view, cx| {
            view.set_filter_query("", cx);
            let mut releases = view.release_list().to_vec();
            releases.reverse();
            view.on_releases(
                Ok(releases),
                Some(("cache".to_owned(), "default".to_owned())),
                cx,
            );
        });
        view.read_with(cx, |view, _| assert_eq!(view.selected, Some(2)));
    }

    #[test]
    fn action_copy_names_the_target() {
        let uninstall = HelmAction::Uninstall {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            cluster: Some("kind-dev".to_owned()),
        };
        assert_eq!(uninstall.title(), "Uninstall demo?");
        assert!(uninstall.detail().contains("default"));
        assert!(uninstall.detail().contains("kind-dev"));
        assert!(uninstall.detail().contains("cannot be undone"));
        assert_eq!(uninstall.confirm_label(), "Uninstall");
        assert_eq!(uninstall.success_text(), "Uninstalled demo");

        let rollback = HelmAction::Rollback {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            revision: 3,
            cluster: Some("kind-dev".to_owned()),
        };
        assert_eq!(rollback.title(), "Roll back demo to revision 3?");
        assert!(rollback.detail().contains("kind-dev"));
        assert!(rollback.detail().contains("chart version"));
        assert!(rollback.detail().contains("revision 3"));
        assert_eq!(rollback.confirm_label(), "Roll back");
        assert!(rollback.success_text().contains("revision 3"));

        let upgrade = HelmAction::Upgrade {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            chart: "bitnami/demo".to_owned(),
            revision: "4".to_owned(),
            cluster: Some("kind-dev".to_owned()),
        };
        assert_eq!(upgrade.title(), "Upgrade demo?");
        assert!(upgrade.detail().contains("kind-dev"));
        assert!(upgrade.detail().contains("bitnami/demo"));
        assert!(upgrade.detail().contains("chart version"));
        assert!(
            upgrade
                .detail()
                .contains("reuses the current release values")
        );
        assert!(upgrade.detail().contains("revision 4"));
        assert_eq!(upgrade.confirm_label(), "Upgrade");
        assert_eq!(upgrade.success_text(), "Upgraded demo");
    }

    #[test]
    fn release_actions_put_upgrade_before_rollbacks_and_skip_current() {
        let mut release = test_release("demo");
        release.revision = "3".to_owned();
        let history = vec![test_revision(1), test_revision(2), test_revision(3)];
        assert_eq!(
            release_menu_targets(Some(&release), Some(&history)),
            vec![
                ReleaseMenuTarget::Upgrade,
                ReleaseMenuTarget::Rollback { revision: 2 },
                ReleaseMenuTarget::Rollback { revision: 1 },
                ReleaseMenuTarget::Uninstall,
            ]
        );
        let mut pending = release.clone();
        pending.status = ReleaseStatus::PendingUpgrade;
        assert!(
            !release_menu_targets(Some(&pending), Some(&history))
                .contains(&ReleaseMenuTarget::Upgrade)
        );
        assert!(release_menu_targets(None, Some(&history)).is_empty());
    }

    #[gpui::test]
    fn upgrade_selection_requires_the_current_revision(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        view.update(cx, |view, _| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("demo")])));
            view.selected = Some(0);
        });
        let action = HelmAction::Upgrade {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            chart: "demo-1.0.0".to_owned(),
            revision: "1".to_owned(),
            cluster: None,
        };
        view.read_with(cx, |view, _| {
            assert!(view.action_matches_selection(&action));
        });
        let mut stale = action.clone();
        if let HelmAction::Upgrade { revision, .. } = &mut stale {
            *revision = "0".to_owned();
        }
        view.read_with(cx, |view, _| {
            assert!(!view.action_matches_selection(&stale));
        });
    }

    #[gpui::test]
    fn upgrade_requests_through_the_existing_action_callback(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let requested: Rc<std::cell::RefCell<Option<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(None));
        let requested_handler = requested.clone();
        view.update(cx, |view, _| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("demo")])));
            view.selected = Some(0);
            view.on_action_requested(move |action, _, _| {
                *requested_handler.borrow_mut() = Some(action);
            });
        });
        let action = upgrade_request(
            "demo".to_owned(),
            "default".to_owned(),
            "1".to_owned(),
            None,
        );
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request(action.clone(), window, cx));
        });
        assert_eq!(requested.borrow().clone(), Some(action));
        assert!(matches!(
            requested.borrow().as_ref(),
            Some(HelmAction::Upgrade { chart, .. }) if chart.is_empty()
        ));
    }

    #[test]
    fn capability_preserves_probe_error_kinds_and_copy() {
        assert_eq!(
            HelmCapability::from_error(&HelmError::NotInstalled),
            HelmCapability::NotInstalled
        );
        assert_eq!(
            HelmCapability::from_error(&HelmError::Timeout),
            HelmCapability::Timeout
        );
        assert_eq!(
            HelmCapability::from_error(&HelmError::Exit {
                code: Some(1),
                stderr: "broken".to_owned(),
            }),
            HelmCapability::Error
        );
        assert_eq!(
            HelmCapability::NotInstalled.reason(),
            Some(HELM_NOT_INSTALLED)
        );
        assert_eq!(HelmCapability::Timeout.reason(), Some(HELM_PROBE_TIMED_OUT));
    }

    #[test]
    fn not_installed_maps_to_actionable_copy() {
        let failure = HelmFailure::from_error(&HelmError::NotInstalled);
        assert_eq!(failure.message, HELM_NOT_INSTALLED);
        assert!(failure.message.contains("PATH"));
        assert!(failure.is_not_installed());
    }

    #[test]
    fn failures_keep_stderr_out_of_the_body_copy() {
        let failure = HelmFailure::from_error(&HelmError::Exit {
            code: Some(1),
            stderr: "Error: release: not found".to_owned(),
        });
        assert_eq!(failure.message, HELM_COMMAND_FAILED);
        assert!(
            failure.message.contains("Retry"),
            "User-facing copy must include a next step: {}",
            failure.message
        );
        for internal in ["stderr", "exit code", "exit", "exit status", "{code"] {
            assert!(
                !failure.message.contains(internal),
                "User-facing copy must not contain internal field {internal:?}: {}",
                failure.message
            );
        }
        assert!(
            failure.detail.contains("release: not found"),
            "Keep the original Helm output for the tooltip and log: {}",
            failure.detail
        );
        assert!(!failure.is_not_installed());
    }

    #[test]
    fn truncated_values_get_a_full_value_tooltip() {
        let updated = "2026-09-24 06:03:00.148985148 +0800 CST";
        assert!(cell_needs_tooltip(updated, UPDATED_COLUMN_WIDTH));
        assert!(cell_needs_tooltip(
            "2026-09-24T06:03:00.148985148+08:00",
            HISTORY_TIME_WIDTH
        ));
        assert!(!cell_needs_tooltip("1", REVISION_COLUMN_WIDTH));
        assert!(!cell_needs_tooltip("", UPDATED_COLUMN_WIDTH));
        assert!(!cell_needs_tooltip(updated, 0.0));
    }

    #[test]
    fn destructive_action_is_last_and_marked() {
        assert_eq!(
            release_menu_targets(Some(&test_release("demo")), Some(&[test_revision(2)])),
            vec![
                ReleaseMenuTarget::Upgrade,
                ReleaseMenuTarget::Rollback { revision: 2 },
                ReleaseMenuTarget::Uninstall,
            ],
            "the menu keeps the destructive action last"
        );
        assert_eq!(
            DETAIL_ACTION_ORDER,
            [
                DetailAction::Upgrade,
                DetailAction::Rollback,
                DetailAction::Uninstall
            ],
            "the detail action bar keeps the destructive action last"
        );
        assert!(
            ReleaseMenuTarget::Uninstall.is_destructive(),
            "uninstall removes the release history"
        );
        assert!(!ReleaseMenuTarget::Rollback { revision: 2 }.is_destructive());
        assert!(!ReleaseMenuTarget::Upgrade.is_destructive());
    }

    #[test]
    fn upgrade_labels_ask_for_more_input() {
        assert_eq!(
            ReleaseMenuTarget::Upgrade.label(),
            "Upgrade…",
            "upgrade needs a chart reference, so the label carries an ellipsis"
        );
        assert_eq!(DetailAction::Upgrade.label(), "Upgrade…");
        assert_eq!(
            ReleaseMenuTarget::Rollback { revision: 3 }.label(),
            "Roll back to revision 3"
        );
        assert_eq!(ReleaseMenuTarget::Uninstall.label(), "Uninstall");
        assert_eq!(
            DetailAction::Rollback.label(),
            "Roll back…",
            "roll back needs a revision, so the label carries an ellipsis"
        );
        assert_eq!(DetailAction::Uninstall.label(), "Uninstall");
        assert!(
            DetailAction::Uninstall
                .tooltip()
                .contains("cannot be restored"),
            "the destructive tooltip states the consequence"
        );
    }

    #[test]
    fn columns_sort_low_high_then_back_to_the_helm_order() {
        assert_eq!(
            next_release_sort(None, ReleaseColumn::Name),
            Some(ReleaseSort {
                column: ReleaseColumn::Name,
                descending: false,
            })
        );
        let ascending = next_release_sort(None, ReleaseColumn::Updated).unwrap();
        let descending = next_release_sort(Some(ascending), ReleaseColumn::Updated).unwrap();
        assert!(descending.descending);
        assert_eq!(
            next_release_sort(Some(descending), ReleaseColumn::Updated),
            None,
            "a third click returns to the order Helm returned"
        );
        let other = next_release_sort(Some(ascending), ReleaseColumn::Chart).unwrap();
        assert_eq!(other.column, ReleaseColumn::Chart);
        assert!(!other.descending);
    }

    #[test]
    fn a_sorted_column_reports_its_direction_in_words_and_an_arrow() {
        let ascending = ReleaseSort {
            column: ReleaseColumn::Name,
            descending: false,
        };
        assert_eq!(
            header_accessibility_label(ReleaseColumn::Name, Some(ascending)),
            "Name, sorted from low to high"
        );
        assert_eq!(
            header_accessibility_label(ReleaseColumn::Chart, Some(ascending)),
            "Chart, not sorted"
        );
        assert_eq!(
            column_affordance(Some(ascending), ReleaseColumn::Name).0,
            Some(IconName::ArrowUp)
        );
        assert_eq!(
            column_affordance(
                Some(ReleaseSort {
                    column: ReleaseColumn::Name,
                    descending: true
                }),
                ReleaseColumn::Name
            )
            .0,
            Some(IconName::ArrowDown)
        );
        assert_eq!(column_affordance(None, ReleaseColumn::Name).0, None);
        assert!(
            header_accessibility_description(ReleaseColumn::Name, Some(ascending))
                .contains("sort from high to low"),
            "the header must say what the next click does"
        );
        assert!(
            table_sort_description(None).contains("order Helm returned"),
            "{}",
            table_sort_description(None)
        );
        assert!(table_sort_description(Some(ascending)).contains("low to high"));
    }

    #[test]
    fn sorting_orders_the_filtered_rows_without_losing_any() {
        let mut second = test_release("beta");
        second.namespace = "payments".to_owned();
        second.revision = "10".to_owned();
        let mut first = test_release("alpha");
        first.revision = "2".to_owned();
        let mut third = test_release("gamma");
        third.revision = "9".to_owned();
        let releases = vec![second, first, third];

        let mut indices = ranked_release_indices(&releases, "");
        assert_eq!(indices, vec![0, 1, 2], "no filter keeps the Helm order");
        sort_release_indices(
            &releases,
            &mut indices,
            ReleaseSort {
                column: ReleaseColumn::Name,
                descending: false,
            },
        );
        assert_eq!(indices, vec![1, 0, 2]);
        sort_release_indices(
            &releases,
            &mut indices,
            ReleaseSort {
                column: ReleaseColumn::Name,
                descending: true,
            },
        );
        assert_eq!(indices, vec![2, 0, 1]);
        sort_release_indices(
            &releases,
            &mut indices,
            ReleaseSort {
                column: ReleaseColumn::Revision,
                descending: true,
            },
        );
        assert_eq!(
            indices,
            vec![0, 2, 1],
            "revisions sort on their number, not their text"
        );

        let mut cache = ReleaseIndexCache::default();
        let sorted = cache.get_or_rank(
            &releases,
            1,
            "",
            Some(ReleaseSort {
                column: ReleaseColumn::Revision,
                descending: true,
            }),
        );
        assert_eq!(sorted.as_slice(), &[0, 2, 1]);
        let unsorted = cache.get_or_rank(&releases, 1, "", None);
        assert_eq!(unsorted.as_slice(), &[0, 1, 2]);
    }

    #[test]
    fn an_unparsable_revision_sorts_last_instead_of_first() {
        let mut broken = test_release("beta");
        broken.revision = "unknown".to_owned();
        let releases = vec![broken, test_release("alpha")];
        let mut indices = ranked_release_indices(&releases, "");
        sort_release_indices(
            &releases,
            &mut indices,
            ReleaseSort {
                column: ReleaseColumn::Revision,
                descending: false,
            },
        );
        assert_eq!(indices, vec![1, 0]);
    }

    #[test]
    fn every_column_has_a_label_an_index_and_a_width() {
        let widths: Vec<f32> = ReleaseColumn::ALL.iter().map(|c| c.width()).collect();
        assert_eq!(
            widths,
            vec![
                NAME_COLUMN_WIDTH,
                NAMESPACE_COLUMN_WIDTH,
                CHART_COLUMN_WIDTH,
                STATUS_COLUMN_WIDTH,
                REVISION_COLUMN_WIDTH,
                UPDATED_COLUMN_WIDTH
            ]
        );
        for (position, column) in ReleaseColumn::ALL.into_iter().enumerate() {
            assert_eq!(column.index(), position + 1);
            assert!(!column.label().is_empty());
            assert!(widths[position] > 0.0);
        }
        assert!(ReleaseColumn::Revision.numeric());
        assert!(!ReleaseColumn::Name.numeric());
        assert!(RELEASE_TABLE_CONTENT_WIDTH >= widths.iter().sum::<f32>());
    }

    #[test]
    fn an_empty_values_document_is_reported_instead_of_rendered() {
        assert!(values_have_no_content(""));
        assert!(values_have_no_content("null\n"));
        assert!(values_have_no_content("{}"));
        assert!(!values_have_no_content("replicaCount: 2"));
    }

    #[test]
    fn a_palette_request_uses_the_selected_release_and_names_the_revision() {
        let mut release = test_release("demo");
        release.revision = "3".to_owned();
        let history = vec![test_revision(1), test_revision(2), test_revision(3)];
        let data = DetailActionData {
            name: release.name.clone(),
            namespace: release.namespace.clone(),
            cluster: Some("kind-dev".to_owned()),
            revision: release.revision.clone(),
            can_upgrade: true,
            rollback_choices: rollback_choices(Some(&release), Some(&history)),
            blocked: None,
        };

        assert_eq!(
            HelmReleaseAction::Upgrade.request(Some(data.clone())),
            Some(HelmAction::Upgrade {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                chart: String::new(),
                revision: "3".to_owned(),
                cluster: Some("kind-dev".to_owned()),
            }),
            "upgrade asks the shell for the chart reference, it never guesses one"
        );
        assert_eq!(
            HelmReleaseAction::Rollback.request(Some(data.clone())),
            Some(HelmAction::Rollback {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                revision: 2,
                cluster: Some("kind-dev".to_owned()),
            }),
            "roll back takes the newest earlier revision, and the dialog names it"
        );
        assert_eq!(
            HelmReleaseAction::Uninstall.request(Some(data.clone())),
            Some(HelmAction::Uninstall {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                cluster: Some("kind-dev".to_owned()),
            })
        );

        let mut blocked = data;
        blocked.blocked = Some(
            "Helm is still working on this release. Wait for the current operation to finish.",
        );
        for action in [
            HelmReleaseAction::Upgrade,
            HelmReleaseAction::Rollback,
            HelmReleaseAction::Uninstall,
        ] {
            assert_eq!(
                action.request(Some(blocked.clone())),
                None,
                "{action:?} must not start while the release is busy"
            );
            assert_eq!(action.request(None), None);
        }
        assert_eq!(
            HelmReleaseAction::Uninstall.command_label(),
            "Uninstall Selected Release"
        );
    }

    #[gpui::test]
    fn the_palette_entry_point_opens_the_same_confirmation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let requested: Rc<std::cell::RefCell<Vec<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = requested.clone();
        view.update(cx, |view, _| {
            view.available = true;
            let mut release = test_release("demo");
            release.revision = "2".to_owned();
            view.replace_releases(LoadState::Ready(Arc::new(vec![release])));
            view.selected = Some(0);
            view.detail = Some(DetailState {
                status: LoadState::Ready(ReleaseDetail::default()),
                history: LoadState::Ready(vec![test_revision(1), test_revision(2)]),
                values: LoadState::Ready("replicaCount: 2\n".to_owned()),
            });
            view.on_action_requested(move |action, _, _| recorded.borrow_mut().push(action));
        });

        view.read_with(cx, |view, _| {
            assert!(view.can_start(HelmReleaseAction::Uninstall));
            assert!(view.can_start(HelmReleaseAction::Rollback));
            assert!(view.blocked_reason(HelmReleaseAction::Uninstall).is_none());
        });
        for action in [HelmReleaseAction::Rollback, HelmReleaseAction::Uninstall] {
            let started = cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.request_release_action(action, window, cx)
                })
            });
            assert!(started, "{action:?} must start from outside the panel");
        }
        assert_eq!(
            *requested.borrow(),
            vec![
                HelmAction::Rollback {
                    name: "demo".to_owned(),
                    namespace: "default".to_owned(),
                    revision: 1,
                    cluster: None,
                },
                HelmAction::Uninstall {
                    name: "demo".to_owned(),
                    namespace: "default".to_owned(),
                    cluster: None,
                },
            ]
        );

        // Without a selection every entry point says so instead of guessing.
        view.update(cx, |view, _| view.clear_selection());
        assert!(!cx.update(
            |window, cx| view.update(cx, |view, cx| view.request_release_action(
                HelmReleaseAction::Uninstall,
                window,
                cx
            ))
        ));
        view.read_with(cx, |view, _| {
            for action in [
                HelmReleaseAction::Upgrade,
                HelmReleaseAction::Rollback,
                HelmReleaseAction::Uninstall,
            ] {
                assert!(!view.can_start(action), "{action:?} needs a release");
                assert!(
                    view.blocked_reason(action).is_some(),
                    "{action:?} must explain itself"
                );
            }
        });
        assert_eq!(requested.borrow().len(), 2);
    }

    #[gpui::test]
    fn the_busy_label_counts_up_and_says_nothing_before_a_second(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        assert_eq!(
            view.read_with(cx, |view, _| view.busy_label()),
            "Action in progress",
            "no running action has no label of its own"
        );
        view.update(cx, |view, _| {
            view.pending_action = Some(PendingAction {
                generation: 1,
                epoch: view.epoch,
                action: HelmAction::Uninstall {
                    name: "demo".to_owned(),
                    namespace: "default".to_owned(),
                    cluster: None,
                },
            });
            view.started_at = Some(Instant::now());
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.busy_label()),
            "Uninstalling demo…",
            "a fraction of a second is not worth showing"
        );
        view.update(cx, |view, _| {
            view.started_at = Some(Instant::now() - Duration::from_secs(7));
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.busy_label()),
            "Uninstalling demo… 7s"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    fn a_running_action_can_be_canceled(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        cx.update(gpui_tokio::init);
        let handle = cx.update(|cx| gpui_tokio::Tokio::handle(cx));
        let dir = std::env::temp_dir().join(format!("k8s-gpui-helm-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fake helm directory");
        let binary = dir.join("helm");
        std::fs::write(&binary, BLOCKING_HELM).expect("write fake helm");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make fake helm executable");
        let (view, cx) = cx.add_window_view(|_, cx| {
            HelmView::new(Some(Helm::with_binary(&binary)), Some(handle), cx)
        });
        let loaded = wait_for(cx, |cx| {
            view.read_with(cx, |view, _| view.release_list().len() == 2)
        });
        assert!(loaded, "releases must load from the fake helm");
        let notices: Rc<std::cell::RefCell<Vec<(String, Severity)>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = notices.clone();
        view.update(cx, |view, _| {
            view.selected = Some(0);
            view.set_notice_handler(move |message, severity, _| {
                recorded.borrow_mut().push((message, severity));
            });
        });
        view.update(cx, |view, cx| {
            view.run_action(
                HelmAction::Uninstall {
                    name: "first".to_owned(),
                    namespace: "default".to_owned(),
                    cluster: None,
                },
                cx,
            )
        });
        assert!(view.read_with(cx, |view, _| view.is_busy()));
        cx.run_until_parked();
        let cancel = cx
            .debug_bounds("helm-cancel")
            .expect("a running action offers Cancel");
        cx.simulate_click(cancel.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.is_busy(), "cancel stops the action");
            assert!(view.action_error.is_none(), "a cancel is not a failure");
            assert!(view.retryable_action().is_none());
            assert!(view.started_at.is_none());
        });
        assert_eq!(
            *notices.borrow(),
            vec![("Helm action canceled".to_owned(), Severity::Warning)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_running_action_blocks_its_own_retry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let requested: Rc<std::cell::RefCell<Vec<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = requested.clone();
        let retry = HelmAction::Uninstall {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            cluster: None,
        };
        view.update(cx, |view, cx| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("demo")])));
            view.selected = Some(0);
            view.on_action_requested(move |action, _, _| recorded.borrow_mut().push(action));
            // The first attempt failed, so a retry is on offer, and a second action is
            // already running.
            view.action_error = Some(HelmFailure::timed_out());
            view.failed_action = Some(retry.clone());
            view.pending_action = Some(PendingAction {
                generation: 1,
                epoch: view.epoch,
                action: HelmAction::Rollback {
                    name: "demo".to_owned(),
                    namespace: "default".to_owned(),
                    revision: 2,
                    cluster: None,
                },
            });
            view.action_task = Some(cx.spawn(async move |_, _| {
                std::future::pending::<()>().await;
            }));
        });

        assert!(view.read_with(cx, |view, _| view.is_busy()));
        assert!(
            view.read_with(cx, |view, _| view.retryable_action())
                .is_some(),
            "the failed action still has a retry"
        );
        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.request(retry.clone(), window, cx));
        });
        view.update(cx, |view, cx| view.run_action(retry.clone(), cx));
        assert!(
            requested.borrow().is_empty(),
            "a retry while an action runs would start a second Helm command"
        );
    }

    #[test]
    fn a_pending_release_masks_every_action_and_says_why() {
        for status in [
            ReleaseStatus::PendingInstall,
            ReleaseStatus::PendingUpgrade,
            ReleaseStatus::PendingRollback,
            ReleaseStatus::Uninstalling,
        ] {
            let mut release = test_release("demo");
            release.status = status;
            let history = vec![test_revision(1), test_revision(2)];
            let reason = release_blocked_reason(status)
                .expect("a pending release explains why it cannot act");
            assert!(
                reason.contains("Wait for the current operation"),
                "{status:?} reason: {reason}"
            );
            assert!(
                release_menu_targets(Some(&release), Some(&history)).is_empty(),
                "{status:?} offers no action"
            );
            assert!(!release_can_upgrade(&release), "{status:?} cannot upgrade");
            assert!(
                rollback_choices(Some(&release), Some(&history)).is_empty(),
                "{status:?} has no roll back target"
            );
        }

        let mut uninstalled = test_release("demo");
        uninstalled.status = ReleaseStatus::Uninstalled;
        assert_eq!(
            release_blocked_reason(ReleaseStatus::Uninstalled),
            Some("This release is uninstalled. Install the chart again to use it.")
        );
        assert!(
            release_menu_targets(Some(&uninstalled), Some(&[test_revision(1)])).is_empty(),
            "an uninstalled release has nothing to act on"
        );

        for status in [
            ReleaseStatus::Deployed,
            ReleaseStatus::Failed,
            ReleaseStatus::Superseded,
            ReleaseStatus::Unknown,
        ] {
            assert_eq!(
                release_blocked_reason(status),
                None,
                "{status:?} is settled and can act"
            );
        }
    }

    #[gpui::test]
    fn a_masked_release_rejects_every_action_entry_point(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let requested: Rc<std::cell::RefCell<Vec<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = requested.clone();
        view.update(cx, |view, _| {
            view.available = true;
            let mut release = test_release("demo");
            release.status = ReleaseStatus::Uninstalling;
            view.replace_releases(LoadState::Ready(Arc::new(vec![release])));
            view.selected = Some(0);
            // A previous attempt failed, so a retry is on offer for the same release.
            view.action_error = Some(HelmFailure::timed_out());
            view.failed_action = Some(HelmAction::Uninstall {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            });
            view.on_action_requested(move |action, _, _| recorded.borrow_mut().push(action));
        });

        for action in [
            HelmAction::Uninstall {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            },
            HelmAction::Rollback {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                revision: 2,
                cluster: None,
            },
            HelmAction::Upgrade {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                chart: "demo-1.0.0".to_owned(),
                revision: "1".to_owned(),
                cluster: None,
            },
        ] {
            assert!(
                !view.read_with(cx, |view, _| view.action_matches_selection(&action)),
                "{action:?} must not match a release Helm is already working on"
            );
            cx.update(|window, cx| {
                view.update(cx, |view, cx| view.request(action.clone(), window, cx));
            });
            view.update(cx, |view, cx| view.run_action(action, cx));
        }
        assert!(
            requested.borrow().is_empty(),
            "a masked release starts no action and offers no retry"
        );
        assert!(view.read_with(cx, |view, _| view.retryable_action().is_none()));
        assert!(!view.read_with(cx, |view, _| view.is_busy()));
    }

    #[test]
    fn rollback_choices_skip_the_current_revision_and_run_newest_first() {
        let mut release = test_release("demo");
        release.revision = "3".to_owned();
        let history = vec![test_revision(1), test_revision(2), test_revision(3)];

        assert_eq!(
            rollback_choices(Some(&release), Some(&history))
                .iter()
                .map(|choice| choice.revision)
                .collect::<Vec<_>>(),
            vec![2, 1],
            "the picker offers every earlier revision, newest first"
        );
        assert!(
            rollback_choices(Some(&release), Some(&history))
                .iter()
                .all(|choice| choice.label().contains("demo-1.0.0")),
            "the entry keeps the chart so two revisions stay apart"
        );
        assert_eq!(
            rollback_choices(Some(&release), None),
            Vec::new(),
            "an unloaded history offers no target"
        );
        assert_eq!(rollback_choices(None, Some(&history)), Vec::new());

        // A row whose revision is not in the history cannot name the running
        // revision, so the picker offers the whole history and the confirmation
        // dialog names the revision the user picked.
        let mut unmatched_row = test_release("demo");
        unmatched_row.revision = "0".to_owned();
        assert_eq!(
            rollback_choices(Some(&unmatched_row), Some(&history))
                .iter()
                .map(|choice| choice.revision)
                .collect::<Vec<_>>(),
            vec![3, 2, 1],
            "an unmatched row offers every revision, newest first"
        );
        let first_release = test_release("demo");
        assert_eq!(
            rollback_choices(Some(&first_release), Some(&history))
                .iter()
                .map(|choice| choice.revision)
                .collect::<Vec<_>>(),
            vec![3, 2],
            "revision 1 is never offered as a roll back target"
        );
    }

    #[test]
    fn rollback_picker_entries_keep_the_release_identity() {
        let data = DetailActionData {
            name: "demo".to_owned(),
            namespace: "default".to_owned(),
            cluster: Some("kind-dev".to_owned()),
            revision: "3".to_owned(),
            can_upgrade: true,
            rollback_choices: vec![RollbackChoice {
                revision: 2,
                chart: "demo-1.0.0".to_owned(),
                updated: "2026-09-24 10:00:00 +0800".to_owned(),
            }],
            blocked: None,
        };
        assert_eq!(
            rollback_request(&data, 2),
            HelmAction::Rollback {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                revision: 2,
                cluster: Some("kind-dev".to_owned()),
            },
            "the entry requests the revision the user picked"
        );
        assert_eq!(
            data.rollback_choices[0].updated, "2026-09-24 10:00:00 +0800",
            "the entry keeps the deployment date"
        );
    }

    #[gpui::test]
    fn rollback_button_asks_for_a_revision_before_it_runs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        cx.run_until_parked();
        let requested: Rc<std::cell::RefCell<Vec<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = requested.clone();
        view.update(cx, |view, cx| {
            view.available = true;
            let mut release = test_release("demo");
            release.revision = "3".to_owned();
            view.replace_releases(LoadState::Ready(Arc::new(vec![release])));
            view.selected = Some(0);
            view.detail = Some(DetailState {
                status: LoadState::Ready(ReleaseDetail::default()),
                history: LoadState::Ready(vec![
                    test_revision(1),
                    test_revision(2),
                    test_revision(3),
                ]),
                values: LoadState::Ready(String::new()),
            });
            view.on_action_requested(move |action, _, _| recorded.borrow_mut().push(action));
            cx.notify();
        });
        cx.run_until_parked();

        let trigger = cx
            .debug_bounds("helm-detail-rollback")
            .expect("the roll back picker");
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            requested.borrow().is_empty(),
            "the button opens a picker instead of rolling back on a guess"
        );
        let entry = cx
            .debug_bounds("MENU_ITEM-Revision 2 · demo-1.0.0")
            .expect("the picker offers revision 2");
        cx.simulate_click(entry.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            *requested.borrow(),
            vec![HelmAction::Rollback {
                name: "demo".to_owned(),
                namespace: "default".to_owned(),
                revision: 2,
                cluster: None,
            }],
            "the picked revision is the one that gets confirmed"
        );
    }

    #[gpui::test]
    fn a_pending_release_cannot_open_the_rollback_picker(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        cx.run_until_parked();
        let requested: Rc<std::cell::RefCell<Vec<HelmAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = requested.clone();
        view.update(cx, |view, cx| {
            view.available = true;
            let mut release = test_release("demo");
            release.revision = "3".to_owned();
            release.status = ReleaseStatus::PendingUpgrade;
            view.replace_releases(LoadState::Ready(Arc::new(vec![release])));
            view.selected = Some(0);
            view.detail = Some(DetailState {
                status: LoadState::Ready(ReleaseDetail::default()),
                history: LoadState::Ready(vec![
                    test_revision(1),
                    test_revision(2),
                    test_revision(3),
                ]),
                values: LoadState::Ready(String::new()),
            });
            view.on_action_requested(move |action, _, _| recorded.borrow_mut().push(action));
            cx.notify();
        });
        cx.run_until_parked();

        for selector in [
            "helm-detail-upgrade",
            "helm-detail-rollback",
            "helm-detail-uninstall",
        ] {
            let button = cx.debug_bounds(selector).expect("detail action");
            cx.simulate_click(button.center(), gpui::Modifiers::none());
        }
        cx.run_until_parked();
        assert!(
            requested.borrow().is_empty(),
            "a pending release starts no action"
        );
        assert!(
            cx.debug_bounds("MENU_ITEM-Revision 2 · demo-1.0.0")
                .is_none(),
            "a masked release shows no roll back picker"
        );
    }

    #[gpui::test]
    fn list_and_detail_slots_do_not_replace_the_running_action(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        cx.update(gpui_tokio::init);
        let handle = cx.update(|cx| gpui_tokio::Tokio::handle(cx));
        let helm = Helm::with_binary("/nonexistent/k8s-gpui/helm");
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(Some(helm), Some(handle), cx));
        let initialized = wait_for(cx, |cx| {
            view.read_with(cx, |view, _| !matches!(view.releases, LoadState::Loading))
        });
        assert!(initialized, "the initial list request must settle");
        view.update(cx, |view, cx| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![
                test_release("first"),
                test_release("second"),
            ])));
            view.selected = Some(0);
            view.action_error = Some(HelmFailure::timed_out());
            view.failed_action = Some(HelmAction::Uninstall {
                name: "first".to_owned(),
                namespace: "default".to_owned(),
                cluster: None,
            });
            view.pending_action = Some(PendingAction {
                generation: 1,
                epoch: view.epoch,
                action: HelmAction::Uninstall {
                    name: "first".to_owned(),
                    namespace: "default".to_owned(),
                    cluster: None,
                },
            });
            // A task that never resolves keeps the action slot occupied.
            view.action_task = Some(cx.spawn(async move |_, _| {
                std::future::pending::<()>().await;
            }));
        });
        assert!(view.read_with(cx, |view, _| view.is_busy()));

        view.update(cx, |view, cx| view.load_detail(cx));
        view.read_with(cx, |view, _| {
            assert!(
                view.detail_task.is_some(),
                "the detail slot starts its own task"
            );
            assert!(
                view.action_task.is_some(),
                "loading a detail must not drop the action slot"
            );
            assert!(view.is_busy(), "the action stays pending");
        });

        view.update(cx, |view, cx| view.select(1, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, Some(1), "the selection still moves");
            assert!(
                view.action_task.is_some(),
                "selecting another release must not drop the action slot"
            );
            assert!(view.is_busy(), "the action stays pending");
        });

        view.update(cx, |view, cx| view.refresh(cx));
        view.read_with(cx, |view, _| {
            assert!(
                view.action_task.is_some(),
                "reloading releases must not drop the action slot"
            );
            assert!(view.is_busy(), "the action stays pending");
            assert!(
                view.retryable_action().is_none(),
                "a retry is only offered when the action still matches the selection"
            );
        });
    }

    #[gpui::test]
    fn uninstall_and_rollback_have_no_single_key_shortcuts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| HelmView::new(None, None, cx));
        let requested = std::rc::Rc::new(std::cell::Cell::new(0usize));
        view.update(cx, |view, _| {
            view.available = true;
            view.replace_releases(LoadState::Ready(Arc::new(vec![test_release("first")])));
            let counter = std::rc::Rc::clone(&requested);
            view.on_action_requested(move |_, _, _| counter.set(counter.get() + 1));
        });
        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        for key in ["u", "U", "r", "R"] {
            cx.simulate_keystrokes(key);
        }
        assert_eq!(
            requested.get(),
            0,
            "U/R must not trigger a destructive action"
        );
        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(0));
        assert_eq!(requested.get(), 0);
    }

    #[cfg(unix)]
    const BLOCKING_HELM: &str = r#"#!/bin/sh
dir=$(dirname "$0")
cmd=""
for arg in "$@"; do
  case "$arg" in
    list|status|history|uninstall|rollback|upgrade|get) cmd="$arg"; break ;;
  esac
done
case "$cmd" in
  list)
    printf '%s' '[{"name":"first","namespace":"default","revision":"1","updated":"2026-09-24 10:00:00 +0800","status":"deployed","chart":"demo-1.0.0","app_version":"1.0.0"},{"name":"second","namespace":"default","revision":"1","updated":"2026-09-24 10:00:00 +0800","status":"deployed","chart":"demo-1.0.0","app_version":"1.0.0"}]'
    ;;
  status) printf '{"name":"%s","namespace":"default","version":1,"info":{"status":"deployed"}}' "$2" ;;
  history) printf '%s' '[]' ;;
  get) printf 'replicaCount: 2\n' ;;
  uninstall|rollback|upgrade)
    while [ ! -f "$dir/go" ]; do sleep 0.02; done
    if [ "$cmd" = upgrade ]; then printf '%s' '{}'; fi
    ;;
  *)
    printf 'unknown helm command: %s\n' "$cmd" >&2
    exit 2
    ;;
esac
"#;

    fn wait_for(
        cx: &mut gpui::VisualTestContext,
        mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) -> bool {
        for _ in 0..400 {
            if done(cx) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
            cx.run_until_parked();
        }
        done(cx)
    }

    #[cfg(unix)]
    #[gpui::test]
    fn pending_action_survives_selection_change_and_finishes_once(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        cx.update(|cx| {
            ::settings::init(cx);
            theme_settings::init(LoadThemes::JustBase, cx);
        });
        cx.dispatcher.allow_parking();
        cx.update(gpui_tokio::init);
        let handle = cx.update(|cx| gpui_tokio::Tokio::handle(cx));
        let dir = std::env::temp_dir().join(format!("k8s-gpui-helm-action-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fake helm directory");
        let binary = dir.join("helm");
        std::fs::write(&binary, BLOCKING_HELM).expect("write fake helm");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make fake helm executable");

        let (view, cx) = cx.add_window_view(|_, cx| {
            HelmView::new(Some(Helm::with_binary(&binary)), Some(handle), cx)
        });
        let loaded = wait_for(cx, |cx| {
            view.read_with(cx, |view, _| view.release_list().len() == 2)
        });
        assert!(loaded, "releases must load from the fake helm");
        let notices: Rc<std::cell::RefCell<Vec<(String, Severity)>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = notices.clone();
        view.update(cx, |view, _| {
            view.selected = Some(0);
            view.set_notice_handler(move |message, severity, _| {
                recorded.borrow_mut().push((message, severity));
            });
        });

        let action = HelmAction::Uninstall {
            name: "first".to_owned(),
            namespace: "default".to_owned(),
            cluster: None,
        };
        view.update(cx, |view, cx| view.run_action(action, cx));
        assert!(view.read_with(cx, |view, _| view.is_busy()));
        assert_eq!(
            view.read_with(cx, |view, _| view.busy_label()),
            "Uninstalling first…"
        );

        view.update(cx, |view, cx| view.select(1, cx));
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, Some(1));
            assert!(
                view.is_busy(),
                "selecting another release must keep the action pending"
            );
            assert!(view.action_task.is_some());
            assert!(view.detail_task.is_some());
        });

        let pending = view
            .read_with(cx, |view, _| view.pending_action.clone())
            .expect("a pending action");
        let mut stale = pending.clone();
        stale.generation = pending.generation.wrapping_add(1);
        view.update(cx, |view, cx| {
            view.on_action_finished(stale, Err(HelmFailure::timed_out()), cx)
        });
        view.read_with(cx, |view, _| {
            assert!(view.is_busy());
            assert!(view.action_error.is_none());
            assert!(view.failed_action.is_none());
        });
        assert!(notices.borrow().is_empty());

        std::fs::write(dir.join("go"), b"go").expect("release the pending action");
        let finished = wait_for(cx, |cx| view.read_with(cx, |view, _| !view.is_busy()));
        assert!(finished, "the action result must not be lost");
        view.read_with(cx, |view, _| {
            assert!(view.pending_action.is_none());
            assert!(view.action_error.is_none());
            assert!(view.failed_action.is_none());
        });
        assert_eq!(
            *notices.borrow(),
            vec![("Uninstalled first".to_owned(), Severity::Success)]
        );

        let refreshed = wait_for(cx, |cx| {
            view.read_with(cx, |view, _| {
                view.release_list().len() == 2 && !matches!(view.releases, LoadState::Loading)
            })
        });
        assert!(refreshed, "the action must reload releases");
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, Some(1));
            assert!(!view.is_busy());
        });

        view.update(cx, |view, cx| view.on_action_finished(pending, Ok(()), cx));
        assert_eq!(
            *notices.borrow(),
            vec![("Uninstalled first".to_owned(), Severity::Success)],
            "a replayed result must not apply twice"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
