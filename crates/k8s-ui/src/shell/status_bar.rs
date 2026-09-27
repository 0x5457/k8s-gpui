//! Status bar and notification center.
//!
//! The bar shows nearby status and activity counts. The notification center
//! keeps detailed failures expandable and keeps success and information in history.

use std::cmp::Ordering;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::{ActiveTheme, Icon, RoleOverride, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, FocusHandle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Pixels, Role, SharedString,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, div, px,
};

use super::{ConnectionState, Notification, Shell};
use crate::design::{self, Severity, space};
use crate::panels::ForwardSummary;
use crate::panels::common;

/// Root tab group of the status bar, after the Dock.
const STATUS_BAR_TAB_GROUP: isize = 8;
/// Root tab group of the notification center, after the status bar.
const NOTIFICATION_TAB_GROUP: isize = 9;
const NOTIFICATION_WIDTH: f32 = 360.0;
const NOTIFICATION_MAX_HEIGHT: f32 = 360.0;
/// Clearance between the popovers and the top of the status bar. A floating surface must not
/// read as attached to the bar that owns it.
const POPOVER_CLEARANCE: f32 = 16.0;
/// Width of the relative-time column. It holds the longest age label the bar can print
/// (`23 hours`), so every timestamp starts at the same x and never touches the message.
const NOTIFICATION_AGE_COLUMN: f32 = 56.0;

/// The bar's content inset, and the shell's.
///
/// The title bar, the sidebar, the Dock and this bar are one chrome band, so the
/// text inside them starts on the same line and the bar's right edge *is* the
/// window's right edge. It is the same number the notification popover anchors to,
/// which is what stops a popover that belongs to this bar from hanging one pixel
/// in from it.
const STATUS_BAR_INSET: Pixels = space::SM;

/// The one gap between the bar's items.
///
/// `space::MD`, and it is the only separator the items have. The strip used to
/// interleave a middot between every pair, which is what turned three independent
/// readouts and one control into a breadcrumb: a path says *you have arrived
/// somewhere*, and a status bar is a row of peers.
///
/// Spacing is the whole grouping device here, so it has to be one number. `SM`
/// read as a hairline between the states, and it put the forwards control at the
/// same distance from the connection state as from its own count.
const STATUS_BAR_GAP: Pixels = space::MD;

/// The reserved width of the trailing count lane.
///
/// The bar is a permanent strip, so nothing on it may move when a number changes
/// (`UI-SPEC` §4.17 and §8's 克制 group ask for the same thing twice). A count that
/// arrives or leaves therefore cannot resize the item that carries it, which means
/// the lane has to be there before the count is.
///
/// 72px is the width of the widest count the bar can print at `text::CAPTION`:
/// four digits, one thousands separator and the longest phase word, which is
/// `1,024 active`. The count is bounded by construction rather than by this
/// number — a port forward is a bound TCP port, so a cluster cannot reach five
/// digits — and 72 is the next 4pt step up from that width, so the lane is on the
/// same grid as the gaps around it.
///
/// Right-aligned, so `2` and `1,024` end on the same pixel and a reader scanning
/// the bar compares digits rather than words.
const FORWARD_COUNT_LANE: f32 = 72.0;

/// The noun the bar's leading item is *about*, and the same word in its accessible name.
///
/// The strip used to print `Connection · Live`, and the middot was doing two jobs
/// at once: it separated the noun from its state, and — because every neighbouring
/// item was joined the same way — it drew a breadcrumb. `Connection · Live · 3
/// active · Port forwards` is a path, and a path says "you have arrived
/// somewhere"; the bar is a row of independent readouts and a control, none of
/// which is on the way to another.
///
/// The noun is *announced* and not drawn. On the strip it is a label for the
/// window rather than for the state, and it duplicates what the 6px mark already
/// says: a reader looking at `● Live` has been told the state, and `Connection` in
/// front of it only tells them which window they are in. A screen reader arrives
/// with no mark at all, so the name it gets keeps the noun.
const CONNECTION_LABEL: &str = "Connection";

/// Width of the notification popover.
///
/// A narrow window still has to show a readable popover, so the width keeps a
/// control-width floor instead of clamping down to nothing.
fn notification_width(viewport_width: f32) -> f32 {
    (viewport_width - 2.0 * f32::from(space::XXL))
        .min(NOTIFICATION_WIDTH)
        .max(f32::from(design::size::CONTROL))
}

fn notification_capacity() -> usize {
    // The rows are the status bar token's height, so the list and the bar that owns it cannot
    // drift apart.
    let row = f32::from(design::size::STATUS_BAR);
    ((NOTIFICATION_MAX_HEIGHT - row) / row).floor().max(0.0) as usize
}

fn notification_overflow_count(count: usize) -> usize {
    count.saturating_sub(notification_capacity())
}

fn notification_is_active_incident(notification: &Notification) -> bool {
    notification.detail.is_some()
        || matches!(notification.severity, Severity::Error | Severity::Warning)
}

/// Loudness order for the notification list.
///
/// This is an ordering and not a vocabulary: the arms pick a number, and no glyph, word or
/// colour is chosen here. `DESIGN.md` §4 reserves the shape and label vocabularies for
/// `design::health_icon` and `design::health_label`, and this match touches neither. The list it
/// sorts is a mix of problems and confirmations, and the reader scanning it needs the problem that
/// is still active above the confirmation that already happened.
///
/// `Severity` deliberately has no ordering on it — `Info` sits after `Error` and `Neutral` and
/// `Muted` are quietness roles rather than steps on a scale — so the order has to be written out
/// here, and every arm is spelled so that adding a variant is a compile error rather than a
/// silent tie.
fn notification_severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
        Severity::Success => 3,
        Severity::Neutral => 4,
        Severity::Muted => 5,
    }
}

fn notification_cmp(left: &Notification, right: &Notification) -> Ordering {
    notification_is_active_incident(right)
        .cmp(&notification_is_active_incident(left))
        .then_with(|| {
            notification_severity_rank(left.severity)
                .cmp(&notification_severity_rank(right.severity))
        })
        .then_with(|| right.at.cmp(&left.at))
        .then_with(|| left.message.as_ref().cmp(right.message.as_ref()))
        .then_with(|| left.id.cmp(&right.id))
}

fn ordered_notifications(notifications: &[Notification]) -> Vec<&Notification> {
    let mut ordered: Vec<_> = notifications.iter().collect();
    ordered.sort_by(|left, right| notification_cmp(left, right));
    ordered
}

/// The rows the notification center shows, plus how many repeats were folded into them.
///
/// The history keeps every entry, because a retry can raise the same failure twice. The list
/// does not: one problem shown twice is one problem with two timestamps to read past.
fn collapsed_notifications(notifications: &[Notification]) -> (Vec<&Notification>, usize) {
    let mut seen: Vec<(Severity, &str)> = Vec::new();
    let mut rows: Vec<&Notification> = Vec::new();
    let mut collapsed = 0;
    // Ordering puts the newest entry of a severity first, so the first one seen is the one kept.
    for notification in ordered_notifications(notifications) {
        let key = (notification.severity, notification.message.as_ref());
        if seen.contains(&key) {
            collapsed += 1;
            continue;
        }
        seen.push(key);
        rows.push(notification);
    }
    (rows, collapsed)
}

/// The header count. One number: a second "active" total repeated the same figure next to it.
fn notification_count_label(count: usize) -> String {
    design::format::count_with_noun(count, "notification", "notifications")
}

/// The disclosure under the list. It has to name what the list did with the history, because a
/// count the user can see no longer matches the count the status bar reported.
fn notification_status_label(collapsed: usize, overflow: usize) -> String {
    let mut parts = Vec::new();
    if collapsed > 0 {
        parts.push(format!(
            "{} duplicate {} collapsed",
            design::format::count(collapsed),
            if collapsed == 1 { "entry" } else { "entries" }
        ));
    }
    if overflow > 0 {
        parts.push(format!(
            "{} more {} · scroll to view",
            design::format::count(overflow),
            if overflow == 1 {
                "notification"
            } else {
                "notifications"
            }
        ));
    }
    parts.join(" · ")
}

/// The full breakdown. The bar's tooltip, the popover header's tooltip, and the list's own label.
fn forward_summary_label(summary: ForwardSummary) -> String {
    format!(
        "{} active, {} failed, {} pending, {} stopped",
        design::format::count(summary.active),
        design::format::count(summary.failed),
        design::format::count(summary.pending),
        design::format::count(summary.stopped)
    )
}

/// The noun the forwards item is about, and the label its control wears.
///
/// `UI-SPEC` §14.6 wants the forwards count in the status bar *as a link* to the list, and the
/// link is a control rather than a report — the same way the top bar's bell is a control that
/// stands on the strip with nothing to say (`panels.rs`). A control on a permanent strip is named;
/// a report on one carries a number. `Port forwards · 0 active` was the one number in the window
/// that could never change, and §8's 克制 group asks the interface not to spend attention on what
/// is not happening.
///
/// It is a noun because it is a destination, and it is the only word drawn: the count rides a
/// reserved lane at the item's trailing edge ([`FORWARD_COUNT_LANE`]) and the item's own name
/// carries the rest.
const FORWARD_LINK_NAME: &str = "Port forwards";

/// The count the trailing lane carries, and nothing else.
///
/// One number and one word, right-aligned in a lane that is already reserved, so a count arriving
/// or leaving moves nothing on the strip. The *phase* is chosen rather than listed, because the
/// lane has room for one bucket and the reader has room for one decision:
///
/// - a failure wins, then a start in flight, then the running count. `0 active · 3 failed` is the
///   case a zero would otherwise hide, so it is the case the lane is for.
/// - a problem bucket is the one that changes what the reader does next, and the running count it
///   displaces is still in the item's accessible name and in its hover.
///
/// With nothing running, nothing failed and nothing starting, the lane is empty and the item is
/// just its name. The item itself stays — §14.6's link is how the list is opened — and an item
/// that is on the strip because it is a control does not owe the reader a count of nothing.
fn forward_count_lane_label(summary: ForwardSummary) -> Option<String> {
    let (count, word) = if summary.failed > 0 {
        (summary.failed, "failed")
    } else if summary.pending > 0 {
        (summary.pending, "pending")
    } else if summary.active > 0 {
        (summary.active, "active")
    } else {
        return None;
    };
    Some(format!("{} {word}", design::format::count(count)))
}

