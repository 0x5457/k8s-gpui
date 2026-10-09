//! Port Forward sessions.
//!
//! A forward is a **session with a lifecycle** — established, active, failed,
//! stopped — the same class of thing as a terminal, so it gets its own centre
//! view instead of a section inside the notification centre (`UI-SPEC.md` §14.1,
//! `UI-REDESIGN.md` D28). Three things follow from that, and they are the whole of
//! this file: creation is a right-click on a Service or a Pod rather than a form
//! in a panel, the list is divided by what a session is *doing* rather than by
//! which namespace it lives in, and a broken session says which step broke and
//! reconnects on its own — visibly.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::{FluentBuilder as _, InteractiveElement, StatefulInteractiveElement};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable, Hsla,
    IntoElement, MouseButton, ParentElement, Pixels, Render, Role, SharedString, Styled,
    Subscription, Task, UnderlineStyle, Window, div, px,
};
use kube_core::DynamicObject;

use super::common;
use super::dock::{DockPanel, ForwardId, ForwardPhase, ForwardSnapshot, ForwardSummary};
use crate::design::{self, Severity, role, space};
use crate::settings;
use crate::table_view::TextInput;

/// The shell's hook for the title bar's `+ New`.
///
/// `UI-SPEC.md` §14.2 says 90% of the path is a row's context menu, so this is
/// the *other* tenth: someone who wants a forward and has no row to right-click.
pub type NewForwardCallback = Rc<dyn Fn(&mut Window, &mut App)>;

const TITLE: &str = "Port forwards";
const SUMMARY_LABEL: &str = "Port forward summary";
const EMPTY_TITLE: &str = "No port forwards";
const EMPTY_HINT: &str = "Right-click a Service or a Pod and choose Forward a port to start one.";
const FILTERED_EMPTY_TITLE: &str = "No matching port forwards";
/// The one sentence `UI-SPEC.md` §4.13 *requires* under this title: how many filters
/// are running. It used to be "No port forward matches the filter. Clear it to see
/// every forward." — the first clause restating the title in five more words, the
/// second repeating a `Clear filter` button that sits 32px below it, and neither
/// clause saying the one fact that separates this state from an empty cluster.
///
/// The count, the button and the funnel are now the same three things this state
/// wears in the sidebar, in the table and in the Helm panel beside it, so a reader
/// who has learned one "被筛掉了" has learned all four.
const FILTERED_EMPTY_HINT: &str = "1 filter is active.";
/// The state where there is no cluster to forward from.
///
/// A state and not an error report: the panel is not broken, there is simply nothing for it to
/// list, and the sentence names the missing thing and the way back to it in one line. It carries
/// no control, because the way back is the context selector in the title bar and a button here
/// could only restate it.
const DISCONNECTED_TITLE: &str = "No cluster connected";
const DISCONNECTED_HINT: &str =
    "Port forwards run against a cluster. Connect one from the context selector to start one.";
/// What the list tells a reader about its keys.
///
/// The chords that belong to the panel's whole purpose — open the address, copy it — are
/// named on the address itself, so a permanent legend under the list only repeated the
/// arrow keys every list in every application already has. The rest stays here, where a
/// screen reader reaches it and a sighted reader does not have to read past.
const LIST_DESCRIPTION: &str = "Use Up and Down to move between port forwards and Home and End for the first or last one. Enter runs the row action, Control C copies the address, Control O opens it in the browser, and slash focuses the filter.";
const FILTER_WIDTH: f32 = 208.;

/// How long a copy or open confirmation stays in the toolbar.
const FEEDBACK_DURATION: Duration = Duration::from_millis(1500);
/// The list repaints once a second so an elapsed time and a reconnect countdown
/// are numbers a reader can watch instead of numbers frozen at the moment the
/// forward started. A repaint only happens when one of those strings changes.
const TICK: Duration = Duration::from_secs(1);
/// How long the panel waits before it tries a broken forward again, and how
/// many times it tries.
///
/// `UI-SPEC.md` §14.5 wants auto-reconnect on by default *and* visible while it
/// happens, because a silent retry is a lie. Three tries with a widening gap is
/// enough to ride out a Pod restart; a session that survives none of them is
/// broken in a way only the user can fix, so the panel stops and says why.
const RECONNECT_DELAYS: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(8),
    Duration::from_secs(20),
];

// ---------------------------------------------------------------------------
// Phase vocabulary
// ---------------------------------------------------------------------------

/// The three groups the list is divided into, in the order a reader wants them:
/// what is working, what is broken, what is over.
///
/// `UI-SPEC.md` §14.3 draws exactly these three headings. The old list grouped by
/// namespace, which answered "which namespace?" — a question nobody asks about a
/// forward, and one the row's own target column already answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Active,
    Failed,
    Stopped,
}

impl Section {
    const ALL: [Self; 3] = [Self::Active, Self::Failed, Self::Stopped];

    /// Which group a phase belongs to. `Stopping` sits with the stopped ones
    /// because it is on its way out, and `Starting` sits with the active ones
    /// because the user asked for it and it is about to answer.
    fn of(phase: ForwardPhase) -> Self {
        match phase {
            ForwardPhase::Running | ForwardPhase::Starting => Self::Active,
            ForwardPhase::Failed => Self::Failed,
            ForwardPhase::Stopping | ForwardPhase::Stopped => Self::Stopped,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
        }
    }
}

/// The channel a phase's mark wears.
///
/// The list used to resolve the dot straight to an ink, so every other reader of a phase's state
/// had to resolve its own: `shell/status_bar.rs` keeps a second copy of the same mapping for the
/// chip in the bar, and nothing checked that the two agreed. It resolves to a [`Severity`] now and
/// the two inks come from the role layer's one mapping — `role::status_for` for the mark,
/// `role::status_word_for` for the word beside it — which is what makes "a 6px mark and a 12px
/// label do not read at the same contrast" one decision instead of two.
///
/// `UI-SPEC.md` §14.3 still turns the vocabulary over for this list: an active forward is the
/// normal case, and `role::status_for(Success)` is secondary ink rather than green, so a working
/// forward wears no status channel at all and only a failure is coloured. A forward that is still
/// starting is graded by age, the same rule the tables use for a Pending pod — under half a minute
/// is normal, over half a minute is a warning, over five minutes is a problem.
fn phase_severity(phase: ForwardPhase, age: Duration) -> Severity {
    match phase {
        ForwardPhase::Running | ForwardPhase::Stopping => Severity::Success,
        ForwardPhase::Stopped => Severity::Muted,
        ForwardPhase::Starting => {
            if age > Duration::from_secs(300) {
                Severity::Error
            } else if age > Duration::from_secs(30) {
                Severity::Warning
            } else {
                Severity::Success
            }
        }
        ForwardPhase::Failed => Severity::Error,
    }
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
///
/// The list itself draws a dot rather than these shapes, but the status bar's chip
/// still reads this, so the vocabulary stays in one place.
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

/// The channel and the word one row's phase wears.
///
/// The word is [`phase_presentation`]'s, so the list, the status bar's chip and the phase an
/// entry point outside the panel reports all spell a phase the same way, and the channel is
/// [`phase_severity`]'s, so the mark and the word are two readings of one severity rather than two
/// independent decisions.
fn phase_status(phase: ForwardPhase, age: Duration) -> (Severity, &'static str) {
    let (_, word, _, _) = phase_presentation(phase);
    (phase_severity(phase, age), word)
}

/// The 12/16 role, for the quieter columns of a row.
///
/// The address and the name are body copy; the remote end, the target and the arrow
/// beside them are one step quieter, which is what makes the address the thing the
/// eye lands on.
fn label_quiet(text: impl Into<SharedString>) -> Label {
    Label::new(text)
        .text_size(design::text::LABEL)
        .line_height(design::text::LABEL_LINE_HEIGHT)
}

/// The status channel a severity names, taken from the role layer.
///
/// One mapping for the whole product: `role::status_for` keeps `Success`
/// quiet, so a routine copy confirmation is secondary ink, not the green the
/// product reserves for something that needs the reader.
fn severity_role(severity: Severity, cx: &App) -> Hsla {
    role::status_for(severity, cx)
}

fn summary_text(summary: ForwardSummary) -> String {
    format!(
        "{} active · {} failed",
        design::format::count(summary.active),
        design::format::count(summary.failed)
    )
}

fn id_key(id: ForwardId) -> SharedString {
    SharedString::from(format!("{id:?}"))
}

// ---------------------------------------------------------------------------
// The session model
// ---------------------------------------------------------------------------

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

    /// One name for the control a phase shows. The element id and the debug
    /// selector both use it, so a test can address the control a row renders
    /// right now.
    fn name(self) -> &'static str {
        match self {
            Self::Start(_) => "forwards-start",
            Self::Stop(_) => "forwards-stop",
            Self::Retry(_) => "forwards-retry",
            Self::Waiting(_) => "forwards-waiting",
        }
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

/// `localhost:8080`, the address as a row *reads* it.
///
/// `UI-SPEC.md` §14.3 and `mockup/secondary.html` both print the bare host and port, and the
/// scheme is four characters this column cannot spare: the fixed address cell is 104px, and
/// `http://localhost:8080` is already at the edge of it before a five-digit port arrives. The
/// scheme belongs to the href, the clipboard and the spoken name, all of which keep it — a
/// browser types it for you, and a reader who needs the whole thing has three ways to get it.
fn forward_address(snapshot: &ForwardSnapshot) -> Option<String> {
    live_local_port(snapshot).map(|port| format!("localhost:{port}"))
}

/// The width of the address cell, sized to the longest address the column can ever print.
///
/// `localhost:` is ten characters and a port is at most five, so fifteen is the ceiling, and the
/// column is given that plus the tail the ellipsis needs. A fixed width is the point — every
/// row's arrow lands on the same x — so it is derived from the text rather than guessed at.
const ADDRESS_WIDTH: f32 = 104.;

/// The width of the status lane: the 6px mark, `design::space::SM` of gap, and the widest
/// word [`phase_presentation`] can print.
///
/// `Stopping…` is the longest at `caption`, about 53px, so the lane needs `6 + 8 + 53 = 67` and
/// is 76 so the word has room to grow without the lane and the words disagreeing again — the
/// same reason the address cell is a fixed width rather than a measured one. It is a lane, not a
/// column: the words are what the row is read for and the lane is only the room they share.
const STATUS_LANE_WIDTH: f32 = 76.;

/// The width of the name lane, in the same spirit as [`ADDRESS_WIDTH`]: a fixed
/// lane is what puts every row's address, arrow and remote port on one spine,
/// so the port columns do not drift with the length of the name in front of
/// them. Long names truncate inside the lane.
const NAME_LANE_WIDTH: f32 = 160.;

/// The gap between a status mark and the word beside it.
///
/// `design::space::SM`, not `design::size::STATUS_DOT`. The mark and the word are two readings of
/// one state, not one ornament split in two, so they are a group inside the lane and get the
/// scale's "closely related" step. `STATUS_DOT` is the mark's own size and using it as a gap is
/// how two things that mean the same thing end up welded together — and the lane is measured
/// from the mark and the gap, so the number has to be one the scale owns rather than one borrowed
/// from a shape.
const STATUS_MARK_GAP: Pixels = space::SM;

/// The width of the remote-port lane: `65535` at `label` is about 38px, `/tcp` is another 22, and
/// the lane is 68 so the longest remote end in the RFC's range draws whole.
///
/// Fixed because the lane is right-aligned: an auto-width lane puts its right edge wherever the
/// widest row happens to end, and a column of numbers whose right edge moves is not a column.
const REMOTE_LANE_WIDTH: f32 = 68.;

/// Namespace a forward targets. `default` is the namespace Kubernetes uses when a request leaves
/// it empty, so a target cell never reads as a blank cell.
fn forward_namespace(snapshot: &ForwardSnapshot) -> &str {
    snapshot
        .request
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or("default")
}

/// `pod:8080`, the Pod and remote port this forward targets.
fn target_text(snapshot: &ForwardSnapshot) -> String {
    format!("{}:{}", snapshot.label, snapshot.remote_port)
}

/// The short name of the thing being forwarded, as a row shows it.
///
/// The snapshot's own `label` is a deep link (`context/namespace/Pod/name`), which is
/// the right thing to copy and the wrong thing to read in a 32px row.
fn row_name(snapshot: &ForwardSnapshot) -> SharedString {
    snapshot.request.name.clone()
}

/// `namespace/name`, the target as a row shows it.
fn row_target(snapshot: &ForwardSnapshot) -> String {
    format!("{}/{}", forward_namespace(snapshot), row_name(snapshot))
}

/// `8080/tcp`, the remote end of the forward with the protocol a stream needs spelled out.
fn remote_text(snapshot: &ForwardSnapshot) -> String {
    format!("{}/tcp", snapshot.remote_port)
}

/// The reason a failure gives, or nothing when the runtime reported none.
fn failure_reason(snapshot: &ForwardSnapshot) -> Option<SharedString> {
    snapshot
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(SharedString::from)
}

/// Which step of a forward broke, in the words `UI-SPEC.md` §14.3 asks for.
///
/// The runtime's own sentence stays the text of record. What this adds is the step,
/// because "error upgrading connection" names a stream and not a step, and a reader
/// who is told *where* it broke does not have to go and find out.
fn failure_step(snapshot: &ForwardSnapshot) -> Option<&'static str> {
    let reason = failure_reason(snapshot)?;
    let reason = reason.to_lowercase();
    Some(
        if reason.contains("not found") || reason.contains("no such pod") {
            "The target no longer exists"
        } else if reason.contains("refused") {
            "Nothing is listening on the target port"
        } else if reason.contains("timeout") || reason.contains("timed out") {
            "The API server did not answer"
        } else if reason.contains("forbidden") || reason.contains("unauthorized") {
            "The connection is not allowed"
        } else if reason.contains("already in use") || reason.contains("not available") {
            // A local port the reader asked for and did not get, which is the one collision
            // that happens on this machine rather than in the cluster. The forward refuses it
            // rather than moving, so the row has to name the step that stopped it.
            "The local port was already taken"
        } else if reason.contains("connect") || reason.contains("stream") {
            "The stream to the Pod broke"
        } else {
            "The forward could not be established"
        },
    )
}

