//! Status bar and notification center.
//!
//! The bar shows nearby status and activity counts. The notification center
//! keeps detailed failures expandable and keeps success and information in history.

use std::cmp::Ordering;

use gpui::{
    AnyElement, ClickEvent, Context, FocusHandle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Pixels, Role, StatefulInteractiveElement, Styled,
    Window, div, px,
};
use ui::prelude::*;
use ui::{ElevationIndex, IconButton, ListItem, ListItemSpacing, TintColor, Tooltip};

use super::{ConnectionState, Notification, Shell, StatusPanel};
use crate::design::{self, Severity, space};
use crate::panels::common::label_panel_title;
use crate::panels::forwards::phase_presentation;
use crate::panels::{ForwardId, ForwardPhase, ForwardSnapshot, ForwardSummary};

/// Root tab group of the status bar, after the Dock.
const STATUS_BAR_TAB_GROUP: isize = 8;
/// Root tab group of the notification center, after the status bar.
const NOTIFICATION_TAB_GROUP: isize = 9;
/// Root tab group of the port forward panel, after the notification center.
const PORT_FORWARD_TAB_GROUP: isize = 10;
const NOTIFICATION_WIDTH: f32 = 360.0;
const NOTIFICATION_MAX_HEIGHT: f32 = 360.0;
/// Clearance between the popovers and the top of the status bar. A floating surface must not
/// read as attached to the bar that owns it.
const POPOVER_CLEARANCE: f32 = 16.0;
/// Width of the relative-time column. It holds the longest age label the bar can print
/// (`23 hours`), so every timestamp starts at the same x and never touches the message.
const NOTIFICATION_AGE_COLUMN: f32 = 56.0;

/// Width of the notification and port forward popovers.
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