/// The item's accessible name: the subject, then every count it stands for.
///
/// It names the subject before the count, so it has the same shape as the three
/// metrics beside it instead of a fourth grammar of its own, and it keeps the
/// problem buckets that the single visible lane has to choose between — a name
/// that dropped `3 failed` because the lane showed it would be a quieter name
/// than the thing it names.
///
/// With nothing running, nothing failed and nothing starting, it prints the
/// control's name and no number, which is the same answer the lane gives.
///
/// The middot *inside* this name is the noun's own qualifier and stays: one
/// item's name is a name. What made the bar read as a breadcrumb was the same
/// mark between the *items*, and that one is gone (`render_status_bar`).
fn forward_summary_short_label(summary: ForwardSummary) -> String {
    if summary.active == 0 && summary.failed == 0 && summary.pending == 0 {
        return FORWARD_LINK_NAME.to_owned();
    }
    let mut label = format!(
        "{FORWARD_LINK_NAME} · {} active",
        design::format::count(summary.active)
    );
    if summary.failed > 0 {
        label.push_str(&format!(
            " · {} failed",
            design::format::count(summary.failed)
        ));
    } else if summary.pending > 0 {
        label.push_str(&format!(
            " · {} pending",
            design::format::count(summary.pending)
        ));
    }
    label
}

/// Shape and severity for the port forward summary the bar prints.
///
/// The shape comes from the shared health vocabulary, and the nothing-to-report state borrows the
/// metrics' muted ink. It used to draw `ArrowRightLeft` for both "some are running" and "none are",
/// so the shape carried nothing and only the colour told the two apart, which `color.md >
/// Inclusive color` forbids.
fn forward_summary_presentation(summary: ForwardSummary) -> (IconName, Severity) {
    let severity = if summary.failed > 0 {
        Severity::Error
    } else if summary.pending > 0 {
        Severity::Warning
    } else if summary.active > 0 {
        Severity::Success
    } else {
        Severity::Muted
    };
    (design::health_icon(severity), severity)
}

/// Severity for the operations count.
///
/// The count is pending writes on the view in the centre tab, so a zero means nothing is in
/// flight. It used to wear a *static* loading spinner, which claimed work that does not exist and
/// borrowed the same glyph the log stream used to mean `Connecting`. The shape now comes from the
/// shared health vocabulary, and only a non-zero count is marked at all.
fn operations_severity(count: usize) -> Severity {
    if count > 0 {
        Severity::Info
    } else {
        Severity::Muted
    }
}

/// Shape and severity for a log stream state.
///
/// The Dock renders its own chip with this information, but the Dock is not rendered at all while
/// it is collapsed, so a paused stream would leave no trace on screen. The shape is the shared
/// health vocabulary and the words beside it always name the state, so `color.md` never has colour
/// or a shape carrying the state on its own. `DESIGN.md` §4 reserves the glyph vocabulary for
/// `design::health_icon`, and this file used to keep a second one that collided with the
/// operations metric's static spinner.
fn log_status_presentation(label: &str) -> (IconName, Severity) {
    let severity = match label {
        "Live" => Severity::Success,
        "Paused" => Severity::Muted,
        "Connecting" => Severity::Warning,
        "Reconnecting" => Severity::Warning,
        "Failed" => Severity::Error,
        // "Unavailable", and any state a later version adds, still prints its own name.
        _ => Severity::Muted,
    };
    (design::health_icon(severity), severity)
}

/// What the bar prints next to the log icon. It names the surface, because a bare "Paused" does
/// not say whether logs, a session, or a download are paused.
fn log_status_text(label: &str) -> String {
    format!("Logs · {label}")
}

/// What the notification list is announced as.
///
/// The separator is not decoration: this is the one list in the app that can hold four digits,
/// and a `Notifications: 1200 shown` next to a `1,200` in the same window is two formats for one
/// fact. `DESIGN.md` §9 claims the count format is one format.
fn notification_list_announcement(count: usize) -> String {
    format!("Notifications: {} shown", design::format::count(count))
}

/// How well the app can currently vouch for what the bar reports.
///
/// Health answers "how is the cluster doing"; this answers "did the app get an answer at all".
/// A cluster the app could not read is not a healthy one, and merging the two is what makes a
/// status display lie exactly during an incident, so they stay two channels with two shapes.
fn observation_confidence(
    connection: &ConnectionState,
    catalog_stale: Option<bool>,
) -> design::Confidence {
    match connection {
        // Still asking, or no usable connection: there is no verdict to report yet.
        ConnectionState::Connecting | ConnectionState::Failed(_) => design::Confidence::Unknown,
        // A retry is scheduled, so the last answer is older than the refresh interval.
        ConnectionState::Reconnecting(_) => design::Confidence::Stale,
        ConnectionState::Live => match catalog_stale {
            // The resource list is a standing cache while it refreshes in the background.
            Some(true) => design::Confidence::Stale,
            _ => design::Confidence::Known,
        },
    }
}

/// The confidence marker's shape, or nothing at all when the app has a current answer.
///
/// A known answer draws no mark: the health glyph beside it already speaks, and a second glyph
/// on every healthy row would imply a second thing to read.
fn confidence_marker(state: design::Confidence) -> Option<IconName> {
    (state != design::Confidence::Known).then(|| design::confidence::icon(state))
}

/// The resting edge of a floating surface.
///
/// `colors.border` is a control boundary, solved for a control sitting on a panel. A popover sits
/// on `raised`, and there the same token measured 1.37:1 in dark and 2.67:1 in light, which is less
/// than a boundary has to clear: `design::border::INTERACTIVE_MIN_CONTRAST` is the threshold meant
/// for a floating surface's edge, because a popover has no other thing that says "this floats".
/// Resolving it once per surface is also what stops the focus ring from being the permanent border,
/// which is what the previous `focus_visible(border_color)` arrangement amounted to. This is the
/// same solve the table's column-resize affordance uses, on the surface it is painted on.
fn popover_border(raised: Hsla, preferred: Hsla) -> Hsla {
    design::graphic_on_with_minimum(raised, preferred, design::border::INTERACTIVE_MIN_CONTRAST)
}

/// A structural rule inside a floating surface.
///
/// `border.variant` is solved against the table, and the popover paints on `raised`, where it lands
/// under the rule floor. `DESIGN.md` §3.5 wants structural rules quiet, and quiet is not the same as
/// absent, so the same floor the rest of the app enforces is applied here.
fn popover_rule(raised: Hsla, preferred: Hsla) -> Hsla {
    design::graphic_on_with_minimum(raised, preferred, design::border::MIN_RULE_CONTRAST)
}

/// The bar's own top edge, against the plane it is painted on.
///
/// `colors.border` is solved for a control sitting on a panel, and the bar paints
/// on `role::surface_chrome`. On the shipped themes the raw token happens to land
/// above the floor here — 1.28:1 in dark and 1.24:1 in light, because the token's
/// own alpha is composited over the plane it is measured against — so this solve
/// is a no-op today and is here for the two reasons a floor needs an owner:
///
/// - a token that clears the floor on the plane it was *measured* on is a
///   coincidence, and a theme that moved the chrome plane is one edit away from a
///   boundary a reader cannot find. Every structural rule in the product is
///   solved against the surface it lands on; this one was the exception.
/// - the bar, the rail and the popovers then answer the same question with the same
///   helper and the same floor, so "how quiet is a structural rule" has one
///   number in the window instead of one per call site.
///
/// The bar is the only owner: the Dock above it draws no bottom edge and the
/// centre column's chrome does not reach this far.
fn bar_rule(plane: Hsla, preferred: Hsla) -> Hsla {
    design::graphic_on_with_minimum(plane, preferred, design::border::MIN_RULE_CONTRAST)
}
/// The ring a status panel paints on whichever control the keyboard is on.
///
/// A status panel is app state, so its controls are app elements wrapped around
/// gpui-kit components rather than components that paint their own ring, and the
/// rail is the same accent the popover's own edge uses, so a focused control
/// still reads as belonging to the surface it sits on.
fn panel_focus_ring(color: Hsla) -> impl Fn(StyleRefinement) -> StyleRefinement {
    move |style| style.border(design::border::FOCUS_RAIL).border_color(color)
}

/// A named control of a status panel that the shell's own Enter key acts on.
///
/// The shell's keystroke interceptor owns Enter and Space while a status panel
/// is open — a panel is modal, and a keystroke that reached the surface behind
/// it would act on something the reader cannot see — so a gpui-kit button's own
/// key handling never runs while the panel is on screen, and the panel has to
/// dispatch from the focused handle instead. That makes the focus handle the
/// control, and this carrier is where it lives: the wrapper is focusable,
/// named, and ringed, and the button inside it is the appearance and the
/// pointer target. The button is presentational, so the accessibility tree
/// still names exactly one control for the action, and a pointer click lands
/// on the button, so the two paths can never disagree about what was pressed.
fn panel_action(
    id: &'static str,
    focus: &FocusHandle,
    label: impl Into<SharedString>,
    color: Hsla,
    button: Button,
) -> AnyElement {
    div()
        .id(id)
        .track_focus(focus)
        .role(Role::Button)
        .aria_label(label)
        .rounded_md()
        .focus_visible(panel_focus_ring(color))
        .child(button.role(RoleOverride::Presentational).tab_stop(false))
        .into_any_element()
}

/// Vertical padding for the popover's empty list.
///
/// `space::LG` made an 84px shell around one line of 11px text — 50px of air holding a single
/// sentence. `popovers.md` asks for a popover that is only as big as its contents, and §2 would
/// rather delete the padding than the content.
const NOTIFICATION_EMPTY_PADDING: Pixels = space::SM;

/// Whether the popover's header draws its rule.
///
/// An empty list puts `No notifications` directly under the title, so a rule there separates
/// nothing, and §2 asks for meaningless dividers to be deleted rather than drawn faintly.
fn notification_header_rule(count: usize) -> bool {
    count > 0
}

/// The word a notification row's severity gets in its accessible label.
///
/// The row used to keep a second severity-to-word map that disagreed with `design::health_label`
/// in the same file: `Error` against `Failed`, `Warning` against `Needs attention`, `Info` against
/// `Syncing`. `DESIGN.md` §4 makes the shared vocabulary the app's only one, and a row that says
/// `Error` here and `Failed` three lines up is two vocabularies for one thing.
fn notification_severity_word(severity: Severity) -> &'static str {
    design::health_label(severity)
}

/// Whether row `index` of `total` draws its rule.
///
/// A rule separates two rows. The last one has nothing after it to separate from, and the popover
/// clips its children, so that rule landed exactly on the panel's own bottom edge and read as a
/// second border.
fn notification_row_rule(index: usize, total: usize) -> bool {
    index + 1 < total
}

/// The rail a focused popover draws inside its own edge.
///
/// `DESIGN.md` §3.5 asks for a focus boundary that cannot be read as an ordinary edge, and the
/// popover used to spend one property on both jobs: it focused itself on open, so the resting
/// appearance was a 1px accent ring and the edge underneath it was the hairline above. The edge is
/// solved on its own now and the rail is a separate element, so one property is not doing two
/// jobs. The rail is positioned inside the surface, which is also why a keyboard focus ring still
/// costs the content no width — a border that only appears on focus would shift the text sideways
/// under Taffy's border box.
fn popover_focus_rail(focus: &FocusHandle, color: Hsla) -> AnyElement {
    div()
        .absolute()
        .inset(design::border::FOCUS_RAIL)
        .rounded_md()
        .track_focus(focus)
        .focus_visible(|style| style.border(design::border::FOCUS_RAIL).border_color(color))
        .into_any_element()
}