/// Spoken text for one row. It carries the address and the failure reason, which a tooltip
/// alone does not reach, and `retry_stopped` so a reader who cannot see the row's second line
/// hears that no further attempt is coming either.
fn row_aria_label(snapshot: &ForwardSnapshot, retry_stopped: bool) -> String {
    let mut label = format!(
        "Port forward for {}. {}.",
        target_text(snapshot),
        phase_presentation(snapshot.phase).1
    );
    if let Some(url) = forward_url(snapshot) {
        label.push_str(&format!(" {url}."));
    }
    if retry_stopped {
        label.push_str(" Automatic retry has stopped; select Retry to try again.");
    }
    if let Some(error) = failure_reason(snapshot) {
        label.push(' ');
        label.push_str(&error);
    }
    label
}

/// How long a session has been in its current phase, in the words a row shows.
///
/// `UI-SPEC.md` §14.3 wants the running time rather than the word "active", and a
/// failure wants "3 min ago" rather than a second field. One formatter serves both,
/// so a reader learns one scale.
fn elapsed_text(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

/// When a forward last changed phase, and why it last failed.
///
/// The dock owns the phase; the *age* of a phase is a reading, and this is where the
/// panel keeps it. A forward that was already running when this view opened counts
/// from the moment it was first seen, so its running time is honest about being a
/// lower bound rather than invented.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PhaseRecord {
    phase: ForwardPhase,
    since: Instant,
    reason: SharedString,
}

/// The age of a phase, or the zero age for a forward this panel has only just seen.
fn phase_age(
    phase_since: &HashMap<ForwardId, PhaseRecord>,
    id: ForwardId,
    now: Instant,
) -> Duration {
    phase_since.get(&id).map_or(Duration::ZERO, |record| {
        now.saturating_duration_since(record.since)
    })
}

/// A reconnect this panel is waiting out.
#[derive(Clone, Debug)]
struct Reconnect {
    id: ForwardId,
    /// Which failure this attempt belongs to, so a *new* failure gets a fresh
    /// budget instead of inheriting the last one's spent one.
    reason: SharedString,
    due: Instant,
}

/// The rows the list draws: the forwards that match the filter, in section order.
fn build_sections(forwards: &[ForwardSnapshot]) -> Vec<(Section, Vec<ForwardSnapshot>)> {
    let mut sections: Vec<(Section, Vec<ForwardSnapshot>)> = Vec::with_capacity(Section::ALL.len());
    for section in Section::ALL {
        let members: Vec<ForwardSnapshot> = forwards
            .iter()
            .filter(|snapshot| Section::of(snapshot.phase) == section)
            .cloned()
            .collect();
        if !members.is_empty() {
            sections.push((section, members));
        }
    }
    sections
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
        row_name(snapshot),
        snapshot.remote_port,
        forward_namespace(snapshot),
        phase_presentation(snapshot.phase).1,
        forward_url(snapshot).unwrap_or_default()
    );
    if let Some(error) = failure_reason(snapshot) {
        haystack.push(' ');
        haystack.push_str(&error);
    }
    haystack.to_lowercase().contains(&query)
}

/// `Stop all` can only end forwards that are still starting or running.
fn is_stoppable(phase: ForwardPhase) -> bool {
    matches!(phase, ForwardPhase::Starting | ForwardPhase::Running)
}

fn stoppable_count(snapshots: &[ForwardSnapshot]) -> usize {
    snapshots
        .iter()
        .filter(|snapshot| is_stoppable(snapshot.phase))
        .count()
}

// ---------------------------------------------------------------------------
// Ports: the target a forward offers, and the collision it has to refuse
// ---------------------------------------------------------------------------

/// One port a Pod or a Service offers, as a choice instead of a value to type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerPort {
    pub port: u16,
    /// The container, or the Service's own port name, so a multi-port target stays
    /// unambiguous.
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

/// The `spec.ports` of a Service, in document order and without duplicates.
///
/// A Service row's context menu prefills from here rather than from the Pod behind
/// it, because a Service is what the user right-clicked and a Service's port is the
/// port they mean. `targetPort` is the Service's own name for the container port:
/// when it is a number that is the port, and when it is a name only the Pod behind
/// the Service can resolve it — which is a second command and a second wait, so the
/// Service port itself is offered and the Pod list is the escape hatch.
pub fn service_ports(object: &DynamicObject) -> Vec<ContainerPort> {
    let Some(declared) = object
        .data
        .get("spec")
        .and_then(|spec| spec.get("ports"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut ports: Vec<ContainerPort> = Vec::new();
    for entry in declared {
        let tcp = entry
            .get("protocol")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|protocol| protocol.eq_ignore_ascii_case("TCP"));
        if !tcp {
            continue;
        }
        let port = entry
            .get("targetPort")
            .and_then(|port| match port {
                serde_json::Value::Number(number) => number.as_u64(),
                serde_json::Value::String(text) => text.parse::<u64>().ok(),
                _ => None,
            })
            .and_then(|port| u16::try_from(port).ok());
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(SharedString::from);
        let Some(port) = port else {
            continue;
        };
        if ports.iter().any(|existing| existing.port == port) {
            continue;
        }
        ports.push(ContainerPort {
            port,
            container: name,
        });
    }
    ports
}

/// The address a forward's local listener is opened on.
///
/// `127.0.0.1` is the default and the only value the field offers without a
/// deliberate act, because a forward is a way to reach something that was not
/// reachable before, and `0.0.0.0` publishes it to the whole network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BindAddress {
    #[default]
    Loopback,
    Any,
}

impl BindAddress {
    pub const ALL: [Self; 2] = [Self::Loopback, Self::Any];

    pub fn label(self) -> &'static str {
        match self {
            Self::Loopback => "127.0.0.1",
            Self::Any => "0.0.0.0",
        }
    }
    fn ip(self) -> Ipv4Addr {
        match self {
            Self::Loopback => Ipv4Addr::LOCALHOST,
            Self::Any => Ipv4Addr::UNSPECIFIED,
        }
    }
}

/// Who holds a local port, so a conflict can be named instead of left to fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortHolder {
    /// A forward this window already started.
    Forward { label: SharedString },
    /// Another program on the machine.
    Process,
}

impl PortHolder {
    /// The sentence to show under the local field, in the words of a collision
    /// the user can act on.
    pub fn sentence(&self, port: u16) -> String {
        match self {
            Self::Forward { label } => format!("{port} is already in use by {label}."),
            Self::Process => format!("{port} is already in use by another program."),
        }
    }
}

/// The local ports this window's live forwards hold, with the name of each.
///
/// [`port_collision`] takes this, so a collision with a forward this window started
/// is named as such rather than as "another program".
pub fn port_holders(snapshots: &[ForwardSnapshot]) -> Vec<(SharedString, u16)> {
    snapshots
        .iter()
        .filter_map(|snapshot| live_local_port(snapshot).map(|port| (row_name(snapshot), port)))
        .collect()
}