/// The status bar label. It names the subject before the count, so it has the same shape as the
/// three metrics beside it instead of a fourth grammar of its own, and the problem buckets follow
/// it because `0 active · 3 failed` is the case a zero would otherwise hide.
fn forward_summary_short_label(summary: ForwardSummary) -> String {
    let mut label = format!(
        "Port forwards · {} active",
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

/// The popover header's trailing count.
///
/// The header already says `Port Forwards` in its title, so this one carries only the numbers. It
/// exists separately from [`forward_summary_short_label`] because repeating the subject 100px from
/// the title would be the redundancy `layout.md > Visual hierarchy` warns about, and the header is
/// a fixed height that truncates rather than grows.
fn forward_summary_header_count(summary: ForwardSummary) -> String {
    if summary.failed > 0 {
        format!(
            "Active {} · {} failed",
            design::format::count(summary.active),
            design::format::count(summary.failed)
        )
    } else if summary.pending > 0 {
        format!(
            "Active {} · {} pending",
            design::format::count(summary.active),
            design::format::count(summary.pending)
        )
    } else {
        format!("Active {}", design::format::count(summary.active))
    }
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

/// The bar's leading item text.
///
/// It reports the connection and nothing else. `ConnectionState`'s only input is a single
/// `GET /readyz` against kube-apiserver, so the health word it used to print here (`Healthy`) rested
/// on nothing but "the app could still reach the API server". `DESIGN.md` §4 is blunt about it —
/// **读不到 ≠ 健康** — and a one-shot probe can never turn amber because a workload broke, so the
/// word was guaranteed to be reassuring at the exact moment it should not have been. Cluster health
/// is the Overview banner's verdict (`k8s_core::overview::Overview::level`), and the top bar already
/// prints this same state as `Live`.
fn connection_text(connection: &ConnectionState) -> String {
    format!("Connection · {}", connection.label())
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
/// rather than this file's, and the scope sentence rides along in the accessible name.
fn status_metric(
    id: &'static str,
    icon: Icon,
    count: usize,
    singular: &'static str,
    plural: &'static str,
    tooltip: String,
) -> AnyElement {
    let label = design::format::count_with_noun(count, singular, plural);
    let mut metric = h_flex()
        .id(id)
        .h(px(super::status_bar_height()))
        .flex_none()
        .gap(space::XS)
        .items_center()
        .role(Role::Status)
        .aria_label(metric_announcement(&label, &tooltip))
        .child(icon)
        .child(
            Label::new(label)
                .size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .color(Color::Muted),
        );
    metric.interactivity().tooltip(Tooltip::text(tooltip));
    metric.into_any_element()
}

/// A state that has no count: the icon carries the shape, the label carries the words, and the
/// colour only reinforces them.
fn log_status_metric(
    id: &'static str,
    icon: Icon,
    label: &'static str,
    tooltip: String,
) -> AnyElement {
    let text = log_status_text(label);
    let mut metric = h_flex()
        .id(id)
        // A selector so a test can prove the state is on screen while the Dock is collapsed.
        .debug_selector(|| id.to_owned())
        .h(px(super::status_bar_height()))
        .flex_none()
        .gap(space::XS)
        .items_center()
        .role(Role::Status)
        .aria_label(metric_announcement(&text, &tooltip))
        .child(icon)
        .child(
            Label::new(text)
                .size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .color(Color::Muted),
        );
    metric.interactivity().tooltip(Tooltip::text(tooltip));
    metric.into_any_element()
}

impl Shell {
    /// The bar's leading pair: whether the app can reach the API server on one channel, and whether
    /// that answer can be vouched for on the other.
    ///
    /// The strip stays quiet — `METADATA` at the bar's muted color, no chip, no badge — because
    /// this is a readout, not a dashboard. All the boldness goes to the glyph, and the confidence
    /// mark sits on the item's trailing edge from a hollow shape family, so a reader who cannot
    /// separate the hues still sees two different questions.
    ///
    /// The glyph still comes from `design::health_icon`, because reachability *is* a status, but
    /// the words are the connection's own. Printing `Cluster · Healthy` here put an observation
    /// fact in the health channel and then added a confidence marker beside it, saying the same
    /// thing twice with the first one in the wrong place: the Overview banner could show
    /// `Cluster needs attention` in the same frame this read `Healthy`, and a green tick plus a
    /// health word is a promise no readiness probe can keep.
    fn render_cluster_health(&self, cx: &Context<Self>) -> AnyElement {
        let severity = self.connection.severity();
        let confidence = observation_confidence(&self.connection, self.catalog_stale);
        let marker = confidence_marker(confidence);
        let label = connection_text(&self.connection);
        // The confidence word only reaches a pointer or a screen reader here; the shape carries it
        // for everyone else, which is why the marker is not a color change.
        let description = design::confidence_label(confidence);
        let mut item = h_flex()
            .id("status-bar-cluster-health")
            .debug_selector(|| "status-bar-cluster-health".to_owned())
            .h(px(super::status_bar_height()))
            .flex_none()
            .gap(space::XS)
            .items_center()
            .role(Role::Status)
            .aria_label(format!("{label}. {description}"))
            .child(
                Icon::new(design::health_icon(severity))
                    .size(IconSize::XSmall)
                    .color(Color::Custom(severity.marker(cx))),
            )
            .child(
                Label::new(label.clone())
                    .size(LabelSize::Custom(rems_from_px(f32::from(
                        design::text::METADATA,
                    ))))
                    .color(Color::Muted),
            )
            .when_some(marker, |this, marker| {
                this.child(
                    Icon::new(marker)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(design::confidence::foreground(
                            confidence, cx,
                        ))),
                )
            });
        item.interactivity()
            .tooltip(Tooltip::text(format!("{label}. {description}")));
        item.into_any_element()
    }

    pub(super) fn render_status_bar(&self, _window: &Window, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let summary = self.status_summary(cx);
        let port_forwards_open = self.status_panel == StatusPanel::PortForwards;
        let forward_summary = summary.port_forwards;
        let forward_action = if port_forwards_open {
            "Close Port Forwards"
        } else {
            "Open Port Forwards"
        };
        let forward_label = format!(
            "{forward_action}: {}",
            forward_summary_label(forward_summary)
        );
        // The operations count is only meaningful on top of a resource view, and it reads 0 when
        // there is none, so the scope sentence has to say which of the two is on screen.
        let operations_tooltip = operations_scope(self.active_resource_view().is_some()).to_owned();
        h_flex()
            .id("status-bar")
            .debug_selector(|| "status-bar".to_owned())
            .flex_none()
            .w_full()
            .h(px(super::status_bar_height()))
            .min_w(px(0.))
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .tab_group()
            .tab_index(STATUS_BAR_TAB_GROUP)
            // The readouts are `Role::Status`, which is not a tab stop, so Tab walked past every
            // number in the bar and stopped on the one button: a keyboard user arrived and found
            // unfocusable text on both sides of it. One handle for the group is enough, because
            // the metrics are read rather than operated, and the port-forward button keeps its own
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
            .bg(colors.panel_background.alpha(1.0))
            .border_t_1()
            .border_color(colors.border)
            .text_color(colors.text_muted)
            .child(div().flex_1().min_w(space::SM))
            // Reading order runs along the bar, so the connection and the confidence of that
            // reading come before the activity counts. It is the item that stays on screen while
            // the table is scrolled, which makes it the bar's reason to exist.
            .child(self.render_cluster_health(cx))
            // Running work, open sessions, notifications, and port forwards are four different
            // questions, so each one keeps its own label and a zero stays visible instead of
            // disappearing.
            .child(status_metric(
                "status-bar-operations",
                Icon::new(design::health_icon(operations_severity(summary.operations)))
                    .size(IconSize::XSmall),
                summary.operations,
                "operation",
                "operations",
                operations_tooltip,
            ))
            .child(status_metric(
                "status-bar-sessions",
                Icon::new(IconName::Terminal).size(IconSize::XSmall),
                summary.sessions,
                "session",
                "sessions",
                "Terminal sessions open in the Dock".to_owned(),
            ))
            // The notification count belongs here as well as in the top bar. The popover is anchored
            // to this bar, so with nothing here to belong to it read as a panel that belonged to
            // the top bar's bell 928px away, and `DESIGN.md` §9 already promised this item.
            .child(status_metric(
                "status-bar-notifications",
                Icon::new(IconName::Bell).size(IconSize::XSmall),
                summary.notifications,
                "notification",
                "notifications",
                "Notifications in the centre. Active incidents are listed first.".to_owned(),
            ))
            .child({
                let (icon, severity) = forward_summary_presentation(forward_summary);
                // `Active 0` is the least important thing in the bar and it used to be the
                // loudest: the only button, the only Tab stop, default label size next to three
                // `METADATA` readouts, and a full-strength ink. The size drops to the metrics'
                // size and the nothing-to-report state borrows their muted ink, so a real failure
                // still gets its colour.
                let icon_color = if severity == Severity::Muted {
                    Color::Muted
                } else {
                    Color::Custom(severity.marker(cx))
                };
                let mut button = Button::new(
                    "status-bar-port-forwards",
                    forward_summary_short_label(forward_summary),
                )
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Medium)
                .label_size(LabelSize::Custom(rems_from_px(f32::from(
                    design::text::METADATA,
                ))))
                .tab_index(0isize)
                .track_focus(&self.status_bar_port_forward_focus)
                .start_icon(Icon::new(icon).size(IconSize::XSmall).color(icon_color))
                .aria_label(forward_label.clone())
                .aria_expanded(port_forwards_open)
                .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
                    if shell.status_panel == StatusPanel::PortForwards {
                        shell.close_port_forward_panel(window, cx);
                    } else {
                        shell.open_port_forward_panel(window, cx);
                    }
                }));
                if !port_forwards_open {
                    button = button.tooltip(Tooltip::text(forward_label));
                }
                button
            })
            // The bell that opens the notification centre stays in the top bar's right-hand group,
            // where `windows.md` wants it: a window can be moved so that its bottom edge is
            // hidden, and a bottom bar is exactly that edge. The bar carries the count and the
            // popover's own anchor; the top bar keeps the control.
            .when_some(summary.log_status, |this, log_status| {
                let (icon, severity) = log_status_presentation(log_status);
                this.child(log_status_metric(
                    "status-bar-log-stream",
                    Icon::new(icon)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(severity.marker(cx))),
                    log_status,
                    format!("Log stream in the Dock: {log_status}"),
                ))
            })
            .into_any_element()
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
            .bg(design::surface::backdrop(cx))
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

    pub(super) fn sync_port_forward_focus_handles(&mut self, cx: &mut Context<Self>) {
        let count = self.dock_panel.read(cx).forward_snapshots().len();
        while self.port_forward_action_focus_handles.len() < count {
            self.port_forward_action_focus_handles
                .push(cx.focus_handle().tab_stop(true).tab_index(1isize));
        }
        self.port_forward_action_focus_handles.truncate(count);
    }

    pub(super) fn focus_port_forward_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reverse: bool,
    ) {
        self.sync_port_forward_focus_handles(cx);
        let snapshots = self.dock_panel.read(cx).forward_snapshots();
        let mut controls = vec![self.port_forward_focus.clone()];
        let mut action_rows = Vec::new();
        for (index, snapshot) in snapshots.iter().enumerate() {
            if snapshot.phase == ForwardPhase::Stopping {
                continue;
            }
            if let Some(focus) = self.port_forward_action_focus_handles.get(index) {
                action_rows.push(index);
                controls.push(focus.clone());
            }
        }
        let action_count = controls.len() - 1;
        controls.push(self.port_forward_open_focus.clone());
        controls.push(self.port_forward_new_focus.clone());
        let current = window.focused(cx);
        let index = current
            .and_then(|handle| controls.iter().position(|control| *control == handle))
            .unwrap_or(0);
        let next = if reverse {
            (index + controls.len() - 1) % controls.len()
        } else {
            (index + 1) % controls.len()
        };
        if next > 0 && next <= action_count {
            self.port_forward_scroll
                .scroll_to_item(action_rows[next - 1]);
        }
        window.focus(&controls[next], cx);
    }

    fn run_port_forward_action(
        &mut self,
        id: ForwardId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let phase = self
            .dock_panel
            .read(cx)
            .forward_snapshots()
            .into_iter()
            .find(|snapshot| snapshot.id == id)
            .map(|snapshot| snapshot.phase);
        let result = self.dock_panel.update(cx, |dock, cx| match phase {
            Some(ForwardPhase::Stopped) => dock.restart_forward(id, cx),
            Some(ForwardPhase::Failed) => dock.retry_forward(id, cx),
            Some(ForwardPhase::Starting | ForwardPhase::Running) => {
                dock.stop_forward(id, cx);
                Ok(())
            }
            Some(ForwardPhase::Stopping) | None => Ok(()),
        });
        if result.is_ok() && matches!(phase, Some(ForwardPhase::Starting | ForwardPhase::Running)) {
            window.focus(&self.port_forward_focus, cx);
        }
        cx.notify();
    }

    pub(super) fn activate_port_forward_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = window.focused(cx) else {
            return;
        };
        if current == self.port_forward_open_focus {
            self.close_port_forward_panel(window, cx);
            self.command_open_forwards(window, cx);
            return;
        }
        if current == self.port_forward_new_focus {
            self.close_port_forward_panel(window, cx);
            self.request_new_port_forward(window, cx);
            return;
        }
        self.sync_port_forward_focus_handles(cx);
        let Some(index) = self
            .port_forward_action_focus_handles
            .iter()
            .position(|handle| *handle == current)
        else {
            return;
        };
        let Some(id) = self
            .dock_panel
            .read(cx)
            .forward_snapshots()
            .get(index)
            .map(|snapshot| snapshot.id)
        else {
            return;
        };
        self.run_port_forward_action(id, window, cx);
    }

    fn render_port_forward_action(
        &self,
        index: usize,
        snapshot: &ForwardSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = snapshot.id;
        let target = format!("{}:{}", snapshot.label, snapshot.remote_port);
        let focus = self
            .port_forward_action_focus_handles
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.port_forward_focus.clone());
        match snapshot.phase {
            ForwardPhase::Stopped => Button::new(("status-port-forward-start", index), "Start")
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .size(ButtonSize::Medium)
                .tab_index(1isize)
                .track_focus(&focus)
                .tooltip(Tooltip::text("Start this port forward again"))
                .aria_label(format!("Start port forward for {target}"))
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.run_port_forward_action(id, window, cx)
                }))
                .into_any_element(),
            ForwardPhase::Starting | ForwardPhase::Running => {
                IconButton::new(("status-port-forward-stop", index), IconName::Stop)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .tab_index(1isize)
                    .track_focus(&focus)
                    .tooltip(Tooltip::text("Stop this port forward"))
                    .aria_label(format!("Stop port forward for {target}"))
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        shell.run_port_forward_action(id, window, cx)
                    }))
                    .into_any_element()
            }
            ForwardPhase::Stopping => {
                IconButton::new(("status-port-forward-waiting", index), IconName::Stop)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .disabled(true)
                    .aria_label(format!("Stopping port forward for {target}"))
                    .into_any_element()
            }
            ForwardPhase::Failed => Button::new(("status-port-forward-retry", index), "Retry")
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .size(ButtonSize::Medium)
                .tab_index(1isize)
                .track_focus(&focus)
                .tooltip(Tooltip::text("Retry this port forward"))
                .aria_label(format!("Retry port forward for {target}"))
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.run_port_forward_action(id, window, cx)
                }))
                .into_any_element(),
        }
    }

    fn render_port_forward_row(
        &self,
        index: usize,
        total: usize,
        snapshot: &ForwardSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The bar and the Port Forwards panel print the same phase, so they take the same shape
        // and the same words. This file used to keep a second phase-to-glyph map, which is the
        // private vocabulary `DESIGN.md` §4 forbids; the two in-progress phases shared a static
        // `LoadCircle` here and a different pair of glyphs there, for the same state.
        let (icon, status, severity, next_step) = phase_presentation(snapshot.phase);
        let target = match snapshot.local_port {
            Some(local_port) => format!(
                "localhost:{local_port} → {}:{}",
                snapshot.label, snapshot.remote_port
            ),
            None => format!("{}:{}", snapshot.label, snapshot.remote_port),
        };
        let accessible_target = target.replace(" → ", " to ");
        let mut content = h_flex()
            .flex_1()
            .min_w(px(0.))
            .gap(space::SM)
            .items_center()
            .child(Label::new(target).truncate())
            // The phase is one of six fixed words and the row has a fixed height, so it holds its
            // ground. The target above it is the part that gives way, and it carries the full
            // string in a tooltip.
            .child(
                div().flex_none().child(
                    Label::new(status)
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
                ),
            );
        content
            .interactivity()
            .tooltip(Tooltip::text(snapshot.error.clone().unwrap_or_else(|| {
                SharedString::from(format!("{accessible_target}. {status} {next_step}"))
            })));
        // ListItem cannot report the set position, so the row is wrapped in the
        // accessible list item itself.
        div()
            .id(("status-port-forward-item", index))
            .w_full()
            .role(Role::ListItem)
            .accessibility_id(format!("port-forward-{index}"))
            .aria_label(format!(
                "{accessible_target}. Port forward {status} {next_step}"
            ))
            .aria_position_in_set(index + 1)
            .aria_size_of_set(total)
            .child(
                ListItem::new(("status-port-forward-row", index))
                    .height(design::size::ROW)
                    .spacing(ListItemSpacing::ExtraDense)
                    .selectable(false)
                    .start_slot(
                        Icon::new(icon)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(severity.marker(cx))),
                    )
                    .child(content)
                    .end_slot(self.render_port_forward_action(index, snapshot, cx)),
            )
            .into_any_element()
    }

    pub(super) fn render_port_forward_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.sync_port_forward_focus_handles(cx);
        let snapshots = self.dock_panel.read(cx).forward_snapshots();
        let summary = self.dock_panel.read(cx).forward_summary();
        let body = if snapshots.is_empty() {
            h_flex()
                .id("status-port-forward-empty")
                .h(design::size::ROW)
                .px(space::MD)
                .items_center()
                .justify_center()
                .role(Role::Status)
                .aria_label("No port forwards")
                .child(
                    Label::new("No port forwards")
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .id("status-port-forward-list")
                .role(Role::List)
                .aria_label(forward_summary_label(summary))
                .children(snapshots.iter().enumerate().map(|(index, snapshot)| {
                    self.render_port_forward_row(index, snapshots.len(), snapshot, cx)
                }))
                .into_any_element()
        };
        let colors = cx.theme().colors();
        let raised = design::surface::raised(cx);
        div()
            .id("port-forward-panel")
            .debug_selector(|| "port-forward-panel".to_owned())
            .track_focus(&self.port_forward_focus)
            .tab_group()
            .tab_index(PORT_FORWARD_TAB_GROUP)
            .key_context("PortForwardPanel")
            .role(Role::Dialog)
            .accessibility_id("port-forward-panel")
            .aria_label("Port forwards")
            .aria_keyshortcuts("Tab Shift+Tab Escape")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, _, window, cx| {
                    window.focus(&shell.port_forward_focus, cx);
                    cx.stop_propagation();
                }),
            )
            .absolute()
            .bottom(px(super::status_bar_height() + POPOVER_CLEARANCE))
            .right(space::SM)
            .w(px(notification_width(f32::from(
                window.viewport_size().width,
            ))))
            .max_h(px(NOTIFICATION_MAX_HEIGHT))
            .rounded_lg()
            .border_1()
            .border_color(popover_border(raised, colors.border))
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .h(design::size::TOOLBAR)
                    .px(space::SM)
                    .gap(space::SM)
                    .items_center()
                    .border_b_1()
                    .border_color(popover_rule(raised, colors.border_variant))
                    .child(div().flex_none().child(label_panel_title("Port Forwards")))
                    .child(div().flex_1())
                    .child({
                        // The header row is a fixed height, so the count has to shorten instead of
                        // wrapping into a line the popover then cuts. The title is the fixed part
                        // and holds its ground; the count is data, and the tooltip keeps the full
                        // breakdown that the short label leaves out.
                        let mut count = h_flex().min_w(px(0.)).justify_end().child(
                            Label::new(forward_summary_header_count(summary))
                                .truncate()
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        );
                        count
                            .interactivity()
                            .tooltip(Tooltip::text(forward_summary_label(summary)));
                        count
                    }),
            )
            .child(
                div()
                    .id("status-port-forward-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.port_forward_scroll)
                    .child(body),
            )
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    // These two labels name what happens, so the row grows when the popover is too
                    // narrow to hold them side by side. A fixed height would let the popover's own
                    // `overflow_hidden` cut the primary action off at the panel edge instead.
                    .min_h(design::size::TOOLBAR)
                    .flex_wrap()
                    .px(space::SM)
                    .gap(space::SM)
                    .items_center()
                    .border_t_1()
                    .border_color(popover_rule(raised, colors.border_variant))
                    .child(
                        Button::new("status-port-forward-open", "Open Port Forwards")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Medium)
                            .label_size(LabelSize::Custom(rems_from_px(f32::from(
                                design::text::BODY,
                            ))))
                            .tab_index(2isize)
                            .track_focus(&self.port_forward_open_focus)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.close_port_forward_panel(window, cx);
                                shell.command_open_forwards(window, cx);
                            })),
                    )
                    .child(div().flex_1().min_w(px(0.)))
                    .child(
                        // `buttons.md` › Push buttons: a button that opens another view keeps the
                        // trailing ellipsis, so the label stays "New Port Forward…" and the width
                        // gives way instead of the text.
                        Button::new("status-port-forward-new", "New Port Forward…")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .size(ButtonSize::Medium)
                            .label_size(LabelSize::Custom(rems_from_px(f32::from(
                                design::text::BODY,
                            ))))
                            .tab_index(2isize)
                            .track_focus(&self.port_forward_new_focus)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.close_port_forward_panel(window, cx);
                                shell.request_new_port_forward(window, cx);
                            })),
                    ),
            )
            // Painted last so the rail is never hidden behind a row's own background.
            .child(popover_focus_rail(
                &self.port_forward_focus,
                colors.border_focused,
            ))
            .into_any_element()
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
                        .size(LabelSize::Custom(rems_from_px(f32::from(
                            design::text::METADATA,
                        ))))
                        .color(Color::Muted),
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
        let colors = cx.theme().colors();
        let raised = design::surface::raised(cx);
        div()
            .id("notification-center")
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
            .right(space::SM)
            .w(px(notification_width(f32::from(
                window.viewport_size().width,
            ))))
            .max_h(px(NOTIFICATION_MAX_HEIGHT))
            .rounded_lg()
            .border_1()
            .border_color(popover_border(raised, colors.border))
            .bg(colors.elevated_surface_background.alpha(1.0))
            .shadow(ElevationIndex::ModalSurface.shadow(cx))
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
                    .child(
                        Label::new("Notifications").size(LabelSize::Custom(rems_from_px(
                            f32::from(design::text::SECTION),
                        ))),
                    )
                    .when(count > 0, |this| {
                        this.child(
                            Label::new(notification_count_label(count))
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .when(count > 0, |this| {
                        this.child(
                            Button::new("notifications-clear", "Clear All")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Medium)
                                .tab_index(super::NOTIFICATION_CLEAR_TAB_INDEX)
                                .track_focus(&self.notification_clear_focus)
                                .aria_label("Clear All Notifications")
                                .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
                                    shell.notifications.clear();
                                    shell.close_notifications(window, cx);
                                })),
                        )
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
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(status_label)
                                .size(LabelSize::Custom(rems_from_px(f32::from(
                                    design::text::METADATA,
                                ))))
                                .color(Color::Muted),
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
        let colors = cx.theme().colors();
        let raised = design::surface::raised(cx);
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
                this.cursor_pointer()
                    .hover(|this| this.bg(colors.element_hover))
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
                    .size(IconSize::XSmall)
                    .color(Color::Custom(notification.severity.marker(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.0))
                    .child(
                        Label::new(notification.message.clone()).size(LabelSize::Custom(
                            rems_from_px(f32::from(design::text::METADATA)),
                        )),
                    )
                    .when(expanded, |this| {
                        this.when_some(notification.detail.clone(), |this, detail| {
                            this.child(
                                Label::new(detail)
                                    .size(LabelSize::Custom(rems_from_px(f32::from(
                                        design::text::METADATA,
                                    ))))
                                    .color(Color::Muted),
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
                            .size(LabelSize::Custom(rems_from_px(f32::from(
                                design::text::METADATA,
                            ))))
                            .color(Color::Muted),
                    ),
            )
            .when(has_detail, |this| {
                this.child(
                    Icon::new(if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
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
    use gpui::Hsla;

    use super::*;

    fn forward_summary(active: usize, failed: usize, pending: usize) -> ForwardSummary {
        ForwardSummary {
            active,
            failed,
            pending,
            stopped: 0,
        }
    }

    /// A `#rrggbb` / `#rrggbbaa` string from the theme file, as a colour.
    fn k8s_hex(raw: &str) -> Hsla {
        let digits = raw.strip_prefix('#').expect("hex color");
        gpui::rgba(u32::from_str_radix(&digits[..6], 16).expect("hex color") << 8 | 0xff).into()
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
        assert_eq!(forward_summary_header_count(summary), "Active 2 · 1 failed");
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
        assert_eq!(
            forward_summary_short_label(forward_summary(0, 0, 0)),
            "Port forwards · 0 active"
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
        assert_eq!(
            forward_summary_header_count(forward_summary(0, 0, 0)),
            "Active 0"
        );
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
        assert_eq!(
            design::health_label(ConnectionState::Live.severity()),
            "Healthy"
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
            (IconName::XCircleFilled, Severity::Error)
        );
        assert_eq!(
            log_status_presentation("Paused"),
            (IconName::Dash, Severity::Muted)
        );
        // Nothing in the bar may claim a spinner is turning when the count is zero.
        assert_ne!(
            design::health_icon(operations_severity(0)),
            IconName::LoadCircle,
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
                let ratio = ui::utils::calculate_contrast_ratio(border, raised.alpha(1.0));
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
                let ratio = ui::utils::calculate_contrast_ratio(rule, raised.alpha(1.0));
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
            + f32::from(design::text::METADATA_LINE_HEIGHT);
        // 32px header + 16px padding + 14px line = 62px, against the 84px it measured before.
        assert_eq!(height, 62.0);
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
                IconName::Warning,
                IconName::XCircleFilled,
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