/// The bar's leading item's *announced* text, and its hover.
///
/// It reports the connection and nothing else. `ConnectionState`'s only input is a single
/// `GET /readyz` against kube-apiserver, so the health word it used to print here (`Healthy`) rested
/// on nothing but "the app could still reach the API server". `DESIGN.md` §4 is blunt about it —
/// **读不到 ≠ 健康** — and a one-shot probe can never turn amber because a workload broke, so the
/// word was guaranteed to be reassuring at the exact moment it should not have been. Cluster health
/// is the Overview banner's verdict (`k8s_core::overview::level`), and the top bar already
/// prints this same state as `Live`.
///
/// This is the name, not the strip. The drawn item is a `size::STATUS_DOT` mark and the state word
/// (see [`status_mark_dot`]) because the mark is the channel a reader without a screen reader gets,
/// and a noun in front of it duplicated what the mark already said. A middot would be worse: in a
/// 24px row of 11px type it is punctuation whether or not a screen reader ever says it out loud,
/// and a name is allowed the sentence the strip is not.
fn connection_text(connection: &ConnectionState) -> String {
    format!("{CONNECTION_LABEL} · {}", connection.label())
}

/// A count the bar reports, with the sentence that says what it counts.
///
/// The bar's readouts are one Tab stop, so a reader who lands on the bar has to be able to
/// answer "what is this?" from the element itself. The count alone is not that answer: `0
/// operations` is the pending writes on the view in the centre tab, and it reads 0 whenever no
/// resource view is open. So the sentence is announced as well as hovered, and a screen reader
/// user gets the same scope a pointer user does.
fn metric_announcement(label: &str, scope: &str) -> String {
    format!("{label}. {scope}")
}

/// Why the operations count reads what it reads.
///
/// The count is the pending writes on the view in the centre tab. With no resource view open
/// there is nothing it could be counting, and saying "0 operations" there reads as a quiet
/// cluster rather than as a bar with nothing to report.
fn operations_scope(has_resource_view: bool) -> &'static str {
    if has_resource_view {
        "Pending operations on the view in the centre tab"
    } else {
        "No resource view is open, so nothing can be pending"
    }
}

/// The bar's one count readout.
///
/// The count goes through the shared formatter, so the separator and the plural are the app's
/// rather than this file's, and the scope sentence rides along in the accessible name. The item
/// is pinned to the bar's own height so a count arriving or leaving cannot make the row jump:
/// `UI-SPEC` §4.17 asks for 24px and §8's 克制 group asks twice over that a permanent strip
/// should not change shape to say something that did not change.
///
/// `cx` is here so the item states its own ink instead of inheriting the bar's. A readout that
/// takes its colour from whatever it happens to be sitting on is one theme move away from a
/// different value, and a number is a number on every surface.
fn status_metric(
    id: &'static str,
    icon: Icon,
    count: usize,
    singular: &'static str,
    plural: &'static str,
    tooltip: String,
    cx: &App,
) -> AnyElement {
    let label = design::format::count_with_noun(count, singular, plural);
    let ink = design::role::fg_secondary(cx);
    let mut metric = h_flex()
        .id(id)
        // A selector so a test can prove an item with nothing to report is not on the bar.
        .debug_selector(|| id.to_owned())
        .h(px(super::status_bar_item_height()))
        .flex_none()
        .gap(space::XS)
        .items_center()
        .role(Role::Status)
        .aria_label(metric_announcement(&label, &tooltip))
        .child(icon.text_color(ink))
        .child(
            Label::new(label)
                .text_size(design::text::CAPTION)
                .text_color(ink),
        );
    metric.interactivity().tooltip(common::hover_hint(tooltip));
    metric.into_any_element()
}

/// The 6px mark a state cell wears.
///
/// §4.4's status cell in one element: a dot, not a glyph and not a middot. It is always drawn, so
/// `Live` reads as a state the bar is reporting rather than as the last crumb of a path — the bar's
/// leading item used to wear no shape at all when the cluster was reachable, and a readout whose
/// shape appears only when something is wrong is a readout whose shape has to be read out loud.
///
/// `Muted` is not "no channel": `role::status_for(Severity::Muted)` is `fg_tertiary`, so a healthy
/// connection is a quiet dot beside a quiet word rather than a green one, which is D5's whole
/// point.
fn status_mark_dot(cx: &App, severity: Severity) -> AnyElement {
    div()
        .flex_none()
        .size(design::size::STATUS_DOT)
        .rounded_full()
        .bg(design::role::status_for(severity, cx))
        .into_any_element()
}

/// A state that has no count: the mark carries the shape, the label carries the words, and the
/// colour only reinforces them.
fn log_status_metric(
    id: &'static str,
    icon: Icon,
    label: &'static str,
    tooltip: String,
    cx: &App,
) -> AnyElement {
    let text = log_status_text(label);
    let ink = design::role::fg_secondary(cx);
    let mut metric = h_flex()
        .id(id)
        // A selector so a test can prove the state is on screen while the Dock is collapsed.
        .debug_selector(|| id.to_owned())
        .h(px(super::status_bar_item_height()))
        .flex_none()
        .gap(space::XS)
        .items_center()
        .role(Role::Status)
        .aria_label(metric_announcement(&text, &tooltip))
        .child(icon)
        .child(
            Label::new(text)
                .text_size(design::text::CAPTION)
                .text_color(ink),
        );
    metric.interactivity().tooltip(common::hover_hint(tooltip));
    metric.into_any_element()
}

impl Shell {
    /// The bar's leading pair: whether the app can reach the API server on one channel, and whether
    /// that answer can be vouched for on the other.
    ///
    /// The strip stays quiet — `METADATA`, no chip, no badge — because this is a readout, not a
    /// dashboard. The mark and the word come from the status vocabulary, but the *words* are the
    /// connection's own. Printing `Cluster · Healthy` here put an observation fact in the health
    /// channel and then added a confidence marker beside it, saying the same thing twice with the
    /// first one in the wrong place: the Overview banner could show `Cluster needs attention` in the
    /// same frame this read `Healthy`, and a green tick plus a health word is a promise no readiness
    /// probe can keep.
    fn render_cluster_health(&self, cx: &Context<Self>) -> AnyElement {
        let severity = self.connection.severity();
        let confidence = observation_confidence(&self.connection, self.catalog_stale);
        let marker = confidence_marker(confidence);
        let label = connection_text(&self.connection);
        // The confidence word only reaches a pointer or a screen reader here; the shape carries it
        // for everyone else, which is why the marker is not a color change.
        let description = design::confidence_label(confidence);
        let word = self.connection.label();
        // A *state*, and drawn as one: the 6px mark in `role::status_for`'s channel, then the state
        // word in `role::status_word_for`'s, and nothing else.
        //
        // The noun is gone from the strip and kept in the name (`CONNECTION_LABEL`), because it was a
        // label for the window rather than for the state and it duplicated what the mark already
        // says. A reader looking at `● Live` has been told the state; a screen reader arrives with no
        // mark at all, so `connection_text` — which is this item's accessible name and its hover —
        // keeps `Connection · Live` and is the one place the pair is spelled out.
        //
        // The two roles are two different inks, which is the whole reason the design keeps them: under
        // Increase Contrast they are held to two different floors, and they coincide only while the
        // state is quiet — which is the inversion the design asks for, since a healthy cluster is the
        // absence of a status channel.
        let mut item = h_flex()
            .id("status-bar-cluster-health")
            .debug_selector(|| "status-bar-cluster-health".to_owned())
            .h(px(super::status_bar_item_height()))
            .flex_none()
            .gap(space::XS)
            .items_center()
            .role(Role::Status)
            .aria_label(format!("{label}. {description}"))
            .child(status_mark_dot(cx, severity))
            .child(
                Label::new(word)
                    .text_size(design::text::CAPTION)
                    .text_color(design::role::status_word_for(severity, cx)),
            )
            // The confidence mark is a second question on the same cell — "could the app get an
            // answer at all" — so it stays on the item's trailing edge and stays in the hollow
            // shape family. Two marks in one cell is deliberate: they answer different questions and
            // a reader who cannot separate the hues still sees two different shapes. It draws
            // nothing at all when the answer is current, so it is never a decoration.
            .when_some(marker, |this, marker| {
                this.child(
                    Icon::new(marker)
                        .xsmall()
                        .text_color(design::confidence::foreground(confidence, cx)),
                )
            });
        item.interactivity()
            .tooltip(common::hover_hint(format!("{label}. {description}")));
        item.into_any_element()
    }