/// Whether a local port can be taken, and who has it if it cannot.
///
/// This is the check `UI-SPEC.md` §14.2 asks for *before* submit: the field binds
/// the address and drops the listener, so the answer is the operating system's
/// rather than a guess. It races the forward that eventually binds the port — a
/// port can be taken in the moment between the check and the submit — which is why
/// the row keeps reporting a substituted port after the forward has been started.
pub fn port_collision(
    port: u16,
    bind: BindAddress,
    ours: &[(SharedString, u16)],
) -> Option<PortHolder> {
    if let Some((label, _)) = ours.iter().find(|(_, held)| *held == port) {
        return Some(PortHolder::Forward {
            label: label.clone(),
        });
    }
    match std::net::TcpListener::bind((bind.ip(), port)) {
        Ok(listener) => {
            drop(listener);
            None
        }
        Err(_) => Some(PortHolder::Process),
    }
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

pub struct ForwardsView {
    dock: Entity<DockPanel>,
    selected: Option<ForwardId>,
    focus_handle: FocusHandle,
    new_callback: Option<NewForwardCallback>,
    filter_input: Entity<TextInput>,
    filter: String,
    feedback: Option<Feedback>,
    /// When each forward last changed phase, so a row can show how long it has been
    /// that way rather than a state word.
    phase_since: HashMap<ForwardId, PhaseRecord>,
    /// How much of the reconnect budget each current failure has spent, and the
    /// failure it was spent on.
    attempts: HashMap<ForwardId, (SharedString, usize)>,
    /// The attempts this panel is waiting out right now.
    reconnects: Vec<Reconnect>,
    /// The one repaint task, so elapsed times and reconnect countdowns move. It ends
    /// with the view, which is the only thing that can hold it.
    ticker: Option<Task<()>>,
    /// The last time strings the ticker rendered, so a second that changes nothing
    /// costs no repaint.
    stamp: String,
    _dock_observation: Subscription,
    _keys: Subscription,
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
            view.sync(cx);
            // The selection follows the rows the user can see, so a filter that hides
            // it moves the selection instead of leaving the keyboard on a row that is
            // not drawn.
            view.reconcile_selection(cx);
            cx.notify();
        });
        let filter_input = {
            let view = cx.weak_entity();
            cx.new(|cx| {
                TextInput::new("Filter forwards…", cx, move |text, cx| {
                    // **Deferred**, and this is not a style choice. `TextInput`'s
                    // `on_change` fires from inside the input entity's own update,
                    // so calling `view.update` straight from it re-enters the panel —
                    // and `entity_map` refuses that with
                    // `cannot update ForwardsView while it is already being updated`.
                    //
                    // It was reachable from two paths, and the second one is the reason
                    // this is written down: the field's own `×`, and the empty state's
                    // `Clear filter`. The first happened to work because the click
                    // arrived while the panel was idle; the second arrived from inside
                    // the panel's own render, and the app died. `helm.rs` and
                    // `search.rs` both already deferred here — this panel was the one
                    // that had not.
                    let view = view.clone();
                    let text = text.to_owned();
                    cx.defer(move |cx| {
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |view, cx| view.set_filter(text, cx));
                        }
                    });
                })
                .with_accessibility(
                    "Filter port forwards",
                    "Type text to match a target, namespace, address, or state. Press Escape to clear the filter.",
                    "Clear port forward filter",
                )
                .with_width(px(FILTER_WIDTH))
            })
        };
        // The panel's keys are claimed before any row control sees them, so the
        // chords below still mean what they mean in a row.
        let keys = cx.weak_entity();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(view) = keys.upgrade() else {
                return;
            };
            view.update(cx, |view, cx| view.on_key(&event.keystroke, window, cx));
        });
        Self {
            dock,
            selected: None,
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            new_callback,
            filter_input,
            filter: String::new(),
            feedback: None,
            phase_since: HashMap::new(),
            attempts: HashMap::new(),
            reconnects: Vec::new(),
            ticker: None,
            stamp: String::new(),
            _dock_observation: observation,
            _keys: intercept,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }
    fn snapshots(&self, cx: &App) -> Vec<ForwardSnapshot> {
        self.dock.read(cx).forward_snapshots()
    }

    /// The forwards the list shows, in the order the sections ask for.
    fn visible_forwards(&self, cx: &App) -> Vec<ForwardSnapshot> {
        self.snapshots(cx)
            .into_iter()
            .filter(|snapshot| matches_filter(snapshot, &self.filter))
            .collect()
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

    /// A selection the filter moved out of view follows the first visible forward, so
    /// the keyboard never rests on a row that is not drawn.
    fn reconcile_selection(&mut self, cx: &mut Context<Self>) {
        let visible = self.visible_forwards(cx);
        if self
            .selected
            .is_some_and(|selected| !visible.iter().any(|snapshot| snapshot.id == selected))
        {
            self.selected = visible.first().map(|snapshot| snapshot.id);
        }
    }

    /// The forward an action applies to: the selected row, or the first one when the
    /// list has focus without a selection.
    fn selected_forward(&self, cx: &App) -> Option<ForwardId> {
        let visible = self.visible_forwards(cx);
        match self.selected {
            Some(id) if visible.iter().any(|snapshot| snapshot.id == id) => Some(id),
            _ => visible.first().map(|snapshot| snapshot.id),
        }
    }

    /// Move the selection `delta` rows, stopping at the ends. A list that wraps around
    /// reads as a loop, and the reader loses the forward they were on.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let visible = self.visible_forwards(cx);
        if visible.is_empty() {
            return;
        }
        let last = visible.len() as isize - 1;
        let next = match self
            .selected
            .and_then(|id| visible.iter().position(|snapshot| snapshot.id == id))
        {
            Some(index) => (index as isize + delta).clamp(0, last),
            None if delta < 0 => last,
            None => 0,
        };
        let id = visible[next as usize].id;
        if self.selected != Some(id) {
            self.selected = Some(id);
            cx.notify();
        }
    }

    fn activate(&mut self, action: RowAction, cx: &mut Context<Self>) {
        let id = action.id();
        match action {
            RowAction::Start(_) | RowAction::Retry(_) => {
                // A manual attempt is the user saying this session matters, so it
                // starts the reconnect budget over rather than inheriting a spent one.
                self.forget_attempts(id);
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
                self.forget_attempts(id);
                self.dock.update(cx, |dock, cx| dock.stop_forward(id, cx));
            }
            RowAction::Waiting(_) => return,
        }
        self.selected = Some(id);
        cx.notify();
    }

    /// A forward is no longer the panel's problem, so the budget it spent goes with it.
    fn forget_attempts(&mut self, id: ForwardId) {
        self.attempts.remove(&id);
        self.reconnects.retain(|reconnect| reconnect.id != id);
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
        let Some(id) = self.selected_forward(cx) else {
            return;
        };
        let Some(snapshot) = self.forward(id, cx) else {
            return;
        };
        self.activate(row_action(&snapshot), cx);
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

    /// True while the keystroke carries the editing chord the list owns. Control+C and
    /// Control+O are free here: the log list uses the same copy chord, and the filter
    /// input keeps its own copy binding while it holds the keyboard.
    fn url_chord(keystroke: &gpui_kit::Keystroke) -> Option<UrlChord> {
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

    /// The keys the list owns: the editing chord, the shortcut into the filter, and the
    /// arrows and Home/End that walk the rows.
    ///
    /// This runs as a keystroke interceptor rather than as an `on_key_down` on the panel
    /// because an action stops the dispatch before the element listeners are reached, so
    /// a listener would never see the keys a control in a row binds.
    fn on_key(
        &mut self,
        keystroke: &gpui_kit::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The keys are the window's, so they are claimed only while the keyboard is in
        // this list, and never out of the filter: it holds its own text and its own copy
        // binding.
        if !self.focus_handle.contains_focused(window, cx)
            || self
                .filter_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
            || keystroke.modifiers.alt
            || keystroke.modifiers.platform
        {
            return;
        }
        if let Some(chord) = Self::url_chord(keystroke) {
            let Some(id) = self.selected_forward(cx) else {
                return;
            };
            match chord {
                UrlChord::Copy => self.copy_url(id, cx),
                UrlChord::Open => self.open_url(id, cx),
            }
            cx.stop_propagation();
            return;
        }
        if keystroke.modifiers.control {
            return;
        }
        let handled = match keystroke.key.as_str() {
            "down" => {
                self.move_selection(1, cx);
                true
            }
            "up" => {
                self.move_selection(-1, cx);
                true
            }
            "home" => {
                self.move_selection(i32::MIN as isize, cx);
                true
            }
            "end" => {
                self.move_selection(i32::MAX as isize, cx);
                true
            }
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

    // ── Reconnect ───────────────────────────────────────────────────────────

    /// Note when every forward last changed phase, and queue an attempt for the ones
    /// that just failed.
    ///
    /// This runs on every dock change and on the first frame, which is what makes a
    /// session that failed before this view opened still count its age from roughly
    /// the moment it broke.
    fn sync(&mut self, cx: &mut Context<Self>) -> Vec<ForwardSnapshot> {
        let snapshots = self.snapshots(cx);
        let now = Instant::now();
        let live: Vec<ForwardId> = snapshots.iter().map(|snapshot| snapshot.id).collect();
        self.phase_since.retain(|id, _| live.contains(id));
        self.attempts.retain(|id, _| live.contains(id));
        self.reconnects.retain(|r| live.contains(&r.id));
        for snapshot in &snapshots {
            let reason = failure_reason(snapshot).unwrap_or_default();
            let record = self.phase_since.entry(snapshot.id).or_insert(PhaseRecord {
                phase: snapshot.phase,
                since: now,
                reason: reason.clone(),
            });
            if record.phase != snapshot.phase || record.reason != reason {
                *record = PhaseRecord {
                    phase: snapshot.phase,
                    since: now,
                    reason,
                };
            }
        }
        // A session that is not failed has nothing to retry, and the budget it spent
        // belongs to the failure that is over.
        for snapshot in &snapshots {
            if snapshot.phase != ForwardPhase::Failed {
                self.attempts.remove(&snapshot.id);
                self.reconnects.retain(|r| r.id != snapshot.id);
            }
        }
        let mut queued = Vec::new();
        for snapshot in snapshots
            .iter()
            .filter(|snapshot| snapshot.phase == ForwardPhase::Failed)
        {
            // One attempt at a time. The budget is spent by attempts, and this pass runs on
            // every repaint, so a panel that redraws to count a forward down to its next
            // attempt would otherwise queue — and spend — the whole budget before the first
            // one had run.
            if self
                .reconnects
                .iter()
                .any(|reconnect| reconnect.id == snapshot.id)
            {
                continue;
            }
            let reason = failure_reason(snapshot).unwrap_or_default();
            let spent = match self.attempts.get(&snapshot.id) {
                Some((spent_reason, spent)) if *spent_reason == reason => *spent,
                // A new failure starts with a full budget.
                _ => 0,
            };
            if spent >= RECONNECT_DELAYS.len() {
                continue;
            }
            self.attempts
                .insert(snapshot.id, (reason.clone(), spent + 1));
            queued.push(Reconnect {
                id: snapshot.id,
                reason,
                due: now + RECONNECT_DELAYS[spent],
            });
        }
        for reconnect in queued {
            self.queue_reconnect(reconnect, cx);
        }
        snapshots
    }

    /// Wait out the gap, then start one attempt. The attempt checks the session is still
    /// broken for the same reason before it touches it, so a forward the user stopped
    /// or a Pod that came back is not disturbed.
    fn queue_reconnect(&mut self, reconnect: Reconnect, cx: &mut Context<Self>) {
        let id = reconnect.id;
        let reason = reconnect.reason.clone();
        let delay = reconnect.due.saturating_duration_since(Instant::now());
        self.reconnects.push(reconnect);
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(delay).await;
            view.update(cx, |view, cx| view.run_reconnect(id, &reason, cx))
                .ok();
        })
        .detach();
    }

    fn run_reconnect(&mut self, id: ForwardId, reason: &SharedString, cx: &mut Context<Self>) {
        self.reconnects.retain(|reconnect| reconnect.id != id);
        let Some(snapshot) = self.forward(id, cx) else {
            return;
        };
        if snapshot.phase != ForwardPhase::Failed {
            return;
        }
        if failure_reason(&snapshot).unwrap_or_default() != *reason {
            return;
        }
        // The dock refuses anything but a failed session, and a refusal here means the
        // session ended between the check and the call. Either way the budget is spent:
        // this panel does not spin.
        let _ = self.dock.update(cx, |dock, cx| dock.retry_forward(id, cx));
        cx.notify();
    }

    /// The countdown a reconnecting row shows, or nothing when it is not waiting.
    fn reconnect_remaining(&self, id: ForwardId, now: Instant) -> Option<Duration> {
        self.reconnects
            .iter()
            .find(|reconnect| reconnect.id == id)
            .map(|reconnect| reconnect.due.saturating_duration_since(now))
    }

    /// One repaint a second while the list shows a number that moves.
    fn ensure_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if view
                    .update(cx, |view, cx| {
                        let stamp = view.time_stamp(cx);
                        if stamp != view.stamp {
                            view.stamp = stamp;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Every number the list draws as elapsed time, joined. A second that changes none of
    /// them costs no repaint.
    fn time_stamp(&self, cx: &App) -> String {
        let now = Instant::now();
        self.snapshots(cx)
            .iter()
            .map(|snapshot| {
                format!(
                    "{:?}{}",
                    snapshot.id,
                    elapsed_text(phase_age(&self.phase_since, snapshot.id, now))
                )
            })
            .chain(self.reconnects.iter().map(|reconnect| {
                format!(
                    "{:?}r{}",
                    reconnect.id,
                    reconnect.due.saturating_duration_since(now).as_secs()
                )
            }))
            .collect::<Vec<_>>()
            .join(",")
    }

    // ── Rendering ───────────────────────────────────────────────────────────

    /// The toolbar: what the surface is, the filter, the confirmation of a copy, and the
    /// two actions the list has.
    ///
    /// This used to be a 40px `surface.chrome` title bar carrying the title and the count,
    /// above a 32px `surface.content` strip carrying the filter — two bands where the Helm
    /// panel beside it has one, and neither of them §4.2's resource header. It is one 32px
    /// `surface.content` band now, laid out the way the Helm toolbar is, so the two centre
    /// lists open with the same silhouette and the same 16px left edge.
    fn render_toolbar(
        &self,
        new_callback: Option<NewForwardCallback>,
        stoppable: usize,
        visible: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let new_enabled = new_callback.is_some();
        // Nothing here is a wiring problem, and a user cannot act on one either way, so the
        // disabled state says the thing that would actually help: there is nothing to forward
        // *to* until a Service or a Pod is in view. The old wording named the Shell, which is
        // a module in this repository and not a thing a reader has ever heard of.
        let new_tooltip = "Forward a port. Right-click a Service or a Pod to fill this in.";
        let filtering = !self.filter.trim().is_empty();
        let stop_all_aria = if stoppable == 1 {
            "Stop the only port forward that is still starting or running".to_owned()
        } else {
            format!("Stop all {stoppable} port forwards that are still starting or running")
        };
        h_flex()
            .id("forwards-toolbar")
            .debug_selector(|| "forwards-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::SUMMARY_STRIP)
            .min_w(px(0.))
            // `UI-SPEC.md` §4.4 puts the strip's content at the same 16px edge the table's
            // own cells start at — the one `CONTENT_INSET` spine every docked panel now reads.
            // This panel used a 12px edge on a 40px `surface.chrome` band above a 32px
            // `surface.content` one, and the Helm panel beside it a third arrangement — three
            // bands for the same job, none of them §4.2's resource header.
            .px(common::CONTENT_INSET)
            .gap(space::SM)
            .items_center()
            // The band is 32px tall and its contents are a fixed 208px filter, a title and
            // two buttons, which is more than `design::size::CENTER_MIN` (480) leaves after
            // the padding. The Helm toolbar beside it scrolls rather than clipping, and two
            // sibling panels that both lose their right-hand control at the same width are
            // worse than one that scrolls. Reported upward as a shared-chrome decision:
            // the real fix is a toolbar that sheds its title first, and that is one change
            // in one place for both panels rather than one per panel.
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .bg(role::surface_content(cx))
            .child(
                Icon::new(IconName::Network)
                    .flex_none()
                    .with_size(Size::Size(design::icon::IN_ROW))
                    // The panel's own mark at the resting ink of a control's glyph,
                    // which is also what the Helm mark beside it wears: in the
                    // count tier the two toolbars a reader compares no longer agreed
                    // on how loud their mark is, and the quieter one read as off.
                    .text_color(design::icon::resting(cx)),
            )
            // A panel title is a title, so `fg_primary` — which is what
            // `label_panel_title` already carries. Named at the call site because this
            // panel names the ink on every `label_*` it draws: the shared helpers' inks
            // are a floor that keeps a forgotten call site quiet, not a decision this
            // panel has made, and a reader comparing this toolbar with Helm's should not
            // have to know that to be sure the two agree.
            .child(common::label_panel_title(TITLE).text_color(role::fg_primary(cx)))
            .child(self.filter_input.clone())
            // How much of the list the filter leaves, so a long list stays scannable and a
            // filter that hides everything is visible as a count rather than an empty list.
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
                            common::label_small(format!("{visible} of {total}"))
                                .text_color(role::fg_tertiary(cx)),
                        ),
                )
            })
            .child(div().flex_1().min_w(space::SM))
            // A copy or an open has no visible result, so the toolbar says what happened.
            // The confirmation belongs to the copy that made it: a second copy restarts
            // the clock, and a confirmation from the first one does not outlive its own
            // 1.5s in the middle of the second one's.
            .when_some(
                self.feedback
                    .as_ref()
                    .filter(|feedback| feedback.is_current(Instant::now())),
                |this, feedback| {
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
                                Icon::new(design::health_icon(feedback.severity))
                                    .flex_none()
                                    .with_size(Size::Size(design::icon::IN_ROW))
                                    .text_color(severity_role(feedback.severity, cx)),
                            )
                            .child(
                                common::label_small(feedback.message.clone())
                                    .text_color(role::fg_secondary(cx))
                                    .truncate(),
                            ),
                    )
                },
            )
            // A forward that is already stopping has nothing left to stop, so the action
            // only appears while it can still end a forward.
            .when(stoppable > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .debug_selector(|| "forwards-stop-all".to_owned())
                        .child(
                            Button::new("forwards-stop-all")
                                // The object is named rather than left to the panel title 200px
                                // away: this is the one control on the row that ends *every*
                                // session at once, so "what does all of it apply to" is the question
                                // the label has to answer before the click.
                                .label("Stop all forwards")
                                .ghost()
                                .with_size(Size::Size(design::size::CONTROL))
                                .tab_index(1isize)
                                .tooltip("Stop every forward that is still starting or running")
                                .accessibility_label(stop_all_aria)
                                .on_click(cx.listener(|view, _, _, cx| view.stop_all(cx))),
                        ),
                )
            })
            .child(
                Button::new("forwards-new")
                    // `New forward…`, not `+ New`.
                    //
                    // Two of the guide's rules at once. "Append `…` to every Button that opens a
                    // dialog, a sheet, or a separate window", which this does — it opens the
                    // port-forward form. And "Name the scope when context does not make it clear":
                    // the panel title says `Port forwards`, but `+ New` is a gesture with a plus
                    // sign rather than a verb, so the button does not say what it creates. The `+`
                    // also duplicates what the button already looks like, and `New` on its own is
                    // one of the "wrappers" the guide's word list rules out.
                    .label("New forward…")
                    .with_size(Size::Size(design::size::CONTROL))
                    // `PROMPT.md` §2.1 rule 8 spends one primary per screen, and this
                    // screen's accent goes on the address the reader clicks instead.
                    .secondary()
                    .tab_index(1isize)
                    .tooltip(new_tooltip)
                    .accessibility_label("New port forward")
                    .disabled(!new_enabled)
                    .on_click(move |_, window, cx| {
                        if let Some(callback) = &new_callback {
                            callback(window, cx);
                        }
                    }),
            )
            .into_any_element()
    }

    /// The strip above the list: what the sessions add up to.
    ///
    /// `UI-SPEC.md` §4.4 puts this above a table, and the Helm panel beside this one draws
    /// the same band for the same reason. It used to be a line in this panel's own title bar
    /// instead, which put the count 40px above the rows it counts and gave the two sibling
    /// centre views two different silhouettes.
    ///
    /// Drawn only when there is something to say: an empty list has an empty state that says
    /// so, and `0 active · 0 failed` above it says it a second time.
    fn render_summary(&self, summary: ForwardSummary, cx: &App) -> Option<AnyElement> {
        if summary.active == 0 && summary.failed == 0 {
            return None;
        }
        let text = summary_text(summary);
        Some(
            h_flex()
                .id("forwards-summary")
                .debug_selector(|| "forwards-summary".to_owned())
                .flex_none()
                .w_full()
                .h(design::size::SUMMARY_STRIP)
                .min_w(px(0.))
                .px(common::CONTENT_INSET)
                .gap(space::SM)
                .items_center()
                .role(Role::Status)
                .aria_label(format!("{SUMMARY_LABEL}: {text}"))
                .child(
                    common::label_small(text)
                        .text_color(role::fg_secondary(cx))
                        .truncate(),
                )
                .into_any_element(),
        )
    }

    /// A group heading: the section's name and how many are in it.
    ///
    /// The name is uppercased by `common::section_heading` rather than in `Section::title`,
    /// because the section name is also this element's `ElementId` and debug selector — a test
    /// and an accessibility label both read the sentence case.
    ///
    /// The heading used to lead with a 6px dot beside every title, in `danger` on the Failed
    /// one — uppercase, semibold **and** a strong channel in one region, which is the
    /// combination the typography guide rules out ("Do not combine uppercase, strong color,
    /// and bold weight in the same region"). The healthy sections' dot carried no state the
    /// name and the count beside it did not already say, so the dot is gone. The one section
    /// whose state *is* the news — Failed — says it in the word instead, in the danger
    /// channel's word ink: shape plus ink, one channel, never colour alone.
    fn render_section_heading(&self, section: Section, count: usize, cx: &App) -> AnyElement {
        let heading = section.title();
        let title = format!("{} ({})", heading, design::format::count(count));
        h_flex()
            .id(SharedString::from(format!("forwards-section-{heading}")))
            .debug_selector(move || format!("forwards-group-{heading}"))
            .flex_none()
            .w_full()
            // A group heading, not a table row: the search panel's headings wear
            // the same token.
            .h(design::size::GROUP_HEAD)
            // The toolbar above this list and the Helm table beside it both start their
            // content at `space::LG`. The rows were four pixels inside that edge, so the list
            // had two left spines: one for the panel title and one for every name in it, and
            // a heading that stepped back out again above the rows it introduces.
            .px(space::LG)
            .gap(space::SM)
            .items_center()
            .role(Role::Group)
            .aria_label(title.clone())
            .child(common::section_heading(title).text_color(match section {
                Section::Failed => role::status_word_for(Severity::Error, cx),
                _ => role::fg_tertiary(cx),
            }))
            .into_any_element()
    }

    /// The address a forward answers on, as the link it is.
    ///
    /// `UI-SPEC.md` §14.3: a port forward exists to be used in a browser, and a list
    /// you have to read the digits off is a list you have to transcribe. The underline
    /// is the affordance, the accent is the colour, and a forward with no listener has
    /// neither — it draws inert text so the row never advertises a port that answers
    /// nothing.
    ///
    /// The link is built here rather than taken from `gpui_kit::component::link::Link`, which
    /// does open the address but brings four things this app has already decided against:
    ///
    /// - it paints the text in `theme().link` and the underline in `theme().link @ 50%`, so
    ///   the address and the rule under it are two different blues, and neither is an app role
    ///   (`PROMPT.md` §2.1 rule 3). The text was being repainted in the app's accent and the
    ///   underline underneath it in the component's, which is not a design decision, it is
    ///   two designs fighting;
    /// - its `hover` repaints the text in `theme().link @ 80%`, so the accent a reader is
    ///   asked to recognise changes the moment the pointer arrives;
    /// - it sets `cursor_pointer`, which `PROMPT.md` §3 lists as a web habit the project does
    ///   not have;
    /// - it declares no `Role::Link` and no accessible name, so a screen reader announces the
    ///   one control in this panel that exists to be activated as unlabelled body text.
    ///
    /// Six lines of `div` in the app's own roles is cheaper than any of the four.
    fn render_address(&self, snapshot: &ForwardSnapshot, cx: &App) -> AnyElement {
        let name = SharedString::from(format!("forwards-address-{}", id_key(snapshot.id)));
        let (Some(address), Some(url)) = (forward_address(snapshot), forward_url(snapshot)) else {
            return div()
                .id(name.clone())
                .debug_selector(move || name.to_string())
                .w(px(ADDRESS_WIDTH))
                .flex_none()
                // `fg_tertiary`, the role for a value the context has taken away, rather than
                // `fg_disabled`: nothing here is unavailable, the session simply holds no
                // listener to print.
                .child(common::label_body("—").text_color(role::fg_tertiary(cx)))
                .into_any_element();
        };
        // The two chords that belong to this panel's whole purpose, named where the
        // affordance is rather than in a permanent strip under the list.
        let tooltip = SharedString::from(format!(
            "Open {url} in the browser · Control O opens it · Control C copies it"
        ));
        let spoken = format!("{url}, open in the browser");
        let accent = role::accent(cx);
        let mut address_label = common::label_body(address).text_color(accent).truncate();
        // The colour alone is not the affordance. `mockup/secondary.html` underlines the
        // address, and it is right to: in greyscale, and to a reader who cannot separate
        // this accent from the row it sits on, the underline is the whole signal. gpui 0.6.6
        // has no `text-decoration-color` shorthand, so the rule is set on the text style.
        address_label.text_style().underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(accent),
            wavy: false,
        });
        let target = url.clone();
        let selector = name.clone();
        // Tabular figures, from the reader's configured data font, for the same reason the time
        // column has them: the port is the number in this panel, a proportional face gives `1` a
        // narrower advance than `8`, and a list of addresses whose digits do not line up is a list
        // a reader reads one row at a time. The address stays left-aligned — it is an identifier
        // and a link target, and the guide aligns those to the left — so the figures are what makes
        // the digits form a column inside it.
        let features = settings::data_typography(cx).features.clone();
        let mut link = div()
            .id(format!("forward-link-{name}"))
            .debug_selector(move || selector.to_string())
            .w(px(ADDRESS_WIDTH))
            .flex_none()
            .h(design::size::ROW)
            .items_center()
            .font_features(features)
            .role(Role::Link)
            .aria_label(spoken)
            // `on_mouse_down` rather than `on_click`, so the browser opens on the press the
            // reader sees the row react to. It deliberately does *not* stop propagation:
            // GPUI registers an outer element's handler before an inner one's, so the row
            // has already selected itself by the time this runs, and a reader who clicks
            // the address of row three and finds row two selected afterwards would be right
            // to be annoyed. Selecting the row you clicked is what a row is for.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| cx.open_url(&target))
            .child(address_label);
        // The link paints no plate of its own, at rest or on hover, and that is the whole
        // reason it stayed legible.
        //
        // It used to set a resting background of `surface_content` and a hover of the accent
        // with 4% of the local ink over it. Both were artefacts of the link treating itself as
        // a surface: the resting fill painted an *unselected* plate inside whichever row it
        // was in, so selecting a row put a content-coloured rectangle around its own address,
        // and the hover put the accent — the address's own ink — on an accent plate, which made
        // the link invisible at the one moment the reader was looking for it.
        //
        // The row already owns both states and paints the wash across its whole width, so the
        // plate was never what told the reader anything. What tells them is the accent, the
        // underline, and the row's own hover underneath.
        link.interactivity()
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
        link.into_any_element()
    }

    /// One session. The status lane, the name, the address to click, the remote end,
    /// the target, the time, and the one or two controls that end or revive the session.
    ///
    /// `keyboard_focus` is whether the panel — not the pointer — holds the focus, and it is what
    /// separates the row the keyboard is on from the row the reader last pressed: both are
    /// `selected`, so without it the two states are one colour and the caret is invisible to the
    /// reader who is not looking at the pointer.
    fn render_row(
        &self,
        snapshot: &ForwardSnapshot,
        index: usize,
        now: Instant,
        keyboard_focus: bool,
        panel: &gpui_kit::WeakEntity<ForwardsView>,
        cx: &App,
    ) -> AnyElement {
        let id = snapshot.id;
        let age = phase_age(&self.phase_since, id, now);
        let selected = self.selected == Some(id);
        let target = row_target(snapshot);
        let reason = failure_reason(snapshot);
        let (severity, phase_word) = phase_status(snapshot.phase, age);
        // What the time column says. A running session counts up, a failure counts
        // from when it broke, and a session this panel is reviving counts down to the
        // attempt — never silently.
        let reconnecting = self.reconnect_remaining(id, now);
        let time_text = match reconnecting {
            Some(remaining) => format!("Reconnecting… {}s", remaining.as_secs().max(1)),
            None => match snapshot.phase {
                ForwardPhase::Failed => {
                    if age < Duration::from_secs(60) {
                        "just now".to_owned()
                    } else {
                        format!("{} ago", elapsed_text(age))
                    }
                }
                ForwardPhase::Stopped => "Stopped".to_owned(),
                _ => elapsed_text(age),
            },
        };
        let time_role = match reconnecting {
            // The time is a word, so it takes the word ink: the status word
            // beside it reads `warning_word` and two neighbours in one channel
            // must not read at two brightnesses.
            Some(_) => role::warning_word(cx),
            None => match snapshot.phase {
                ForwardPhase::Stopped => role::fg_disabled(cx),
                _ => role::fg_tertiary(cx),
            },
        };
        let time_id = SharedString::from(format!("forwards-time-{}", id_key(id)));
        let time_label = SharedString::from(time_text);
        // Status as a mark *and* a word, on one fixed lane.
        //
        // The lane is fixed so every row's name starts at the same x, which is the identity spine a
        // list is scanned along; a dot that is present in one row and absent in the next moves the
        // name by six pixels on exactly the rows a reader is comparing. The mark is 6px because
        // that is the mark token and a 6px shape is not a bullet, and the word beside it is the
        // text equivalent, so the state survives a greyscale screenshot and a reader who cannot
        // separate the channel's colour from the row's surface.
        //
        // The two inks come from one severity: `status_for` is the mark, `status_word_for` is the
        // word. `Success` is secondary ink in both, which is the product's inversion — a forward
        // that is working is not a thing that gets marked.
        let status = h_flex()
            .id(SharedString::from(format!(
                "forwards-status-{}",
                id_key(id)
            )))
            .flex_none()
            .w(px(STATUS_LANE_WIDTH))
            .h(design::size::ROW)
            .gap(STATUS_MARK_GAP)
            .items_center()
            .child(
                div()
                    .flex_none()
                    .size(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(role::status_for(severity, cx)),
            )
            .child(
                common::label_small(phase_word)
                    .min_w(px(0.))
                    .text_color(role::status_word_for(severity, cx))
                    .truncate(),
            );
        let main = h_flex()
            .h(design::size::ROW)
            .gap(space::SM)
            .items_center()
            .min_w(px(0.))
            // `fg_primary`, and stated here rather than inherited.
            //
            // This is the session's name — the one thing the eye lands on in the row, and
            // what every other cell in the row is read against. `UI-SPEC` §1.4 gives
            // `fg.primary` to 对象名, and `label_body`'s own default is `fg.secondary`
            // because its usual job is 正文. Taking that default here is the one place
            // this panel's hierarchy would have been silently inverted: the name would
            // have sat *below* the address and the target in ink while outranking both
            // in size. gpui-kit's `Label` also swallows an ancestor's `text_color`, so
            // putting the role on the wrapping `h_flex` would not reach the glyph either.
            .child(
                common::label_body(row_name(snapshot))
                    .text_color(role::fg_primary(cx))
                    .flex_none()
                    .w(px(NAME_LANE_WIDTH))
                    .truncate(),
            )
            .child(self.render_address(snapshot, cx))
            .child(
                label_quiet("→")
                    .flex_none()
                    .text_color(role::fg_disabled(cx)),
            )
            // The remote end is this list's number column, so it gets the number treatment: one
            // fixed lane, right-aligned, with the reader's tabular figures. "8080/tcp" beside
            // "8080" beside "9000/tcp" left-aligned is three different left edges, and the digit a
            // reader is comparing is in the middle of the cell where nothing aligns it.
            //
            // The *local* port is not right-aligned, and deliberately so: it lives inside
            // `localhost:34567`, which is a link target and an identifier, and the guide aligns
            // identifiers and links to the leading edge. Its lane is fixed and it carries tabular
            // figures, so its digits form a column without the cell reading as a column of
            // right-aligned numbers.
            .child(
                h_flex()
                    .flex_none()
                    .w(px(REMOTE_LANE_WIDTH))
                    .font_features(settings::data_typography(cx).features.clone())
                    .justify_end()
                    .child(
                        label_quiet(remote_text(snapshot))
                            .min_w(px(0.))
                            .text_color(role::fg_secondary(cx))
                            .truncate(),
                    ),
            )
            .child(
                label_quiet(target.clone())
                    .min_w(px(0.))
                    .truncate()
                    .text_color(role::fg_tertiary(cx)),
            )
            .child(div().flex_1().min_w(space::SM))
            .child(
                div()
                    .id(time_id.clone())
                    .debug_selector(move || time_id.to_string())
                    .flex_none()
                    // The word it prints is prose — `Stopped`, `just now`,
                    // `Reconnecting… 12s` — so it takes the UI rung the row's other quiet
                    // columns wear, not the mono one: `MONO_XS` is the size for a UID or a
                    // port, and a status word is neither. The digits keep the tabular
                    // figures every number column in the panel carries.
                    .text_size(design::text::LABEL)
                    .line_height(design::text::LABEL_LINE_HEIGHT)
                    .text_color(time_role)
                    .font_features(settings::data_typography(cx).features.clone())
                    .child(time_label),
            );
        let reason_id = SharedString::from(format!("forwards-reason-{}", id_key(id)));
        let step = failure_step(snapshot);
        // Three attempts, then the panel stops. A row that reads the same as one that has not
        // tried yet would leave the reader waiting for a fourth attempt that is never coming,
        // so the row says the automatic retry has stopped and leaves the retry to the reader.
        let retry_stopped = snapshot.phase == ForwardPhase::Failed
            && reconnecting.is_none()
            && self
                .attempts
                .get(&id)
                .is_some_and(|(_, spent)| *spent >= RECONNECT_DELAYS.len());
        // A row has a second line when the runtime said something or the retries ran out, and
        // not otherwise.
        let explained = reason.is_some() || retry_stopped;
        let body = v_flex()
            .flex_1()
            .min_w(px(0.))
            .gap(space::XXS)
            .child(main)
            // A failure has to say which step failed, in the row and not in a tooltip:
            // the second line is the whole point of `UI-SPEC.md` §14.3.
            .when(explained, |this| {
                this.child(
                    h_flex()
                        .id(reason_id.clone())
                        .debug_selector(move || reason_id.to_string())
                        .gap(space::SM)
                        .items_center()
                        .child(
                            common::label_small(step.unwrap_or("The forward ended"))
                                .flex_none()
                                .text_color(role::danger_word(cx)),
                        )
                        // The runtime's sentence is longer than a 32px row, so the
                        // row truncates it and the tooltip carries all of it.
                        .when_some(reason, |this, reason| {
                            let sentence = reason.to_string();
                            this.child(common::with_tooltip(
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "forwards-reason-text-{}",
                                        id_key(id)
                                    )))
                                    .min_w(px(0.))
                                    .child(
                                        common::label_small(sentence.clone())
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_color(role::fg_secondary(cx)),
                                    ),
                                sentence,
                            ))
                        })
                        .when(retry_stopped, |this| {
                            this.child(
                                common::label_small("Automatic retry has stopped.")
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(role::warning_word(cx)),
                            )
                        }),
                )
            });
        let base = role::surface_content(cx);
        // Selection, hover and cursor are three states and three different answers.
        //
        // The selection is `design::row_selected_bg`, the same wash the sidebar's selected row and
        // the shared table's selected row take, because a list that spells "selected" one way in
        // one panel and another way in the next teaches a reader to look for the difference rather
        // than for the row. The 2px accent rail this used to draw down the leading edge is gone:
        // the guide is explicit that a selection marker belongs on the item's own surface, and a
        // one-sided bar "breaks the item's rounded silhouette and adds a second, competing edge to
        // a column that already aligns on its text".
        //
        // The cursor is the keyboard's position and is a different thing from the selection, so it
        // gets the focus wash and a 2px caret. Both are `selected` in this model — a press and an
        // arrow key land in the same cell — so the caret is drawn only while the panel holds the
        // focus, and a reader who pressed a row with the pointer is not shown a caret they did not
        // ask for.
        //
        // `UI-SPEC.md` §4.4: a selected row keeps its wash under the pointer and takes the hover
        // tint *on top of it*. A single hover colour replaced the wash outright, so the row the
        // reader had chosen was the one row that lost its selection the moment the pointer arrived
        // — the selection read as flickering rather than as held.
        let selected_bg = if selected && keyboard_focus {
            design::row_focus_bg(cx)
        } else if selected {
            design::row_selected_bg(cx)
        } else {
            base
        };
        let hover = design::row_hover_bg(cx);
        let hover_selected = design::state::hover_on(selected_bg, role::fg_primary(cx));
        let row = h_flex()
            .id(SharedString::from(format!("forwards-item-{}", id_key(id))))
            .debug_selector(move || format!("forwards-row-{index}"))
            .aria_label(row_aria_label(snapshot, retry_stopped))
            .aria_row_index(index + 1)
            .aria_keyshortcuts("Enter Control+C Control+O")
            .w_full()
            .min_w(px(0.))
            // The panel's left edge, the same `CONTENT_INSET` spine the toolbar and the
            // summary strip start at and the same one the Helm table's own cells start at
            // beside it. At `space::MD` every name in this list sat four pixels inside the
            // title above it, which is one alignment spine for the chrome and another for
            // the data.
            .px(common::CONTENT_INSET)
            .gap(space::SM)
            .items_start()
            .relative()
            // A row with a second line is taller than the shared 32px, and the extra
            // breathing room goes on the side that has one: a session that is fine stays
            // exactly one row tall, so a list of them scans as a grid.
            .when(explained, |this| this.pt(space::XXS).pb(space::XXS))
            .bg(selected_bg)
            .hover(move |this| this.bg(if selected { hover_selected } else { hover }))
            .on_mouse_down(MouseButton::Left, {
                let panel = panel.clone();
                move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |view, cx| {
                            if view.selected != Some(id) {
                                view.selected = Some(id);
                                cx.notify();
                            }
                        });
                    }
                }
            })
            // The caret is reserved on every row and painted on one, so a row never changes width
            // when the cursor moves along it. It is the keyboard's position, not the selection's
            // decoration: the wash above already says which row is chosen.
            .border_l(design::size::SELECTION_RAIL)
            .border_color(if selected && keyboard_focus {
                role::accent(cx)
            } else {
                role::border_subtle(cx).alpha(0.)
            })
            // The status lane is the row's leading edge, so a failed row's second line
            // starts under the name rather than under the mark.
            .child(status)
            .child(body)
            .child(
                h_flex()
                    .flex_none()
                    .h(design::size::ROW)
                    .gap(space::XS)
                    .items_center()
                    .child(render_action(row_action(snapshot), &target, panel, cx))
                    // Stopping is a cross and not a bin: it stops a service, it does not
                    // delete a resource, and the shape difference says so.
                    .child(render_stop(snapshot, &target, panel, cx)),
            );
        row.into_any_element()
    }

    /// The list itself: the sections, their headings, and their rows.
    fn render_list(
        &self,
        forwards: &[ForwardSnapshot],
        filtering: bool,
        connected: bool,
        keyboard_focus: bool,
        panel: &gpui_kit::WeakEntity<ForwardsView>,
        cx: &App,
    ) -> AnyElement {
        if forwards.is_empty() {
            return self.render_empty(filtering, connected, &self.filter_input);
        }
        let now = Instant::now();
        let mut children: Vec<AnyElement> = Vec::new();
        let mut index = 0;
        for (position, (section, members)) in build_sections(forwards).iter().enumerate() {
            // One heading per session class, so the gap between two of them is the
            // divider. A row-height gap read as one more row.
            if position > 0 {
                children.push(div().flex_none().h(space::LG_PLUS).into_any_element());
            }
            children.push(self.render_section_heading(*section, members.len(), cx));
            for snapshot in members {
                children.push(self.render_row(snapshot, index, now, keyboard_focus, panel, cx));
                index += 1;
            }
        }
        v_flex()
            .id("forwards-list")
            .debug_selector(|| "forwards-list".to_owned())
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .w_full()
            .pt(space::SM)
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .children(children)
            .into_any_element()
    }

    /// What the list shows while it has no rows: the first nothing, or a filter that
    /// hid everything, which has to say so rather than claim the cluster has none.
    ///
    /// §4.13's table gives the filtered case an action and this panel had none: it
    /// told the reader to clear the filter and left them to find the field. The way
    /// out of a state is a control, not a sentence — the sentence is only there to
    /// say how many filters are running.
    fn render_empty(
        &self,
        filtering: bool,
        connected: bool,
        filter: &Entity<TextInput>,
    ) -> AnyElement {
        // Three states, and which one this is decides what the sentence may point at.
        //
        // The empty cluster and the disconnected cluster are both "no forwards", and both used to
        // draw the same sentence — one naming a right-click on a Service, which is an instruction
        // against a menu that does not exist when there is no cluster to read one from. So the
        // disconnected state says what is missing and what to do about it, in one sentence, with
        // no control: the way out of it is in the title bar 40px above, not on this panel, and a
        // button that dispatched nothing would be worse than no button.
        let (icon, title, hint) = match (filtering, connected) {
            (true, _) => (
                design::problems_filter_icon(true),
                FILTERED_EMPTY_TITLE,
                FILTERED_EMPTY_HINT,
            ),
            (false, false) => (IconName::Unplug, DISCONNECTED_TITLE, DISCONNECTED_HINT),
            (false, true) => (IconName::Box, EMPTY_TITLE, EMPTY_HINT),
        };
        // No window is handed on, the same reason the Helm panel's clear is a weak
        // update: `TextInput::clear` needs the window that the click already carries,
        // because it is the call that moves the caret and drops a pending composition.
        let clear = filtering.then(|| {
            // The field's own entity, not the panel's weak handle.
            //
            // `Entity<TextInput>::update` is what `search.rs::clear_query` calls, and
            // the reason it calls *that* is the panic in `ForwardsView::new`: going
            // through the panel re-enters it. `TextInput::clear` is also the call that
            // moves the caret and drops a pending composition, which is why the click
            // has to hand the window on rather than the panel setting a string.
            let filter = filter.clone();
            // The wrapper carries the selector, because a `Button` exposes no debug
            // hook of its own — the same reason the cancel button beside the busy
            // spinner needs one.
            div()
                .flex_none()
                .debug_selector(|| "forwards-clear-filter".to_owned())
                .child(
                    Button::new("forwards-clear-filter")
                        .label("Clear filter")
                        .secondary()
                        .with_size(Size::Size(design::size::CONTROL))
                        .tab_index(2isize)
                        .tooltip("Show every port forward again")
                        .accessibility_label("Clear the port forward filter")
                        .on_click(move |_, window, cx| {
                            filter.update(cx, |input, cx| input.clear(window, cx));
                        }),
                )
                .into_any_element()
        });
        v_flex()
            .id("forwards-empty")
            .debug_selector(|| "forwards-empty".to_owned())
            .role(Role::Status)
            .aria_label(title)
            .aria_description(hint)
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .w_full()
            .child(
                v_flex()
                    // The whole list, so §4.13's 居中 has something to centre inside.
                    //
                    // This was `div().size_full()`, and `size_full()`'s `height: 100%`
                    // resolved to `auto` here — the box came out 96px tall (its own
                    // content) inside an 868px list, so the shared state's own box was
                    // 96px too and the glyph sat 0px under the toolbar, which is the
                    // 贴顶 the same paragraph rules out. `flex_1()` states the same
                    // intent without the percentage, and this is also the box a
                    // `h_full()` on `common::EmptyState`'s wrapper will resolve
                    // against — see
                    // `the_empty_state_gets_the_whole_list_to_center_itself_in`.
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(f32::from(design::size::ROW) * 4.))
                    // The problems glyph has one owner, so this panel does not keep a
                    // second funnel of its own; the state that names the missing thing
                    // says so with its own title rather than borrowing a failure shape.
                    .child(common::empty_state_with_action(icon, title, hint, clear)),
            )
            .into_any_element()
    }
}