    /// Renders the bar.
    ///
    /// gpui-kit's `StatusBar` splits into pinned `left` and `right` regions, and this bar is
    /// one left-to-right reading order behind a single focus handle, so the strip is laid out
    /// here and the items in it are built here too, out of one set of parts.
    ///
    /// # The bar is three kinds of thing, and the order says so
    ///
    /// Reading order runs along the bar, so the strip is a **state**, then the
    /// **counts**, then one **destination** carrying its own count, and a second
    /// **state** that the Dock cannot show while it is collapsed. A state has no
    /// count printed in front of it, a count has no noun in front of it, and a
    /// destination is a control with a hover and a focus ring. Drawn that way each
    /// kind is recognisable without reading it, which is what a bar of eleven-pixel
    /// type has to be.
    ///
    /// An item with nothing to report is not drawn. This strip is permanent, so `0 operations ·
    /// 0 sessions · 0 notifications` was a permanent report of three things that were not
    /// happening, spent on the most valuable real estate in the window; the top bar took the same
    /// decision for its error and notification counts (`panels.rs`), and the notification item
    /// here now reads that same `active_notifications` figure the bell does, so one number cannot
    /// be hidden at zero in one place and printed in the other. What remains is the connection,
    /// whatever has something to say, and the one control.
    ///
    /// # The compact band
    ///
    /// Below [`super::chrome_compact_width`] — one number, the window's own floor — the three
    /// *optional counts* go and everything the bar exists for stays: the connection state, the log
    /// state (a state, not a count, and the Dock hides its own chip while it is collapsed), and the
    /// destination with its lane. Each shed item keeps another path, and no path is a hover:
    ///
    /// - **operations** — the pending writes on the view in the centre tab. Its only other home is
    ///   that view: the count is a property of the open tab, so opening the tab is the path, and
    ///   there is no palette command for it. Stated rather than papered over — a second invented
    ///   command for a number the reader can see on the thing it counts would be a worse answer.
    /// - **sessions** — the Dock's own chips; `dock.toggle` opens it, and the terminal is on it.
    /// - **notifications** — the top bar's bell, and the `view.notifications` palette command,
    ///   which is the same chord the bell presses.
    pub(super) fn render_status_bar(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        let plane = design::role::surface_chrome(cx);
        let summary = self.status_summary(cx);
        let compact = f32::from(window.viewport_size().width) < super::chrome_compact_width();
        // The operations count is only meaningful on top of a resource view, and it reads 0 when
        // there is none, so the scope sentence has to say which of the two is on screen.
        let operations_tooltip = operations_scope(self.active_resource_view().is_some()).to_owned();
        // Reading order runs along the bar, so the connection and the confidence of that reading
        // come before the activity counts. It is the item that stays on screen while the table is
        // scrolled, which makes it the bar's reason to exist.
        let mut items = vec![self.render_cluster_health(cx)];
        // Running work, open sessions and notifications are four different
        // questions, so each one keeps its own label — and each one is drawn only when its count
        // has something to say.
        items.extend((!compact && summary.operations > 0).then(|| {
            status_metric(
                "status-bar-operations",
                Icon::new(design::health_icon(operations_severity(summary.operations))).xsmall(),
                summary.operations,
                "operation",
                "operations",
                operations_tooltip,
                cx,
            )
        }));
        items.extend((!compact && summary.sessions > 0).then(|| {
            status_metric(
                "status-bar-sessions",
                Icon::new(IconName::SquareTerminal).xsmall(),
                summary.sessions,
                "session",
                "sessions",
                "Terminal sessions open in the Dock".to_owned(),
                cx,
            )
        }));
        // The notification count belongs here as well as in the top bar. The popover is anchored
        // to this bar, so with nothing here to belong to it read as a panel that belonged to the
        // top bar's bell 928px away, and `DESIGN.md` §9 already promised this item. It counts the
        // bell's incidents rather than the centre's history total, because one number with two
        // homes and two definitions is two numbers.
        items.extend((!compact && summary.active_notifications > 0).then(|| {
            status_metric(
                "status-bar-notifications",
                Icon::new(IconName::Bell).xsmall(),
                summary.active_notifications,
                "active notification",
                "active notifications",
                "Notifications in the centre. Active incidents are listed first.".to_owned(),
                cx,
            )
        }));
        // The Dock hides its own chip while it is collapsed, so a paused stream is legible from
        // the bar alone. It is a state and not a count, so it stays on the strip either way.
        //
        // It stays *last*. The forwards link's count lane is reserved whether or not there is a
        // count, so the space it holds is a gap between two items rather than a hole at the
        // window's edge — which is the whole reason the lane is reserved instead of sized to its
        // contents.
        items.push(self.render_port_forward_link(summary.port_forwards, cx));
        items.extend(summary.log_status.map(|log_status| {
            let (icon, severity) = log_status_presentation(log_status);
            log_status_metric(
                "status-bar-log-stream",
                Icon::new(icon).xsmall().text_color(severity.marker(cx)),
                log_status,
                format!("Log stream in the Dock: {log_status}"),
                cx,
            )
        }));
        h_flex()
            .id("status-bar")
            .debug_selector(|| "status-bar".to_owned())
            .flex_none()
            .w_full()
            .h(px(super::status_bar_height()))
            .min_w(px(0.))
            .px(STATUS_BAR_INSET)
            // One gap between peers, and the only separator the items have.
            .gap(STATUS_BAR_GAP)
            .items_center()
            .tab_group()
            .tab_index(STATUS_BAR_TAB_GROUP)
            // The readouts are `Role::Status`, which is not a tab stop, so Tab walked past every
            // number in the bar and stopped on the one button: a keyboard user arrived and found
            // unfocusable text on both sides of it. One handle for the group is enough, because
            // the metrics are read rather than operated, and the port-forward link keeps its own
            // stop inside it. The focus colour goes on the top rule, which is the bar's only edge,
            // so the state is visible without moving anything.
            //
            // The group needs a name of its own. A focusable element with no role is a generic
            // container, which is exactly the node an assistive technology drops, so the stop
            // would be announced as nothing at all — the numbers the bar exists to report would
            // be the one place a keyboard user cannot reach them.
            .role(Role::Group)
            .aria_label("Connection and activity readouts")
            .track_focus(&self.status_bar_metrics_focus)
            .focus_visible(|style| style.border_color(colors.border_focused))
            // `UI-SPEC.md` §1 puts the title bar, the sidebar and the status bar on
            // `surface.chrome`. This used to read `colors.panel_background`, a different field that
            // happens to hold the same value in both shipped themes: the bar was on chrome by
            // coincidence, and a theme that moved that plane would have moved the bar with it.
            .bg(plane.alpha(1.0))
            // The bar's top edge, and the only place a rule is drawn on it. The Dock above draws
            // none, so there is exactly one owner per boundary — and the rule is solved against
            // the plane it lands on, which is the only reason it is visible at all.
            .border_t_1()
            .border_color(bar_rule(plane, colors.border))
            .child(div().flex_1().min_w(space::SM))
            .children(items)
            .into_any_element()
    }