/// The control that ends or revives a session: `Start`, `Retry`, or a waiting glyph
/// while one is stopping. A running session is ended by the cross beside it.
fn render_action(
    action: RowAction,
    target: &str,
    panel: &gpui_kit::WeakEntity<ForwardsView>,
    _cx: &App,
) -> AnyElement {
    let id = action.id();
    let name = SharedString::from(format!("{}-{}", action.name(), id_key(id)));
    let control = match action {
        // Start and Retry are one control in two words: both begin a new attempt, and
        // the phase says which of the two the row offers.
        RowAction::Start(_) | RowAction::Retry(_) => {
            let word = if matches!(action, RowAction::Start(_)) {
                "Start"
            } else {
                "Retry"
            };
            let panel = panel.clone();
            Button::new(name.clone())
                .label(word)
                .secondary()
                .with_size(Size::Size(design::size::CONTROL))
                .tab_index(2isize)
                .tooltip(format!("{word} this forward"))
                .accessibility_label(format!("{word} port forward for {target}"))
                .on_click(move |_, _, cx| {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |view, cx| {
                            view.activate(action, cx);
                            cx.stop_propagation();
                        });
                    }
                })
                .into_any_element()
        }
        // Names the object in both channels, for the same reason the stop cross does. The tooltip
        // said "Waiting for the port forward to stop" while the accessible name beside it said
        // "Stopping port forward for kube-system/dns" — the same sentence with the object removed
        // from one of them.
        RowAction::Waiting(_) => {
            let spoken = format!("Stopping the forward to {target}");
            let hint = format!("Waiting for the forward to {target} to stop");
            common::reusable_icon_button(name.clone(), IconName::LoaderCircle, spoken)
                .tab_index(2isize)
                .disabled(true)
                .tooltip(hint)
                .into_any_element()
        }
        RowAction::Stop(_) => div().into_any_element(),
    };
    // The control is wrapped so it carries a debug selector. An element id alone stays
    // invisible to `VisualTestContext::debug_bounds` and to the inspector, which is how
    // a Retry that rendered for real could still read as missing.
    div()
        .flex_none()
        .debug_selector(move || name.to_string())
        .child(control)
        .into_any_element()
}

/// The cross that stops a session.
///
/// `UI-SPEC.md` §14.3: stopping is not deleting. A bin says "remove this thing from
/// the cluster"; a cross says "end this service", which is what a click here does. A
/// session that cannot be stopped keeps the control in place and disabled, so the
/// shape stays the shape for "stopped" rather than for "broken".
fn render_stop(
    snapshot: &ForwardSnapshot,
    target: &str,
    panel: &gpui_kit::WeakEntity<ForwardsView>,
    cx: &App,
) -> AnyElement {
    let id = snapshot.id;
    let name = SharedString::from(format!("forwards-stop-{}", id_key(id)));
    let stoppable = is_stoppable(snapshot.phase);
    // Both names the object. The tooltip used to say "Stop this forward", which is the one thing
    // every other control on the row names: the accessible name says "Stop the forward to
    // kube-system/dns", the row's own failure line names the target, and the tooltip — the only
    // one of the three a sighted reader sees before pressing — said "this".
    let (label, tooltip) = if stoppable {
        (
            format!("Stop the forward to {target}"),
            format!("Stop the forward to {target}"),
        )
    } else {
        (
            format!("This forward to {target} is not running"),
            format!("The forward to {target} is not running, so it cannot be stopped"),
        )
    };
    let panel_for_click = panel.clone();
    let ink = role::fg_tertiary(cx);
    let selector = name.clone();
    div()
        .flex_none()
        .debug_selector(move || selector.to_string())
        .child(
            common::reusable_icon_button(name, IconName::X, label)
                .tab_index(2isize)
                .disabled(!stoppable)
                .tooltip(tooltip)
                .text_color(ink)
                .on_click(move |_, _, cx| {
                    if let Some(panel) = panel_for_click.upgrade() {
                        panel.update(cx, |view, cx| {
                            view.activate(RowAction::Stop(id), cx);
                            cx.stop_propagation();
                        });
                    }
                }),
        )
        .into_any_element()
}