    /// The bar's port-forward link — `UI-SPEC` §14.6's one entry point to the list.
    ///
    /// §14.6 says no standing panel, a `2 forwards` in the status bar that is already a link, and
    /// creation on a context menu. So the link stays on the strip whatever the count is: it is a
    /// control, and the top bar's bell is a control that sits there at zero for the same reason.
    ///
    /// It used to be a gpui-kit `Button`, and that is precisely why it was not a peer of the
    /// readouts beside it. `Button::text_size` refines the button's *root* style, while the
    /// button's own content element re-applies `button_text_size(self.size)` — `text_base()`, 14px
    /// at the default `Size::Medium` — over the label, so the `METADATA` it was handed never
    /// reached the text. `.ghost()` then painted that label in `theme.secondary_foreground`
    /// instead of the bar's own muted ink, `.child(icon)` appended the shape *after* the label
    /// rather than before it, and the ghost variant's hover painted a full accent background in
    /// the light appearance. Four reasons for one item to look like the only thing on the bar,
    /// and it was saying it about a count of zero.
    ///
    /// It is built out of the same parts the readouts are — an `h_flex` at the bar's height, an
    /// `xsmall` icon, a `METADATA` label in a product ink — so it cannot drift from them again.
    /// The handle is the one the tab-order test already follows, and Enter and Space reach the
    /// list through this element: what it replaced wrapped a `tab_stop(false)` button in a
    /// focusable wrapper that carried the handle and no key handler, so the element that actually
    /// held the focus was not the one that could open the list.
    ///
    /// It opens the centre Port Forwards view and nothing else. This used to toggle a popover
    /// that held a second copy of the forward list, and that copy is the reason a reader could
    /// not click a `localhost:PORT` off the status bar, could not see how long a forward had
    /// been up, and read a failed forward in the same grey as a healthy one. `UI-SPEC.md` §14.6
    /// asks the link to open the list and §2.5 puts Port Forward in the Console family and out
    /// of the notification family, and `UI-REDESIGN.md` L10 puts the list in the centre column.
    /// One list, with the affordances the reader needs, beats two where one of them is a
    /// degraded copy of the other.
    fn render_port_forward_link(
        &self,
        forward_summary: ForwardSummary,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The name is the same whether or not the list is already on screen. A link that
        // changed its mind between "Open" and "Show" is a control whose label is a guess, and the
        // click is harmless either way: it activates the view and puts the keyboard there.
        let forward_label = format!(
            "Open {FORWARD_LINK_NAME}. {}",
            forward_summary_label(forward_summary)
        );
        // The name a listener gets, which is the subject and every count it stands for
        // (`forward_summary_short_label`). The strip draws the subject and one count in a
        // reserved lane; this says all of it, so nothing is dropped for a reader who cannot see
        // the lane.
        let announced = forward_summary_short_label(forward_summary);
        // One label, and the middots inside the announced name are its own qualifiers rather
        // than separators between the strip's items — `render_status_bar` is where the bar
        // stopped joining its items with punctuation, and this is the line that had to stop
        // looking like a path before that change read as one.
        let (icon, severity) = forward_summary_presentation(forward_summary);
        // The idle shape is `health_icon(Muted)`, which is `IconName::Dash` — a dash, and a dash
        // is punctuation. Drawn four pixels from the middot that used to separate the items it read
        // as a second separator with nothing behind it. The state that has no shape to show draws
        // no glyph, exactly as the three counts draw no item: the check arrives with the forwards
        // that earned it, and the words beside it are the same either way.
        let icon = (severity != Severity::Muted).then_some(icon);
        // Nothing to say borrows the readouts' ink. `role::status_for` sends `Muted` a step
        // quieter than the bar's own text on purpose, and the one item that is on the strip
        // because it is a control must not also be the quietest thing on it.
        let icon_color = match severity {
            Severity::Muted => design::role::fg_secondary(cx),
            _ => severity.marker(cx),
        };
        // A quiet wash of the bar's own ink. The ghost button's hover was the accent token, which
        // spends one of the screen's two accent places on a status bar that asks for none.
        let hover = design::state::hover_on(
            design::role::surface_chrome(cx),
            design::role::fg_primary(cx),
        );
        // The destination's own ink is a rung above the readouts', because it is the one thing on
        // the bar a reader can press. That is the affordance, and it is cheaper than a border.
        let label_ink = design::role::fg_primary(cx);
        let open_list = |shell: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            shell.close_status_panel(window, cx);
            shell.command_open_forwards(window, cx);
        };
        let mut link = h_flex()
            .id("status-bar-port-forwards")
            // A selector so a test can prove §14.6's link is on the bar with nothing to report.
            .debug_selector(|| "status-bar-port-forwards".to_owned())
            .h(px(super::status_bar_item_height()))
            .flex_none()
            .gap(space::XS)
            .items_center()
            .px(space::XXS)
            .rounded_md()
            .role(Role::Button)
            .aria_label(announced.clone())
            .track_focus(&self.status_bar_port_forward_focus)
            .focus_visible(panel_focus_ring(design::colors(cx).border_focused))
            .hover(|this| this.bg(hover))
            .on_click(
                cx.listener(move |shell, _: &ClickEvent, window, cx| open_list(shell, window, cx)),
            )
            .on_key_down(cx.listener(move |shell, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    open_list(shell, window, cx);
                    cx.stop_propagation();
                }
            }))
            .when_some(icon, |this, icon| {
                this.child(Icon::new(icon).xsmall().text_color(icon_color))
            })
            .child(
                Label::new(FORWARD_LINK_NAME)
                    .text_size(design::text::CAPTION)
                    .text_color(label_ink),
            )
            // The count's own lane, reserved whether or not there is a count, so a count
            // arriving does not move the label it belongs to. The lane is inside the control's
            // focus ring, which is right: it is part of the thing you press.
            .child(
                h_flex()
                    .flex_none()
                    .w(px(FORWARD_COUNT_LANE))
                    .h(px(super::status_bar_item_height()))
                    .justify_end()
                    .items_center()
                    .when_some(forward_count_lane_label(forward_summary), |this, count| {
                        this.child(
                            Label::new(count)
                                .text_size(design::text::CAPTION)
                                .text_color(design::role::fg_secondary(cx)),
                        )
                    }),
            );
        link.interactivity()
            .tooltip(common::hover_hint(forward_label.clone()));
        link.into_any_element()
    }

    pub(super) fn render_notification_backdrop(&self, cx: &Context<Self>) -> AnyElement {
        // The status bar stays above the scrim so its trigger can toggle the panel again.
        div()
            .id("notification-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom(px(super::status_bar_height()))
            .occlude()
            // Intercepting the click is right: `popovers.md` closes a popover when you click outside
            // it, and without this the click would land on the content behind and the popover would
            // stay open on top of a view that had already changed. What was wrong was doing it
            // invisibly — an `.occlude()` with no fill made everything above the bar unclickable
            // while looking completely normal, which `modality.md` counts as a hidden modal. The
            // scrim is the app's one backdrop role, so the dead zone is now visible as a dead zone.
            .bg(design::role::surface_backdrop(cx))
            .cursor_default()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, _, window, cx| shell.close_status_panel(window, cx)),
            )
            .into_any_element()
    }

    pub(super) fn focus_notification_control(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reverse: bool,
    ) {
        let mut controls = Vec::with_capacity(self.notification_row_focus_handles.len() + 2);
        controls.push(self.notification_focus.clone());
        if !self.notifications.is_empty() {
            controls.push(self.notification_clear_focus.clone());
        }
        controls.extend(
            self.notification_row_focus_handles
                .iter()
                .take(collapsed_notifications(&self.notifications).0.len())
                .cloned(),
        );
        if controls.len() < 2 {
            return;
        }
        let current = window.focused(cx);
        let index = current
            .and_then(|handle| controls.iter().position(|control| *control == handle))
            .unwrap_or(0);
        let next = if reverse {
            (index + controls.len() - 1) % controls.len()
        } else {
            (index + 1) % controls.len()
        };
        // The panel and the clear control sit outside the scroller, so the row
        // that Tab arrived at is the one before it in this list.
        if next >= 2 {
            self.notifications_scroll.scroll_to_item(next - 2);
        }
        window.focus(&controls[next], cx);
    }

    pub(super) fn activate_notification_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = window.focused(cx) else {
            return;
        };
        if current == self.notification_clear_focus {
            self.notifications.clear();
            self.close_notifications(window, cx);
            return;
        }
        let Some(index) = self
            .notification_row_focus_handles
            .iter()
            .position(|handle| *handle == current)
        else {
            return;
        };
        let Some(notification) = collapsed_notifications(&self.notifications)
            .0
            .into_iter()
            .nth(index)
        else {
            return;
        };
        if notification.detail.is_some() {
            self.toggle_notification(notification.id, cx);
        }
    }

    /// Renders the notification popover above the status bar.
    pub(super) fn render_notification_center(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (visible, collapsed) = collapsed_notifications(&self.notifications);
        let count = visible.len();
        let active = visible
            .iter()
            .filter(|notification| notification_is_active_incident(notification))
            .count();
        let overflow = notification_overflow_count(count);
        let active_summary =
            design::format::count_with_noun(active, "active notification", "active notifications");
        let overflow_summary =
            design::format::count_with_noun(overflow, "more notification", "more notifications");
        let status_label = notification_status_label(collapsed, overflow);
        // One description, because a second call would replace the first.
        let mut description = Vec::new();
        if collapsed > 0 {
            description.push(format!(
                "{} duplicate {} collapsed.",
                design::format::count(collapsed),
                if collapsed == 1 {
                    "entry was"
                } else {
                    "entries were"
                }
            ));
        }
        if overflow > 0 {
            description.push(format!(
                "{active_summary}. Scroll to see {overflow_summary}."
            ));
        }
        let list_description = (!description.is_empty()).then(|| description.join(" "));
        let rows: Vec<AnyElement> = visible
            .into_iter()
            .enumerate()
            .map(|(index, notification)| {
                self.render_notification_row(index, count, notification, cx)
            })
            .collect();
        let body: AnyElement = if rows.is_empty() {
            h_flex()
                .id("notifications-empty")
                .px(space::MD)
                .py(NOTIFICATION_EMPTY_PADDING)
                .justify_center()
                .role(Role::Status)
                .aria_label("No notifications")
                .child(
                    Label::new("No notifications")
                        .text_size(design::text::CAPTION)
                        .text_color(design::colors(cx).text_muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .id("notifications-list")
                .role(Role::List)
                .aria_label(notification_list_announcement(count))
                .when_some(list_description, |this, description| {
                    this.aria_description(description)
                })
                .children(rows)
                .into_any_element()
        };
        let colors = design::colors(cx);
        let raised = design::role::surface_raised(cx);
        div()
            .id("notification-center")
            // A selector so a test can measure the panel the toast has to stay clear of. The
            // port-forward panel carried one for the same reason; this is the surface that
            // replaced it.
            .debug_selector(|| "notification-center".to_owned())
            .track_focus(&self.notification_focus)
            .tab_group()
            .tab_index(NOTIFICATION_TAB_GROUP)
            .key_context("Notifications")
            .role(Role::Dialog)
            .accessibility_id("notification-center")
            .aria_label("Notifications")
            .aria_keyshortcuts("Tab Shift+Tab Escape")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, _, window, cx| {
                    window.focus(&shell.notification_focus, cx);
                    cx.stop_propagation();
                }),
            )
            .absolute()
            // Clear of the status bar, whose notification metric is now the item this popover
            // belongs to. The bell that opens it is in the top bar, and `windows.md` keeps it there
            // because a window can be moved until its bottom edge is off screen.
            .bottom(px(super::status_bar_height() + POPOVER_CLEARANCE))
            // The bar's own inset, so the popover's trailing edge is the trailing
            // edge of the text on the strip it belongs to rather than a second
            // answer to "where does content end".
            .right(STATUS_BAR_INSET)
            .w(px(notification_width(f32::from(
                window.viewport_size().width,
            ))))
            .max_h(px(NOTIFICATION_MAX_HEIGHT))
            .rounded_lg()
            .border_1()
            .border_color(popover_border(raised, colors.border))
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(cx.theme().shadow_tokens().lg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .min_h(design::size::STATUS_BAR)
                    .px(space::SM)
                    .gap(space::SM)
                    .items_center()
                    // With an empty list the header sits directly on the empty state, and a rule
                    // between "Notifications" and "No notifications" separates nothing.
                    .when(notification_header_rule(count), |this| {
                        this.border_b_1()
                            .border_color(popover_rule(raised, colors.border_variant))
                    })
                    .child(Label::new("Notifications").text_size(design::text::TITLE))
                    .when(count > 0, |this| {
                        this.child(
                            Label::new(notification_count_label(count))
                                .text_size(design::text::CAPTION)
                                .text_color(colors.text_muted),
                        )
                    })
                    .child(div().flex_1())
                    .when(count > 0, |this| {
                        this.child(panel_action(
                            "notifications-clear-action",
                            &self.notification_clear_focus,
                            "Clear all notifications",
                            colors.border_focused,
                            Button::new("notifications-clear")
                                // Sentence case, and the scope in the name rather than in
                                // the button: a reader who has just seen eleven errors
                                // dismiss eleven notifications has not been asked to
                                // confirm anything, and `All` in title case is the only
                                // thing about this row that read as a dialog.
                                .label("Clear all")
                                .ghost()
                                .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
                                    shell.notifications.clear();
                                    shell.close_notifications(window, cx);
                                })),
                        ))
                    }),
            )
            .child(
                div()
                    .id("notifications-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.notifications_scroll)
                    .child(body),
            )
            .when(!status_label.is_empty(), |this| {
                this.child(
                    h_flex()
                        .id("notifications-overflow")
                        .flex_none()
                        .w_full()
                        .min_h(design::size::STATUS_BAR)
                        .px(space::SM)
                        .gap(space::XS)
                        .items_center()
                        .border_t_1()
                        .border_color(popover_rule(raised, colors.border_variant))
                        .role(Role::Status)
                        .aria_label(status_label.clone())
                        .child(
                            Icon::new(IconName::Info)
                                .xsmall()
                                .text_color(colors.text_muted),
                        )
                        .child(
                            Label::new(status_label)
                                .text_size(design::text::CAPTION)
                                .text_color(colors.text_muted),
                        ),
                )
            })
            // Painted last so the rail is never hidden behind a row's own background.
            .child(popover_focus_rail(
                &self.notification_focus,
                colors.border_focused,
            ))
            .into_any_element()
    }

    fn toggle_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(entry) = self.notifications.iter_mut().find(|entry| entry.id == id) {
            entry.expanded = !entry.expanded;
        }
        cx.notify();
    }

    fn render_notification_row(
        &self,
        index: usize,
        total: usize,
        notification: &Notification,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = design::colors(cx);
        let raised = design::role::surface_raised(cx);
        let expanded = notification.expanded;
        let has_detail = notification.detail.is_some();
        // The words come from the app's one health vocabulary.
        let severity = notification_severity_word(notification.severity);
        let aria_label = match (has_detail, expanded) {
            (true, true) => format!(
                "{severity} notification: {}. Details:\n{}\nSelect to collapse details.",
                notification.message,
                notification.detail.as_deref().unwrap_or_default()
            ),
            (true, false) => format!(
                "{severity} notification: {}. Select to expand details.",
                notification.message
            ),
            (false, _) => format!("{severity} notification: {}", notification.message),
        };
        let id = notification.id;
        let focus = self
            .notification_row_focus_handles
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.notification_focus.clone());
        let row = h_flex()
            .id(("notification", id as usize))
            .debug_selector(|| "notification-row".to_owned())
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| {
                    window.focus(&focus, cx);
                    cx.stop_propagation();
                }),
            )
            .w_full()
            // The row grows with its message: a fixed height cut the last wrapped line in half.
            .min_h(design::size::STATUS_BAR)
            .px(space::SM)
            .py(space::XS)
            .gap(space::XS)
            .items_start()
            .when(notification_row_rule(index, total), |this| {
                this.border_b_1()
                    .border_color(popover_rule(raised, colors.border_variant))
            })
            .role(Role::ListItem)
            .accessibility_id(format!("notification-{id}"))
            .aria_label(aria_label)
            .aria_position_in_set(index + 1)
            .aria_size_of_set(total)
            .when(has_detail, |this| this.aria_expanded(expanded))
            .when(has_detail, |this| {
                // No hand cursor on a notification row: `UI-SPEC` §9.3 lists
                // one, and a list that changes the cursor is a list that has
                // announced itself as a web page.
                this.hover(|this| this.bg(colors.element_hover))
                    .on_click(cx.listener(move |shell, _: &ClickEvent, _, cx| {
                        shell.toggle_notification(id, cx);
                    }))
                    .on_key_down(cx.listener(move |shell, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            shell.toggle_notification(id, cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(
                Icon::new(design::health_icon(notification.severity))
                    .xsmall()
                    .text_color(notification.severity.marker(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.0))
                    .child(
                        Label::new(notification.message.clone()).text_size(design::text::CAPTION),
                    )
                    .when(expanded, |this| {
                        this.when_some(notification.detail.clone(), |this, detail| {
                            this.child(
                                Label::new(detail)
                                    .text_size(design::text::CAPTION)
                                    .text_color(colors.text_muted),
                            )
                        })
                    }),
            )
            // The age gets its own column, so a wrapped sentence never runs into the timestamp.
            .child(
                h_flex()
                    .flex_none()
                    .w(px(NOTIFICATION_AGE_COLUMN))
                    .ml(space::SM)
                    .justify_end()
                    .child(
                        Label::new(format_age(notification.at.elapsed().as_secs()))
                            .text_size(design::text::CAPTION)
                            .text_color(colors.text_muted),
                    ),
            )
            .when(has_detail, |this| {
                this.child(
                    Icon::new(if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .xsmall()
                    .text_color(colors.text_muted),
                )
            });
        row.into_any_element()
    }
}

fn format_age(seconds: u64) -> String {
    let (value, unit) = if seconds >= 3_600 {
        (seconds / 3_600, "hour")
    } else if seconds >= 60 {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    if value == 1 {
        format!("{value} {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Hsla, TestAppContext};

    use super::*;
    use crate::panels::ForwardPhase;
    use crate::panels::forwards::phase_presentation;

    fn forward_summary(active: usize, failed: usize, pending: usize) -> ForwardSummary {
        ForwardSummary {
            active,
            failed,
            pending,
            stopped: 0,
        }
    }

    /// One of the theme file's own declared values, as a colour.
    ///
    /// This parses the theme; it does not name a colour. The distinction matters:
    /// the forbidden thing is a colour *written down in code*, and a role this
    /// reads out of `style.<key>` is the skin system working — the same value a
    /// different skin would replace. Alpha is forced opaque because a declared
    /// token is a value and a value is not a wash; the washes are their own keys.
    fn k8s_hex(raw: &str) -> Hsla {
        let digits = raw.strip_prefix('#').expect("the theme writes #rrggbb");
        gpui_kit::rgba(
            u32::from_str_radix(&digits[..6], 16).expect("the theme writes six hex digits") << 8
                | 0xff,
        )
        .into()
    }

    /// A role from the product theme, read the same way `design.rs` reads it.
    ///
    /// The theme file keeps a role twice: `style.<key>` is the declared value and
    /// `style.colors.<key>` is the one the refine pass wrote back, and the two already differ
    /// (`elevated_surface.background` is `#202C3A` declared and `#192434` refined in dark). Both
    /// are checked below, because a popover lands on whichever one the running theme holds and
    /// only that surface decides whether its edge is visible.
    fn k8s_role(appearance: &str, key: &str) -> Hsla {
        let style = k8s_style(appearance);
        style
            .get(key)
            .or_else(|| style["colors"].get(key))
            .and_then(serde_json::Value::as_str)
            .map(k8s_hex)
            .unwrap_or_else(|| panic!("missing theme color {appearance}/{key}"))
    }

    fn k8s_style(appearance: &str) -> serde_json::Value {
        let name = match appearance {
            "light" => "K8s Studio Light",
            "dark" => "K8s Studio Dark",
            other => panic!("unknown K8s Studio appearance {other}"),
        };
        let theme: serde_json::Value = serde_json::from_str(include_str!(
            "../../../k8s-app/assets/themes/k8s-studio.json"
        ))
        .expect("theme JSON");
        theme["themes"]
            .as_array()
            .expect("theme list")
            .iter()
            .find(|theme| theme.get("name").and_then(serde_json::Value::as_str) == Some(name))
            .expect("K8s Studio appearance")["style"]
            .clone()
    }

    /// Every raised surface the popovers paint on for one appearance, declared and refined.
    fn k8s_raised_surfaces(appearance: &str) -> Vec<Hsla> {
        let style = k8s_style(appearance);
        let read =
            |value: Option<&serde_json::Value>| value.and_then(|value| value.as_str()).map(k8s_hex);
        let mut surfaces = vec![
            read(style.get("elevated_surface.background"))
                .unwrap_or_else(|| panic!("missing raised surface for {appearance}")),
        ];
        if let Some(refined) = read(style["colors"].get("elevated_surface.background"))
            && !surfaces.contains(&refined)
        {
            surfaces.push(refined);
        }
        surfaces
    }

    #[test]
    fn age_formats_are_user_facing() {
        assert_eq!(format_age(1), "1 second");
        assert_eq!(format_age(5), "5 seconds");
        assert_eq!(format_age(90), "1 minute");
        assert_eq!(format_age(3_600), "1 hour");
        assert_eq!(format_age(7_200), "2 hours");
    }

    #[test]
    fn port_forward_summary_preserves_quick_status() {
        let summary = ForwardSummary {
            active: 2,
            failed: 1,
            pending: 1,
            stopped: 3,
        };
        assert_eq!(
            forward_summary_label(summary),
            "2 active, 1 failed, 1 pending, 3 stopped"
        );
    }

    /// The bar prints the same phase vocabulary the Port Forwards panel does.
    ///
    /// The bar used to carry a second phase-to-shape map, so a forward could wear one glyph here
    /// and another in the panel that owns it, and `Failed` wore a warning triangle here while
    /// every other failure in the app wore the error's filled cross. The shapes are now
    /// `design::health_icon`'s, which gives one shape per health class rather than per phase, so
    /// the words are what tell `Starting` from `Stopping`.
    #[test]
    fn the_bar_and_the_forward_panel_share_one_phase_vocabulary() {
        let (_, status, _, _) = phase_presentation(ForwardPhase::Running);
        assert_eq!(status, "Running");
        for phase in [
            ForwardPhase::Stopped,
            ForwardPhase::Starting,
            ForwardPhase::Running,
            ForwardPhase::Stopping,
            ForwardPhase::Failed,
        ] {
            let (icon, status, severity, next_step) = phase_presentation(phase);
            assert_eq!(
                icon,
                design::health_icon(severity),
                "{status} must take its shape from the shared health vocabulary"
            );
            assert!(!next_step.is_empty(), "{status} has to say what to do next");
        }

        let (_, starting, _, _) = phase_presentation(ForwardPhase::Starting);
        let (_, stopping, _, _) = phase_presentation(ForwardPhase::Stopping);
        // One health class, so one shape. The words are the only thing left to separate them,
        // so they have to actually be different words.
        assert_ne!(starting, stopping);
    }

    /// The bar's three counts and its one toggle all read as the same kind of fact.
    ///
    /// `0 operations`, `0 sessions` and `2 notifications` are "a number and its noun", so the
    /// port-forward entry has to be one too: it used to print `Active 0`, a third grammar that
    /// named no subject at all, and its button was the only one in the bar with a Tab stop. The
    /// popover header keeps the compact form because the title 100px to its left already says
    /// `Port Forwards`, and the two must not say it twice.
    #[test]
    fn the_bar_label_shares_the_grammar_of_its_neighbours() {
        assert_eq!(
            design::format::count_with_noun(0, "operation", "operations"),
            "0 operations"
        );
        assert_eq!(
            design::format::count_with_noun(0, "session", "sessions"),
            "0 sessions"
        );
        assert_eq!(
            design::format::count_with_noun(2, "notification", "notifications"),
            "2 notifications"
        );
        // Nothing running, nothing failed, nothing starting: the bar is the one surface that is
        // always on screen and always the same, so it does not report the absence of a forward.
        // The link itself stays — `UI-SPEC` §14.6 wants the count reachable from the bar — and it
        // keeps naming its subject, it just stops claiming there are none. Every non-zero case
        // below is still a number and its noun, like the operations and sessions beside it.
        assert_eq!(
            forward_summary_short_label(forward_summary(0, 0, 0)),
            "Port forwards"
        );
        assert_eq!(
            forward_summary_short_label(forward_summary(2, 0, 0)),
            "Port forwards · 2 active"
        );
        // A zero active count must not hide a failure or a start in flight, so the problem bucket
        // is spelled out rather than left to the tooltip.
        assert_eq!(
            forward_summary_short_label(forward_summary(0, 3, 0)),
            "Port forwards · 0 active · 3 failed"
        );
        assert_eq!(
            forward_summary_short_label(forward_summary(1, 0, 2)),
            "Port forwards · 1 active · 2 pending"
        );
        // The bar's label is not a count, so it does not read as one.
        assert!(
            !forward_summary_short_label(forward_summary(0, 0, 0)).starts_with("Active"),
            "the bar's entry must name its subject"
        );
    }

    /// The bar is one fixed row that says nothing about nothing.
    ///
    /// Two invariants that both fail silently. The first is the strip's height: the bar is
    /// permanent, so a count that arrives or leaves must not push the row — the window's only
    /// bottom band moving under the pointer is the kind of change nobody reviews and everybody
    /// feels. The second is the zero: `0 operations`, `0 sessions` and `0 notifications` were on
    /// the strip in every state, and §8's 克制 group asks the interface not to spend a permanent
    /// strip on what is not happening. Both were only ever true because each item decided on its
    /// own whether to be drawn, and nothing checked the pair together.
    ///
    /// The notifications are the driver because the shell owns that list directly, and the counts
    /// are all built by the same call, so one of them moving exercises the rule for the other two.
    /// §14.6's link is asserted in the resting state too, because an item that hides at zero and
    /// takes the reader's way into the port-forward list with it would be a different bug.
    #[gpui_kit::test]
    fn the_bar_keeps_its_height_and_draws_no_item_for_a_zero(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
        cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
        cx.run_until_parked();

        let resting = cx.debug_bounds("status-bar").expect("the status bar");
        assert_eq!(resting.size.height, px(super::super::status_bar_height()));
        for absent in [
            "status-bar-operations",
            "status-bar-sessions",
            "status-bar-notifications",
        ] {
            assert!(
                cx.debug_bounds(absent).is_none(),
                "{absent} has nothing to report and must not occupy the bar"
            );
        }
        assert!(
            cx.debug_bounds("status-bar-port-forwards").is_some(),
            "§14.6's link is how the forwards list is opened, so it stays with a count of zero"
        );

        shell.update(cx, |shell, cx| {
            shell.notifications.push(notification(
                1,
                "apply failed",
                Severity::Error,
                std::time::Instant::now(),
            ));
            cx.notify();
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("status-bar-notifications").is_some());
        for still_absent in ["status-bar-operations", "status-bar-sessions"] {
            assert!(cx.debug_bounds(still_absent).is_none());
        }
        let reporting = cx.debug_bounds("status-bar").expect("the status bar");
        assert_eq!(
            reporting.size.height, resting.size.height,
            "the bar changed height when a count appeared: {resting:?} → {reporting:?}"
        );
        assert_eq!(reporting.top(), resting.top());
    }

    #[test]
    fn notification_overlay_stays_inside_narrow_windows() {
        assert_eq!(notification_width(1440.0), NOTIFICATION_WIDTH);
        assert_eq!(notification_width(400.0), 336.0);
        // A window narrower than the gutters keeps a usable popover.
        assert_eq!(notification_width(20.0), f32::from(design::size::CONTROL));
    }

    /// The bar's leading item reports the connection, and only the connection.
    ///
    /// `ConnectionState` is fed by a single `GET /readyz` against kube-apiserver, so the health
    /// word it used to print here rested on nothing but reachability, and it could only ever turn
    /// amber because the app lost the API server — never because a workload broke. The Overview
    /// banner's `Overview::level()` is the cluster health verdict; two auditors measured the same
    /// frame printing `Cluster needs attention` there and `Cluster · Healthy` here.
    #[test]
    fn the_bar_reports_the_connection_and_never_the_clusters_health() {
        let states = [
            ConnectionState::Live,
            ConnectionState::Connecting,
            ConnectionState::Reconnecting("probe failed".to_owned()),
            ConnectionState::Failed("no usable context".to_owned()),
        ];
        let mut words: Vec<&str> = Vec::new();
        for connection in &states {
            let text = connection_text(connection);
            assert_eq!(text, format!("Connection · {}", connection.label()));
            assert!(
                !text.contains("Healthy") && !text.contains("Cluster"),
                "the item must not carry a cluster-health verdict: {text}"
            );
            assert!(
                !text.chars().any(|character| character.is_ascii_digit()),
                "reachability is a state, not a count: {text}"
            );
            words.push(connection.label());
        }
        for (index, word) in words.iter().enumerate() {
            for other in &words[index + 1..] {
                assert_ne!(word, other, "two connection states cannot share one word");
            }
        }
        // The health word is still available to the surfaces that really own a verdict.
        //
        // `UI-REDESIGN.md` D5 inverts the status channel: healthy is grey, so the
        // connection point is `Muted` and must never be the surface that says "Healthy".
        // The Overview banner, which does own a verdict, is the surface that keeps the word.
        assert_eq!(design::health_label(design::Severity::Success), "Healthy");
        assert_eq!(
            design::health_label(ConnectionState::Live.severity()),
            "No verdict",
            "a reachable cluster is not a health verdict (D5)"
        );
    }

    /// The list a reader hears says how many rows it holds, in the app's number format.
    #[test]
    fn the_notification_list_announces_its_length_in_the_shared_format() {
        assert_eq!(notification_list_announcement(0), "Notifications: 0 shown");
        assert_eq!(notification_list_announcement(7), "Notifications: 7 shown");
        assert_eq!(
            notification_list_announcement(1_200),
            "Notifications: 1,200 shown",
            "the centre is the only list in the app that reaches four digits, and a bare `1200` \
             beside a `1,200` in the same window is two formats for one fact"
        );
    }

    /// A readout a Tab lands on has to say what it is counting.
    ///
    /// The metrics are `Role::Status`, which is not a tab stop, so the bar took one handle for the
    /// whole strip and the numbers became reachable. That only helps if the element answers the
    /// question the reader arrives with, and a bare `0 operations` does not: the count is the
    /// pending writes on the view in the centre tab, and with no resource view open it is 0 for a
    /// reason that is not "nothing is happening". So the scope sentence is announced, not just
    /// hovered.
    #[test]
    fn a_reachable_readout_announces_its_scope() {
        assert_eq!(
            metric_announcement("0 operations", operations_scope(false)),
            "0 operations. No resource view is open, so nothing can be pending"
        );
        assert_eq!(
            metric_announcement("3 operations", operations_scope(true)),
            "3 operations. Pending operations on the view in the centre tab"
        );
        // The two cases must not read the same, or the sentence is decoration.
        assert_ne!(operations_scope(true), operations_scope(false));
        assert_eq!(
            metric_announcement(&log_status_text("Live"), "Terminal log stream in the Dock"),
            "Logs · Live. Terminal log stream in the Dock"
        );
    }

    /// The Dock is not rendered while it is collapsed, so a paused or reconnecting stream has to
    /// be legible from the bar alone. Its shape comes from the app's one health vocabulary, and its
    /// words always name the state, so nothing rests on colour or on a shape alone.
    ///
    /// This file used to keep a private glyph map, and one of its entries was `LoadCircle` — the
    /// same glyph the operations metric drew as a *static* spinner next to a zero. One shape was
    /// saying "nothing is running" and "connecting" in the same strip, and the static one claimed
    /// work that did not exist.
    #[test]
    fn log_stream_state_is_shaped_and_named_in_the_bar() {
        let states = [
            "Live",
            "Paused",
            "Connecting",
            "Reconnecting",
            "Failed",
            "Unavailable",
        ];
        for state in states {
            let (icon, severity) = log_status_presentation(state);
            assert_eq!(
                icon,
                design::health_icon(severity),
                "{state} must draw the shared health shape, not a private one"
            );
            assert_eq!(
                log_status_text(state),
                format!("Logs · {state}"),
                "{state} must name itself, so no channel is carrying the state alone"
            );
        }
        assert_eq!(
            log_status_presentation("Live"),
            (IconName::Check, Severity::Success)
        );
        assert_eq!(
            log_status_presentation("Failed"),
            (IconName::CircleX, Severity::Error)
        );
        assert_eq!(
            log_status_presentation("Paused"),
            (IconName::Dash, Severity::Muted)
        );
        // Nothing in the bar may claim a spinner is turning when the count is zero.
        assert_ne!(
            design::health_icon(operations_severity(0)),
            IconName::LoaderCircle,
            "a zero count cannot wear a static loading spinner"
        );
        assert_eq!(operations_severity(0), Severity::Muted);
        assert_eq!(operations_severity(3), Severity::Info);
    }

    /// `DESIGN.md` §4 makes `design::health_icon` the app's only status-shape vocabulary, and the
    /// port-forward entry used to keep a second one: `ArrowRightLeft` for both "some are running"
    /// and "none are", so the shape carried nothing and only the colour separated them.
    #[test]
    fn port_forward_states_use_the_shared_shape_vocabulary() {
        for (summary, severity) in [
            (forward_summary(0, 0, 0), Severity::Muted),
            (forward_summary(2, 0, 0), Severity::Success),
            (forward_summary(0, 0, 1), Severity::Warning),
            (forward_summary(0, 1, 0), Severity::Error),
        ] {
            let (icon, actual) = forward_summary_presentation(summary);
            assert_eq!(actual, severity);
            assert_eq!(icon, design::health_icon(severity));
        }
        let (_, idle) = forward_summary_presentation(forward_summary(0, 0, 0));
        let (_, running) = forward_summary_presentation(forward_summary(2, 0, 0));
        assert_ne!(
            forward_summary_presentation(forward_summary(0, 0, 0)).0,
            forward_summary_presentation(forward_summary(2, 0, 0)).0,
            "an idle count and a running one cannot share one shape"
        );
        // The idle case is the one that has to borrow the metrics' muted ink, because `Active 0`
        // was the loudest item in a bar full of quiet readouts.
        assert_eq!(idle, Severity::Muted);
        assert_ne!(running, Severity::Muted);
    }

    /// A floating surface's edge is a graphic, and `design::border::INTERACTIVE_MIN_CONTRAST` is
    /// the threshold meant for one.
    ///
    /// `colors.border` is solved for a control on a panel, and on the raised surface a popover
    /// paints it measured 1.37:1 in dark and 2.67:1 in light: below the threshold in both, and
    /// invisible next to a popover whose focus ring was drawn in accent blue.
    #[test]
    fn a_popovers_edge_clears_the_interactive_threshold_in_both_appearances() {
        let threshold = design::border::INTERACTIVE_MIN_CONTRAST;
        for appearance in ["light", "dark"] {
            let preferred = k8s_role(appearance, "border");
            for raised in k8s_raised_surfaces(appearance) {
                let border = popover_border(raised, preferred);
                let ratio = design::calculate_contrast_ratio(border, raised.alpha(1.0));
                assert!(
                    ratio >= threshold,
                    "{appearance}: the popover edge is {ratio:.2}:1 on {raised:?}, below the \
                     {threshold:.1}:1 a boundary has to clear"
                );
                assert_eq!(
                    border.alpha(1.0),
                    border,
                    "{appearance}: the edge must be opaque"
                );
                // The focus cue is a separate element, so the edge is never asked to be the ring.
                assert_ne!(
                    border,
                    k8s_role(appearance, "border.focused"),
                    "{appearance}: the resting edge must not be the focus colour"
                );
            }
        }
    }

    /// A structural rule may be quiet; it may not be absent.
    ///
    /// `border.variant` is solved against the table, and the popover paints it on `raised`, where
    /// it landed at 1.12:1 in dark and 1.26:1 in light — under the floor the rest of the app
    /// enforces, which is why the header rule read as nothing at all.
    #[test]
    fn a_popovers_rules_clear_the_rule_floor_in_both_appearances() {
        let floor = design::border::MIN_RULE_CONTRAST;
        for appearance in ["light", "dark"] {
            let preferred = k8s_role(appearance, "border.variant");
            for raised in k8s_raised_surfaces(appearance) {
                let rule = popover_rule(raised, preferred);
                let ratio = design::calculate_contrast_ratio(rule, raised.alpha(1.0));
                assert!(
                    ratio >= floor,
                    "{appearance}: the popover rule is {ratio:.2}:1 on {raised:?}, below the \
                     {floor:.1}:1 floor"
                );
            }
        }
    }

    /// A notification row's severity is named with the app's one health vocabulary.
    ///
    /// The row used to keep a private severity-to-word map that disagreed with the one three lines
    /// up: `Error` against `Failed`, `Warning` against `Needs attention`, `Info` against `Syncing`.
    /// A reader moving between a table row and a notification heard two words for one state.
    #[test]
    fn notification_rows_speak_the_shared_health_words() {
        for (severity, word) in [
            (Severity::Error, "Failed"),
            (Severity::Warning, "Needs attention"),
            (Severity::Info, "Syncing"),
            (Severity::Success, "Healthy"),
            (Severity::Neutral, "No verdict"),
            (Severity::Muted, "No verdict"),
        ] {
            assert_eq!(notification_severity_word(severity), word);
        }
        for retired in ["Error", "Warning", "Information", "Status"] {
            assert!(
                ![
                    Severity::Error,
                    Severity::Warning,
                    Severity::Info,
                    Severity::Success,
                    Severity::Neutral,
                    Severity::Muted,
                ]
                .iter()
                .any(|severity| notification_severity_word(*severity) == retired),
                "the private word `{retired}` must not come back"
            );
        }
    }

    /// The rule between two rows, and only there.
    ///
    /// Every row used to draw a bottom rule including the last one, and the popover's
    /// `overflow_hidden` put that line exactly on the panel's own bottom edge, so a list of one
    /// drew a single rule and a list of three drew three, where a list draws one fewer than it has
    /// rows. An empty list also drew a rule under its header, on top of nothing.
    #[test]
    fn a_list_draws_one_rule_fewer_than_it_has_rows() {
        for total in 0..=4usize {
            let drawn = (0..total)
                .filter(|index| notification_row_rule(*index, total))
                .count();
            assert_eq!(
                drawn,
                total.saturating_sub(1),
                "{total} rows must draw {drawn} rules"
            );
        }
        assert!(notification_row_rule(0, 2));
        assert!(!notification_row_rule(1, 2), "the last row draws no rule");
        assert!(!notification_row_rule(0, 1), "a lone row draws no rule");
        assert!(
            !notification_header_rule(0),
            "an empty list has no rule to draw"
        );
        assert!(notification_header_rule(1));
    }

    /// The popover is as tall as its contents.
    #[test]
    fn the_empty_popover_holds_one_line_instead_of_fifty_pixels_of_air() {
        let height = f32::from(design::size::STATUS_BAR)
            + 2.0 * f32::from(NOTIFICATION_EMPTY_PADDING)
            + f32::from(design::text::CAPTION_LINE_HEIGHT);
        // 24px header + 16px padding + 14px line = 54px. The status bar is 24
        // now (`UI-SPEC` §4.17), down from 32.
        assert_eq!(height, 54.0);
        assert!(
            height < f32::from(design::size::STATUS_BAR) + 50.0,
            "the empty popover is {height}px tall for one line of text"
        );
    }

    #[test]
    fn notification_count_and_overflow_speak_the_shared_number_format() {
        assert_eq!(notification_count_label(1), "1 notification");
        assert_eq!(notification_count_label(1_234), "1,234 notifications");
        assert_eq!(
            notification_status_label(1_200, 3),
            "1,200 duplicate entries collapsed · 3 more notifications · scroll to view"
        );
    }

    /// The bar's severity `match` is an ordering, and says so in a test.
    ///
    /// `DESIGN.md` §4 reserves the glyph and label vocabularies for `design`, so a `match` over
    /// a `Severity` anywhere else has to be justified. This one is: the arms pick a number, and
    /// nothing here draws, names or colours anything. The assertion that makes that checkable is
    /// that the ranks are a strict total order over every variant, and that it agrees with the
    /// rule the list actually sorts by — an active incident outranks a finished one.
    #[test]
    fn the_notification_order_is_a_total_order_over_every_severity() {
        let ladder = [
            (Severity::Error, 0),
            (Severity::Warning, 1),
            (Severity::Info, 2),
            (Severity::Success, 3),
            (Severity::Neutral, 4),
            (Severity::Muted, 5),
        ];
        for (severity, rank) in ladder {
            assert_eq!(notification_severity_rank(severity), rank, "{severity:?}");
        }
        let ranks: Vec<u8> = ladder
            .iter()
            .map(|(severity, _)| notification_severity_rank(*severity))
            .collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ranks.len(),
            "two severities sharing a rank would tie, and the tie-break would be the clock"
        );
        // The two severities the bar treats as incidents have to be the two loudest, or the
        // ordering disagrees with `notification_is_active_incident` for a bare notification.
        for severity in [Severity::Error, Severity::Warning] {
            let notification = Notification {
                id: 1,
                message: "bare".to_owned().into(),
                severity,
                detail: None,
                at: std::time::Instant::now(),
                expanded: false,
            };
            assert!(notification_is_active_incident(&notification));
            assert!(
                notification_severity_rank(severity) < notification_severity_rank(Severity::Info)
            );
        }
    }

    fn notification(
        id: u64,
        message: &str,
        severity: Severity,
        at: std::time::Instant,
    ) -> Notification {
        Notification {
            id,
            message: message.to_owned().into(),
            severity,
            detail: None,
            at,
            expanded: false,
        }
    }

    #[test]
    fn notifications_prioritize_active_incidents_then_severity_time_and_name() {
        let now = std::time::Instant::now();
        let notifications = vec![
            notification(1, "z info", Severity::Info, now),
            notification(
                2,
                "older warning",
                Severity::Warning,
                now - std::time::Duration::from_secs(2),
            ),
            notification(
                3,
                "newer warning",
                Severity::Warning,
                now - std::time::Duration::from_secs(1),
            ),
            notification(
                4,
                "error",
                Severity::Error,
                now - std::time::Duration::from_secs(3),
            ),
        ];

        let messages: Vec<_> = ordered_notifications(&notifications)
            .into_iter()
            .map(|notification| notification.message.to_string())
            .collect();
        assert_eq!(
            messages,
            ["error", "newer warning", "older warning", "z info"]
        );
    }

    #[test]
    fn notification_order_is_stable_by_name_and_id() {
        let at = std::time::Instant::now();
        let notifications = vec![
            notification(8, "same", Severity::Warning, at),
            notification(2, "same", Severity::Warning, at),
            notification(4, "a", Severity::Warning, at),
        ];

        let ids: Vec<_> = ordered_notifications(&notifications)
            .into_iter()
            .map(|notification| notification.id)
            .collect();
        assert_eq!(ids, [4, 2, 8]);
    }

    #[test]
    fn notification_overflow_is_explicit() {
        assert_eq!(notification_overflow_count(notification_capacity()), 0);
        assert_eq!(notification_overflow_count(notification_capacity() + 1), 1);
        // One number, because the old "total · active" pair repeated the same figure.
        assert_eq!(notification_count_label(3), "3 notifications");
        assert_eq!(notification_count_label(1), "1 notification");
        assert_eq!(notification_status_label(0, 0), "");
        assert_eq!(
            notification_status_label(1, 0),
            "1 duplicate entry collapsed"
        );
        assert_eq!(
            notification_status_label(2, 3),
            "2 duplicate entries collapsed · 3 more notifications · scroll to view"
        );

        let mut detailed_info =
            notification(9, "detailed", Severity::Info, std::time::Instant::now());
        detailed_info.detail = Some("context".to_owned());
        assert!(notification_is_active_incident(&detailed_info));
    }

    /// The same message at the same severity is one problem, so the list shows it once and says
    /// how many repeats it folded away.
    #[test]
    fn repeated_notifications_collapse_into_one_row() {
        let now = std::time::Instant::now();
        let notifications = vec![
            notification(1, "apply or revert", Severity::Warning, now),
            notification(
                2,
                "apply or revert",
                Severity::Warning,
                now - std::time::Duration::from_secs(300),
            ),
            notification(3, "apply or revert", Severity::Info, now),
            notification(4, "port forward failed", Severity::Error, now),
        ];

        let (rows, collapsed) = collapsed_notifications(&notifications);
        let messages: Vec<_> = rows
            .iter()
            .map(|notification| notification.message.to_string())
            .collect();
        let severities: Vec<_> = rows.iter().map(|row| row.severity).collect();

        assert_eq!(
            messages,
            ["port forward failed", "apply or revert", "apply or revert"]
        );
        assert_eq!(
            severities,
            [Severity::Error, Severity::Warning, Severity::Info]
        );
        assert_eq!(rows[1].id, 1, "the newest entry of a group is the one kept");
        assert_eq!(collapsed, 1);
    }

    /// A warning that guards unsaved work is not the same shape of news as an update notice.
    ///
    /// The contract is now that severity has exactly one shape vocabulary for the whole app, so a
    /// notification row reads the same as a table row or a status-bar item. This file used to own
    /// a private severity-to-glyph map, which could drift from the shared one and quietly invent a
    /// fourth channel.
    #[test]
    fn notification_severities_use_the_shared_health_vocabulary() {
        assert_eq!(
            [
                design::health_icon(Severity::Success),
                design::health_icon(Severity::Warning),
                design::health_icon(Severity::Error),
                design::health_icon(Severity::Info),
            ],
            [
                IconName::Check,
                IconName::TriangleAlert,
                IconName::CircleX,
                IconName::Circle,
            ]
        );
        assert_eq!(design::health_icon(Severity::Neutral), IconName::Dash);
        assert_eq!(design::health_icon(Severity::Muted), IconName::Dash);
        // A caller that has not been migrated must still land on the same vocabulary.
        for severity in [
            Severity::Success,
            Severity::Warning,
            Severity::Error,
            Severity::Info,
            Severity::Neutral,
            Severity::Muted,
        ] {
            assert_eq!(
                design::severity_icon(severity),
                design::health_icon(severity)
            );
        }
    }

    /// The two channels answer different questions, so a state the app could not read never
    /// reports as a current answer.
    #[test]
    fn observation_confidence_separates_health_from_observability() {
        assert_eq!(
            observation_confidence(&ConnectionState::Live, None),
            design::Confidence::Known
        );
        assert_eq!(
            observation_confidence(&ConnectionState::Live, Some(false)),
            design::Confidence::Known
        );
        assert_eq!(
            observation_confidence(&ConnectionState::Live, Some(true)),
            design::Confidence::Stale
        );
        assert_eq!(
            observation_confidence(
                &ConnectionState::Reconnecting("probe failed".to_owned()),
                None
            ),
            design::Confidence::Stale
        );
        assert_eq!(
            observation_confidence(&ConnectionState::Connecting, None),
            design::Confidence::Unknown
        );
        assert_eq!(
            observation_confidence(&ConnectionState::Failed("no context".to_owned()), None),
            design::Confidence::Unknown
        );
    }

    /// A current answer draws no confidence mark, and a mark that is drawn comes from the shared
    /// confidence vocabulary rather than a glyph the bar invented, so the health and confidence
    /// channels can never collapse into one shape here.
    #[test]
    fn confidence_marker_is_absent_when_known_and_comes_from_the_shared_vocabulary() {
        assert_eq!(confidence_marker(design::Confidence::Known), None);
        for state in [design::Confidence::Stale, design::Confidence::Unknown] {
            assert_eq!(
                confidence_marker(state),
                Some(design::confidence::icon(state))
            );
            assert!(!design::confidence_label(state).is_empty());
        }
    }
}