impl Render for ForwardsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_ticker(cx);
        let snapshots = self.sync(cx);
        let summary = self.dock.read(cx).forward_summary();
        let filtering = !self.filter.trim().is_empty();
        let visible: Vec<ForwardSnapshot> = snapshots
            .iter()
            .filter(|snapshot| matches_filter(snapshot, &self.filter))
            .cloned()
            .collect();
        let panel = cx.entity().downgrade();
        // Whether the keyboard is in this list rather than the pointer, which is the
        // one fact that separates the row the cursor is on from the row that was last pressed.
        let keyboard_focus = self.focus_handle.is_focused(window);
        let connected = self.dock.read(cx).sessions_available();
        let list = self.render_list(&visible, filtering, connected, keyboard_focus, &panel, cx);
        v_flex()
            .id("forwards-view")
            .debug_selector(|| "forwards-view".to_owned())
            .role(Role::Region)
            .aria_label(TITLE)
            .aria_description(LIST_DESCRIPTION)
            .aria_keyshortcuts("ArrowUp ArrowDown Home End Enter Control+C Control+O /")
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(role::surface_content(cx))
            .text_color(role::fg_primary(cx))
            .key_context("Forwards")
            .track_focus(&self.focus_handle)
            .tab_index(0)
            .focus_visible(|style| style.border_l_2().border_color(role::accent(cx)))
            .child(self.render_toolbar(
                self.new_callback.clone(),
                stoppable_count(&snapshots),
                visible.len(),
                snapshots.len(),
                cx,
            ))
            .when_some(self.render_summary(summary, cx), |this, strip| {
                this.child(strip)
            })
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::{Render, TestAppContext, Window};

    use super::*;
    use crate::panels::terminal::{
        ForwardHandle, ForwardRequest, PortForwardFactory, StartedForward, TerminalFactory,
        TerminalServices,
    };

    /// The four panels this agent owns, read as text.
    ///
    /// A source scan is the only instrument that answers "does every `label_*` call
    /// site *name* its ink?", because `common::RoleLabel` deliberately resolves the
    /// colour from `cx.theme()` at paint time: nothing the test surface exposes —
    /// bounds, roles, names — says which of `fg.primary` / `secondary` / `tertiary`
    /// reached the screen.
    const OWNED_PANELS: [(&str, &str); 4] = [
        ("forwards.rs", include_str!("forwards.rs")),
        ("helm.rs", include_str!("helm.rs")),
        ("settings_view.rs", include_str!("settings_view.rs")),
        ("search.rs", include_str!("search.rs")),
    ];

    /// Every `label_*` call site in these four panels names its ink.
    ///
    /// **Why this is a rule and not a preference.** gpui-kit's `Label` renders as
    /// `div().line_height(rems(1.25)).text_color(cx.theme().foreground)` — the
    /// `text_color` is applied *after* the div inherits, so an ancestor's colour never
    /// arrives and a `RoleLabel`'s own default only applies when the call site named
    /// none. That default is a **floor, not a decision**: it is there so a forgotten
    /// call site stops rendering at gpui-kit's hard-coded `fg.primary` (which is
    /// heavier than the chrome around it and reads as a choice). The actual ink still
    /// has to be chosen, and the two ways of getting it wrong are both silent:
    ///
    /// - **Wrong value.** `forwards.rs`'s session name was `label_body`'s `fg_secondary`
    ///   for one round — an object name one step *below* the address beside it, which
    ///   is §1.4's `fg.tertiary`/`fg.secondary` upside down.
    /// - **Drift.** The value a call site agreed with is only recorded in the call.
    ///   Retype the helper's default, and the whole panel moves together.
    ///
    /// So the convention is: state the role at the call site, and put the reason in a
    /// comment there. This test is what keeps the convention from being a convention
    /// nobody remembers.
    #[test]
    fn every_shared_label_call_site_in_these_panels_names_its_ink() {
        for (name, source) in OWNED_PANELS {
            // Only the module body: the tests below contain the strings this scans for.
            // Go line by line rather than splitting on a marker with `\n` in it: a Windows
            // checkout may carry CRLF endings, where the literal marker never matches.
            let all: Vec<&str> = source.lines().collect();
            let marker = (0..all.len())
                .find(|index| {
                    all[*index].trim() == "#[cfg(test)]"
                        && all
                            .get(index + 1)
                            .is_some_and(|line| line.trim().starts_with("mod tests"))
                })
                .unwrap_or_else(|| panic!("{name} has no test module to stop the scan at"));
            let lines: Vec<&str> = all[..marker].to_vec();
            for (index, line) in lines.iter().enumerate() {
                let Some(column) = line.find("label_") else {
                    continue;
                };
                // Skip the `common::` re-exports and the helper definitions themselves.
                let rest = &line[column..];
                if !rest.starts_with("label_body(")
                    && !rest.starts_with("label_text(")
                    && !rest.starts_with("label_panel_title(")
                    && !rest.starts_with("label_metadata(")
                    && !rest.starts_with("label_small(")
                {
                    continue;
                }
                // Walk the chained call to its end: the statement runs until the parens
                // balance *and* the next line is not another `.method(`.
                let mut depth = 0i32;
                let mut end = index;
                loop {
                    for ch in lines[end].chars() {
                        match ch {
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth -= 1,
                            _ => {}
                        }
                    }
                    let next = lines.get(end + 1).copied().unwrap_or_default().trim();
                    if depth <= 0 && !next.starts_with('.') {
                        break;
                    }
                    end += 1;
                    assert!(
                        end < lines.len(),
                        "{name}:{}: an unbalanced label_* call, so the scan cannot end",
                        index + 1
                    );
                }
                let call = lines[index..=end].join("\n");
                assert!(
                    call.contains("text_color"),
                    "{name}:{} names no ink, so this label renders at whatever \
                     `common`'s default for its helper happens to be. §1.4 gives \
                     fg.primary to object names and titles, fg.secondary to body copy, \
                     and fg.tertiary to placeholders, counts and group heads — pick one \
                     and say so here:\n{call}",
                    index + 1
                );
            }
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
        &mut gpui_kit::VisualTestContext,
    ) {
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(DockPanel::new);
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

    /// §4.13 asks an empty state for 48px of air above it and says 不要贴顶, and the
    /// shared state can only be centred if the box it centres itself in is as tall as
    /// the list. Measured on a 1080px window before this round, the state box was
    /// **96px** — its own content — inside an 868px list, and the glyph sat 0px under
    /// the toolbar.
    ///
    /// **This test guards the precondition, not the centring**, and the difference is
    /// a shared-layer finding rather than a decision this file may make:
    ///
    /// - `justify_content: center` in this stack only takes effect when the container's
    ///   height is a *percentage*. Measured with a probe harness: a column with
    ///   `h(200px)` centres a 40px child; the same column sized by `flex_1()` puts the
    ///   child at the top. `align_items: center` did not centre vertically at all, in
    ///   either case.
    /// - `common::EmptyState`'s own wrapper (`common.rs`, not this file) is the element
    ///   that carries `justify_center`, and it is sized with `flex_1()`.
    ///
    /// So the one-word fix — `.flex_1()` → `.h_full()` on that wrapper — belongs to
    /// whoever owns `common.rs`, and it needs this precondition to already hold:
    /// `h_full()` resolves against this panel's wrapper, which is why it is `flex_1()`
    /// and not `size_full()`. Until then the state is 0px from the toolbar in this
    /// panel, in Helm and in both Settings empty states alike: **consistent, and wrong
    /// in the same way everywhere**, which is why fixing it in one place is the only
    /// version of the fix worth having.
    #[gpui_kit::test]
    fn the_empty_state_gets_the_whole_list_to_center_itself_in(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        let (_view, _dock, _state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(900.)));
        cx.run_until_parked();

        let list = cx
            .debug_bounds("forwards-empty")
            .expect("the list draws its empty state");
        let state = cx
            .debug_bounds("empty-state")
            .expect("the shared empty state");
        assert_eq!(
            state.top(),
            list.top(),
            "the state box starts at the top of the list, which is what lets it centre: \
             state {state:?}, list {list:?}"
        );
        assert_eq!(
            state.size.height,
            list.size.height,
            "§4.13's empty state needs the whole list to centre itself in. The box is \
             {}px tall in a {}px list, so `justify_center` has nothing to distribute: \
             a `h_full()` on `common::EmptyState`'s wrapper cannot resolve against a \
             parent that collapsed to the content's height. state {state:?}, list {list:?}",
            f32::from(state.size.height),
            f32::from(list.size.height),
        );
    }

    #[test]
    fn summary_text_reads_as_two_facts_and_names_no_third() {
        assert_eq!(
            summary_text(ForwardSummary {
                active: 2,
                failed: 1,
                pending: 3,
                stopped: 4,
            }),
            "2 active · 1 failed",
            "a title bar has room for the two facts a reader acts on, and the section \
             headings carry the rest"
        );
    }

    /// The filtered empty state's own way out, and it must not re-enter the panel.
    ///
    /// **This is a crash, not a style assertion.** The action was built by holding a
    /// `WeakEntity<ForwardsView>` and calling `view.update(..)` from the click, which
    /// then called `filter_input.update(..)`. `TextInput`'s `on_change` fires from
    /// inside the *input's* update and called `view.update(..)` straight back, so GPUI
    /// aborted the process with
    /// `cannot update k8s_ui::panels::forwards::ForwardsView while it is already being
    /// updated`. Reproduced on the running build, from a real click.
    ///
    /// Two things have to hold and neither is visible from the outside: the click must
    /// reach the field's entity rather than the panel, and the field's `on_change` must
    /// be deferred. So the test does what a reader does — filter to nothing, click the
    /// button the empty state offers — and then asks the panel what it thinks.
    #[gpui_kit::test]
    fn the_filtered_empty_state_clears_the_filter_without_re_entering_the_panel(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(600.)));
        let _ = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, None), cx)
                .expect("forward")
        });
        cx.run_until_parked();
        // Typed, not assigned. `set_filter` only sets the panel's copy, and
        // `TextInput::clear` returns early when the *field* is already empty — so a
        // test that skips the keystrokes would pass for the wrong reason, or fail
        // for one that has nothing to do with the click.
        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("/");
        cx.simulate_input("nothing-matches-this");
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.filter.clone()),
            "nothing-matches-this"
        );

        let clear = cx
            .debug_bounds("forwards-clear-filter")
            .expect("the filtered empty state offers a way out");
        cx.simulate_click(clear.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.filter.clone()),
            "",
            "the state's own control is the way out of the state, so it clears the \
             query it is reporting on"
        );
        let _ = state;
    }

    /// Every phase's shape comes from the one health vocabulary, and the words beside
    /// it name the phase.
    ///
    /// The panel used to keep a private map in which a failure drew a warning triangle
    /// — so the same failure looked like a warning here and like an error in the table
    /// and the status bar — and `shell/status_bar.rs` kept a second copy of it.
    /// `DESIGN.md` §4 reserves the glyphs for `design::health_icon`.
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
        // separates them. That is the answer `Connecting` and `Reconnecting` already
        // give in the status bar.
        let starting = phase_presentation(ForwardPhase::Starting);
        let stopping = phase_presentation(ForwardPhase::Stopping);
        assert_eq!(starting.0, stopping.0);
        assert_ne!(starting.1, stopping.1);
        assert_ne!(starting.3, stopping.3);
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
            "a failed forward has no listener, so the address cell must be empty"
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

    /// `UI-SPEC.md` §14.3: "failed" is not a diagnosis, so a row names the step that
    /// broke as well as the reason the runtime gave.
    #[test]
    fn a_failure_names_the_step_that_broke() {
        let cases = [
            (
                "error: unable to forward port because of an error upgrading connection",
                Some("The stream to the Pod broke"),
            ),
            ("pod not found", Some("The target no longer exists")),
            (
                "connect: connection refused",
                Some("Nothing is listening on the target port"),
            ),
            ("i/o timeout", Some("The API server did not answer")),
            ("forbidden", Some("The connection is not allowed")),
            (
                // A local port the reader asked for and did not get. The forward refuses
                // rather than moving, so the step that stopped it is named rather than the
                // row blaming the cluster.
                "Local port 8080 could not be opened: Address already in use (os error 98). Choose another local port and try again.",
                Some("The local port was already taken"),
            ),
            (
                "something else entirely",
                Some("The forward could not be established"),
            ),
        ];
        for (reason, step) in cases {
            let failed = snapshot(ForwardPhase::Failed, None, Some(reason));
            assert_eq!(failure_step(&failed), step, "for reason {reason:?}");
        }
        let running = snapshot(ForwardPhase::Running, Some(8080), None);
        assert_eq!(
            failure_step(&running),
            None,
            "a session that is not broken has no step to report"
        );
    }

    /// The running time replaces the word "active", and it counts in one scale so a
    /// reader learns it once.
    #[test]
    fn elapsed_time_reads_as_a_duration() {
        assert_eq!(elapsed_text(Duration::from_secs(0)), "0s");
        assert_eq!(elapsed_text(Duration::from_secs(45)), "45s");
        assert_eq!(elapsed_text(Duration::from_secs(5 * 60)), "5m");
        assert_eq!(elapsed_text(Duration::from_secs(2 * 3600)), "2h");
        // A reconnect countdown is a whole number of seconds, so it never shows a
        // fraction and never reads zero, which would look like a stuck forward.
        assert!(RECONNECT_DELAYS.iter().all(|delay| delay.as_secs() >= 1));
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

    /// The list is divided by what a session is *doing*, and a section with nothing in
    /// it is not drawn at all.
    #[test]
    fn the_list_is_divided_into_the_sections_it_has_members_for() {
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
                "kube-system",
                "dns",
                53,
                ForwardPhase::Failed,
                None,
                Some("connection refused"),
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
        let sections = build_sections(&forwards);
        assert_eq!(
            sections
                .iter()
                .map(|(section, members)| (*section, members.len()))
                .collect::<Vec<_>>(),
            vec![
                (Section::Active, 1),
                (Section::Failed, 1),
                (Section::Stopped, 1)
            ],
            "a session appears under what it is doing, not under which namespace it lives in"
        );
        assert!(build_sections(&[]).is_empty());
        let one = build_sections(&[snapshot_for(
            4,
            "default",
            "pod-d",
            7070,
            ForwardPhase::Running,
            Some(1),
            None,
        )]);
        assert_eq!(one.len(), 1, "one section is one heading, not three");
        assert_eq!(one[0].0, Section::Active);
    }

    /// A heading is a heading, not a target, so the rows a cursor walks are the
    /// sessions and never the section they sit under.
    #[test]
    fn a_cursor_only_ever_lands_on_a_session() {
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
                ForwardPhase::Failed,
                None,
                None,
            ),
        ];
        let ids: Vec<ForwardId> = build_sections(&forwards)
            .iter()
            .flat_map(|(_, members)| members.iter().map(|snapshot| snapshot.id))
            .collect();
        assert_eq!(
            ids,
            vec![ForwardId(1), ForwardId(2)],
            "the sections frame the sessions; they are not rows of their own"
        );
    }

    fn pod_object(json: serde_json::Value) -> DynamicObject {
        serde_json::from_value(json).expect("typed object")
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
            "no declared port means the remote field keeps its free text"
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

    /// A Service row prefills from `spec.ports`, which is a different document from a
    /// Pod's `containerPort` list and resolves `targetPort` itself.
    #[test]
    fn a_service_row_prefills_from_its_own_ports() {
        let object = pod_object(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "api-svc", "namespace": "prod" },
            "spec": {
                "ports": [
                    { "name": "http", "port": 80, "targetPort": 8080, "protocol": "TCP" },
                    { "name": "metrics", "port": 9100, "targetPort": 9100, "protocol": "TCP" },
                    { "name": "dns", "port": 53, "targetPort": 53, "protocol": "UDP" }
                ]
            }
        }));
        assert_eq!(
            service_ports(&object),
            vec![
                ContainerPort {
                    port: 8080,
                    container: Some("http".into())
                },
                ContainerPort {
                    port: 9100,
                    container: Some("metrics".into())
                },
            ],
            "a Service's own ports are what the user right-clicked, and a UDP port is not a stream"
        );
    }

    /// The collision is found before the submit, and it names the holder, because a
    /// forward that silently lands on another port sends the user to the wrong address.
    #[test]
    fn a_port_conflict_is_found_before_the_forward_is_started() {
        let ours = vec![(SharedString::from("grafana"), 3000u16)];
        assert_eq!(
            port_collision(3000, BindAddress::Loopback, &ours),
            Some(PortHolder::Forward {
                label: "grafana".into()
            }),
            "a collision with a forward this window started names that forward"
        );
        // A port the machine is holding reports as a process, not as a guess.
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a free port");
        let port = held.local_addr().expect("an address").port();
        assert_eq!(
            port_collision(port, BindAddress::Loopback, &[]),
            Some(PortHolder::Process)
        );
        drop(held);
        assert_eq!(port_collision(port, BindAddress::Loopback, &[]), None);
    }

    #[test]
    fn row_aria_reports_the_address_and_the_failure_reason() {
        let running = row_aria_label(&snapshot(ForwardPhase::Running, Some(8081), None), false);
        assert_eq!(
            running,
            "Port forward for pod-a:8080:8080. Running. http://localhost:8081."
        );

        let failed = row_aria_label(
            &snapshot(
                ForwardPhase::Failed,
                Some(8081),
                Some("Port forward ended unexpectedly."),
            ),
            false,
        );
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
            row_aria_label(&snapshot(ForwardPhase::Failed, None, Some("   ")), false),
            "Port forward for pod-a:8080:8080. Failed.",
            "a blank error adds no spoken text"
        );
        // Three attempts, then the panel stops on its own. A row that spoke exactly like one
        // that had not tried yet would leave the reader waiting for a fourth attempt.
        assert_eq!(
            row_aria_label(
                &snapshot(
                    ForwardPhase::Failed,
                    None,
                    Some("connection refused by the pod"),
                ),
                true,
            ),
            "Port forward for pod-a:8080:8080. Failed. Automatic retry has stopped; select Retry \
             to try again. connection refused by the pod"
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
            "a forward that is already stopping leaves Stop all with nothing to do"
        );
    }

    #[gpui_kit::test]
    fn keyboard_selection_keeps_forward_ids(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(DockPanel::new);
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
                dock.create_forward(request("kube-system", "dns", 53, None), cx)
                    .expect("third forward"),
            ]
        });
        cx.run_until_parked();
        // All three are starting, so all three sit under one heading and no other
        // heading is drawn at all.
        assert!(cx.debug_bounds("forwards-group-Active").is_some());
        assert!(cx.debug_bounds("forwards-group-Failed").is_none());
        let row = cx.debug_bounds("forwards-row-0").expect("a forward row");
        // The shared row rhythm, which `UI-SPEC` §4.4 sets at 32.
        assert!((f32::from(row.size.height) - 32.).abs() <= 1.);

        let focus = view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[0]));
        cx.simulate_keystrokes("down");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[1]));
        cx.simulate_keystrokes("down");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected),
            Some(ids[2]),
            "Down walks past a section heading and into the next section's first row"
        );
        cx.simulate_keystrokes("down");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected),
            Some(ids[2]),
            "the last forward has nowhere to go"
        );
        cx.simulate_keystrokes("up up up");
        assert_eq!(
            view.read_with(cx, |view, _| view.selected),
            Some(ids[0]),
            "and the first forward has nowhere to go the other way"
        );
        cx.simulate_keystrokes("end");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[2]));
        cx.simulate_keystrokes("home");
        assert_eq!(view.read_with(cx, |view, _| view.selected), Some(ids[0]));
        // Enter runs the action the selected row offers, which is how a forward is
        // stopped without reaching for the cross in its row.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Stopping
        );
    }

    #[gpui_kit::test]
    fn a_running_forward_is_one_keystroke_from_the_clipboard_and_the_browser(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        cx.dispatcher.allow_parking();
        let state = Rc::new(RefCell::new(ForwardFactoryState::default()));
        let services = test_services(Rc::clone(&state));
        let dock = cx.update(|cx| {
            let dock = cx.new(DockPanel::new);
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
            dock.update(cx, |dock, cx| {
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
            // `§14.3` draws the cell as `localhost:PORT` and the scheme belongs to the
            // href, so the cell is the address a reader would type, not the one a browser
            // wants. Two forwards on two ports, because one would not tell the click apart
            // from the chord below it: the platform records the last URL opened and nothing
            // else, so the only way to prove *this* address opened is to open a different one
            // first and watch the value change.
            let second = dock.update(cx, |dock, cx| {
                dock.create_forward(request("default", "pod-b", 9090, None), cx)
                    .expect("second forward")
            });
            state
                .borrow_mut()
                .bindings
                .pop()
                .expect("binding channel")
                .send(Ok(34_568))
                .expect("send the second bound port");
            cx.run_until_parked();
            let running = dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].clone());
            assert_eq!(
                forward_address(&running),
                Some("localhost:34567".to_owned()),
                "the cell reads as the spec and the mockup draw it"
            );

            let focus = view.read_with(cx, |view, _| view.focus_handle());
            cx.update(|window, cx| window.focus(&focus, cx));
            cx.simulate_keystrokes("ctrl-o");
            assert_eq!(
                cx.opened_url(),
                Some("http://localhost:34567".to_owned()),
                "Control O hands the address to the platform browser"
            );
            cx.simulate_keystrokes("escape");
            cx.simulate_keystrokes("down");

            // The click itself, which is the part that regresses silently. The cell used to
            // be a bare `div` with no click handler at all, so the panel had the *look* of
            // the affordance `§14.3` asks for and none of the behaviour.
            let second_address = cx
                .debug_bounds(leaked(format!("forwards-address-{second:?}")))
                .expect("the second forward's address");
            cx.simulate_click(second_address.center(), gpui_kit::Modifiers::none());
            cx.run_until_parked();
            assert_eq!(
                cx.opened_url(),
                Some("http://localhost:34568".to_owned()),
                "clicking the address is the whole point of the panel, and it is the clicked \
                 row's address and not the selected one's"
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.selected),
                Some(second),
                "and the row the reader pressed becomes the selection, as it would for any \
                 other press in that row"
            );

            // The selection is the second row now, so this is the second row's address: the
            // chord follows the selection, and the selection follows the press.
            cx.simulate_keystrokes("ctrl-c");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("http://localhost:34568".to_owned()),
                "Control C puts the whole address on the clipboard, scheme and all"
            );
            assert_eq!(
                view.read_with(cx, |view, _| view.selected),
                Some(second),
                "and the copied row is still the selection, so the next copy repeats it"
            );
            assert!(
                cx.debug_bounds("forwards-feedback").is_some(),
                "a copy with no visible result is confirmed in the toolbar"
            );
        }
    }

    #[gpui_kit::test]
    fn a_forward_with_no_listener_offers_no_address_to_copy(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        let (_view, dock, _state, cx) = forwards_harness(cx);
        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, None), cx)
                .expect("forward")
        });
        // A stopped forward keeps no listener, so it has no address to hand over.
        dock.update(cx, |dock, cx| dock.stop_forward(id, cx));
        cx.run_until_parked();
        let address = cx
            .debug_bounds(leaked(format!("forwards-address-{id:?}")))
            .expect(
                "the address cell stays in place, so the row does not change shape between phases",
            );
        assert!(f32::from(address.size.width) > 0.);

        let focus = _view.read_with(cx, |view, _| view.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("ctrl-c");
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            None,
            "a forward with no listener puts nothing on the clipboard"
        );
    }

    #[gpui_kit::test]
    fn the_filter_narrows_the_list_and_reports_how_much_is_left(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        cx.dispatcher.allow_parking();
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(600.)));
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
        assert!(cx.debug_bounds("forwards-row-0").is_some());
        assert!(cx.debug_bounds("forwards-row-1").is_some());
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
            cx.debug_bounds("forwards-row-0").is_some(),
            "the matching forward keeps the first row under its heading"
        );
        assert!(
            cx.debug_bounds("forwards-row-1").is_none(),
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

    /// A forward answers on the port it was asked for, and a retry asks for that port again.
    ///
    /// A session that moved address would break whatever was pointed at the address the reader
    /// typed, so a taken port is refused instead. The row therefore never has two ports to
    /// reconcile, and this covers the one it can report.
    #[gpui_kit::test]
    fn a_forward_answers_on_the_asked_for_port_and_a_retry_asks_for_it_again(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        cx.dispatcher.allow_parking();
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(600.)));
        let id = dock.update(cx, |dock, cx| {
            dock.create_forward(request("default", "pod-a", 8080, Some(8081)), cx)
                .expect("forward")
        });
        assert_eq!(
            state.borrow().requests[0].local_port,
            Some(8081),
            "the port the user asked for reaches the forward"
        );
        // The forward bound the port the request named.
        state
            .borrow_mut()
            .bindings
            .pop()
            .expect("binding channel")
            .send(Ok(8081))
            .expect("send the bound port");
        cx.run_until_parked();
        let snapshot = dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].clone());
        assert_eq!(snapshot.local_port, Some(8081));
        let label = row_aria_label(&snapshot, false);
        assert!(
            label.contains("http://localhost:8081"),
            "the address is the one that was asked for: {label}"
        );
        assert!(
            !label.contains("already in use"),
            "a forward on the port the reader typed has nothing to reconcile: {label}"
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

    #[gpui_kit::test]
    fn stop_all_only_appears_while_a_forward_can_still_be_stopped(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        let (_view, dock, _state, cx) = forwards_harness(cx);
        assert!(
            cx.debug_bounds("forwards-stop-all").is_none(),
            "an empty list has nothing for Stop all to stop"
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
            "a forward that is already stopping leaves Stop all with nothing to do"
        );
    }

    #[gpui_kit::test]
    fn a_failed_forward_drops_its_port_and_explains_itself(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        cx.dispatcher.allow_parking();
        let (_view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(600.)));
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
        let label = row_aria_label(&snapshot, false);
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
            "Stop all disappears once nothing can be stopped"
        );
        // The failed section is the one heading in the list that is coloured, and the
        // row's second line names the step that broke.
        assert!(cx.debug_bounds("forwards-group-Failed").is_some());
        assert!(
            cx.debug_bounds(leaked(format!("forwards-reason-{id:?}")))
                .is_some(),
            "a failure says which step broke, in the row"
        );
        // The name carries the id the dock issued, so the assertion follows the forward
        // under test instead of a constant the counter never produces.
        let retry = cx
            .debug_bounds(leaked(format!("{}-{:?}", RowAction::Retry(id).name(), id)))
            .expect("retry");
        assert!(f32::from(retry.size.width) > 0.);
        // The control is live, not only laid out: a click runs the retry.
        cx.simulate_click(retry.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Starting,
            "clicking Retry starts a new attempt"
        );
    }

    /// `UI-SPEC.md` §14.5: a broken session reconnects on its own, and the row says it
    /// is waiting. A silent retry would be a lie and an endless one a loop, so the
    /// attempt is bounded and the countdown is on screen.
    #[gpui_kit::test]
    fn a_broken_forward_reconnects_and_says_it_is_waiting(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(crate::settings::init);
        cx.dispatcher.allow_parking();
        let (view, dock, state, cx) = forwards_harness(cx);
        cx.simulate_resize(gpui_kit::size(px(1200.), px(600.)));
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
        state
            .borrow_mut()
            .errors
            .pop()
            .expect("error channel")
            .send("connection refused by the pod".to_owned())
            .expect("send the failure");
        cx.run_until_parked();
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Failed
        );
        assert!(
            view.read_with(cx, |view, _| !view.reconnects.is_empty()),
            "the panel waits out a gap before it tries again"
        );
        assert!(
            view.read_with(cx, |view, _| view
                .reconnect_remaining(id, Instant::now())
                .is_some()),
            "the row has a countdown to show, so the retry is never silent"
        );
        // The attempt runs one gap later, and it asks the dock for a new attempt rather
        // than starting a forward of its own.
        cx.dispatcher
            .advance_clock(RECONNECT_DELAYS[0] + Duration::from_millis(10));
        cx.run_until_parked();
        assert_eq!(
            state.borrow().requests.len(),
            2,
            "the panel retried the broken session by itself"
        );
        assert_eq!(
            dock.read_with(cx, |dock, _| dock.forward_snapshots()[0].phase),
            ForwardPhase::Starting
        );
        assert!(
            view.read_with(cx, |view, _| view.reconnects.is_empty()),
            "a session that is trying again is not waiting to try again"
        );
    }
}
